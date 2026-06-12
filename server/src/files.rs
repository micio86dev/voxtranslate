//! Chat file upload + processing pipeline (spec 0018).
//!
//! `POST /api/rooms/{room}/files` accepts a multipart body (`peer_id` + `file`),
//! stores the bytes in Supabase Storage, extracts text by type (audio → Deepgram
//! STT, txt → UTF-8, pdf → text extraction), runs the **existing** translation
//! fan-out, and broadcasts a `ChatMessage` carrying the file `attachment` plus
//! the translated text — so the whole chat pipeline (render, transcript
//! persistence, unread tracking) is reused unchanged.
//!
//! Authorization is room membership: the `peer_id` must be a live member of the
//! room (the same trust model as typed WebSocket chat), so the feature works for
//! guests too. No JWT is required.

use axum::extract::{Multipart, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use uuid::Uuid;

use crate::protocol::{Attachment, ServerMessage};
use crate::transcripts::{EventKind, TranscriptEvent};
use crate::{db, deepgram, now_unix, storage, AppState};

/// Hard ceiling on the multipart body (route-level). Slightly above the default
/// 25 MiB file cap to leave room for multipart boundaries/headers; the handler
/// enforces the exact configured `storage.max_bytes` on the file itself.
pub const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Cap on extracted text fed to the translation fan-out, to bound latency/cost
/// for very long documents. Truncation happens on a char boundary.
const MAX_TEXT_CHARS: usize = 4000;

/// What kind of processing an upload gets, derived from its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// `.mp3` / `.wav` → Deepgram prerecorded transcription.
    Audio,
    /// `.txt` → decoded as UTF-8.
    Text,
    /// `.pdf` → text-layer extraction (no OCR).
    Pdf,
}

/// `GET /api/files/config` — whether chat file upload is available (Supabase
/// Storage configured). Public + cheap; the client hides the attach button when
/// `enabled` is false (spec 0018 R6). Independent of auth/billing.
pub async fn files_config(State(state): State<AppState>) -> Response {
    Json(serde_json::json!({ "enabled": state.storage.is_some() })).into_response()
}

/// `POST /api/rooms/{room}/files` — upload + process a single chat file.
pub async fn upload_file(
    State(state): State<AppState>,
    Path(room): Path<String>,
    multipart: Multipart,
) -> Response {
    // The feature is gated on Supabase Storage being configured.
    let Some(storage_client) = state.storage.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "file upload is not configured",
        )
            .into_response();
    };
    let max_bytes = state
        .config
        .storage
        .as_ref()
        .map(|c| c.max_bytes)
        .unwrap_or(MAX_BODY_BYTES);

    let room = match clean_room(&room) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    // ---- Parse the multipart body (peer_id + file) -------------------------
    let parsed = match parse_upload(multipart).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let Upload {
        peer_id,
        file_name,
        content_type,
        bytes,
    } = parsed;

    // ---- Authorize: the uploader must be a live member of the room ---------
    let Some(snapshot) = state.rooms.peer_snapshot(&room, &peer_id) else {
        return (StatusCode::FORBIDDEN, "not a member of this room").into_response();
    };

    // ---- Validate the file (non-empty, within size, supported type) --------
    if bytes.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty file").into_response();
    }
    if bytes.len() > max_bytes {
        return (StatusCode::PAYLOAD_TOO_LARGE, "file too large").into_response();
    }
    let ext = ext_of(&file_name);
    let Some(kind) = classify_ext(&ext) else {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported file type (allowed: mp3, wav, txt, pdf)",
        )
            .into_response();
    };
    // Normalize a content type for storage + Deepgram (browsers omit it sometimes).
    let content_type = normalize_content_type(content_type.as_deref(), kind);

    let size = bytes.len() as u64;

    // The room's call-session id ties the file to the call lifetime.
    let session_id = state.rooms.session_id(&room).unwrap_or_else(Uuid::nil);

    // ---- Upload to Supabase Storage (must succeed) -------------------------
    let object = storage::object_path(&session_id.to_string(), &Uuid::new_v4().to_string(), &ext);
    let file_url = match storage_client
        .upload(&object, bytes.clone(), content_type)
        .await
    {
        Ok(url) => url,
        Err(e) => {
            tracing::error!("chat file upload to storage failed: {e}");
            return (StatusCode::BAD_GATEWAY, "storage upload failed").into_response();
        }
    };

    // ---- Persist metadata (best-effort; only when the DB is configured) ----
    if let Some(pool) = state.pool.as_ref() {
        if let Err(e) = db::insert_chat_file(
            pool,
            session_id,
            &room,
            &peer_id,
            &snapshot.name,
            &file_url,
            &file_name,
            content_type,
            size as i64,
        )
        .await
        {
            // Non-fatal: the file is stored and will still post to chat.
            tracing::error!("chat_files insert failed: {e}");
        }
    }

    // ---- Extract text by type (best-effort; failure → empty body) ----------
    let (text, source_lang) = extract_text(&state, kind, bytes, content_type, &snapshot.lang).await;
    let text = truncate_chars(text.trim(), MAX_TEXT_CHARS);

    // ---- Translate (reusing the chat fan-out) + persist transcript ---------
    let targets = state.rooms.get_room_languages(&room, &peer_id);
    let glossary = state.glossary.as_ref().and_then(|g| g.cached(&room));
    let translations = state
        .translator
        .translate_fanout(&text, &source_lang, &targets, glossary.as_deref())
        .await;

    if !text.is_empty() {
        if let Some(svc) = state.transcripts.as_ref() {
            svc.record(TranscriptEvent {
                session_id,
                kind: EventKind::Chat,
                speaker_peer_id: peer_id.clone(),
                speaker_user_id: None,
                speaker_name: snapshot.name.clone(),
                original_text: text.clone(),
                original_lang: source_lang.clone(),
                translations: translations.clone(),
                ts: chrono::Utc::now(),
            });
        }
    }

    // ---- Broadcast the chat message with the attachment --------------------
    state.rooms.broadcast(
        &room,
        &ServerMessage::ChatMessage {
            sender_id: peer_id,
            sender_name: snapshot.name,
            sender_lang: source_lang,
            sender_avatar: snapshot.avatar_url,
            original: text,
            translations,
            timestamp: now_unix(),
            attachment: Some(Attachment {
                url: file_url.clone(),
                name: file_name.clone(),
                content_type: content_type.to_string(),
                size,
            }),
        }
        .to_json(),
    );

    Json(serde_json::json!({
        "ok": true,
        "url": file_url,
        "name": file_name,
        "type": content_type,
        "size": size,
    }))
    .into_response()
}

/// The fields we pull out of the multipart body.
struct Upload {
    peer_id: String,
    file_name: String,
    content_type: Option<String>,
    bytes: Vec<u8>,
}

/// Read the `peer_id` text field and the `file` part from the multipart body.
async fn parse_upload(mut multipart: Multipart) -> Result<Upload, Response> {
    let mut peer_id: Option<String> = None;
    let mut file_name: Option<String> = None;
    let mut content_type: Option<String> = None;
    let mut bytes: Option<Vec<u8>> = None;

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(_) => return Err((StatusCode::BAD_REQUEST, "malformed upload").into_response()),
        };
        match field.name() {
            Some("peer_id") => {
                peer_id = field.text().await.ok().map(|s| s.trim().to_string());
            }
            Some("file") => {
                file_name = field.file_name().map(|s| s.to_string());
                content_type = field.content_type().map(|s| s.to_string());
                match field.bytes().await {
                    Ok(b) => bytes = Some(b.to_vec()),
                    Err(_) => {
                        return Err((StatusCode::BAD_REQUEST, "could not read file").into_response())
                    }
                }
            }
            _ => {} // ignore unknown fields
        }
    }

    let peer_id = peer_id.filter(|s| !s.is_empty());
    match (peer_id, file_name, bytes) {
        (Some(peer_id), Some(file_name), Some(bytes)) if !file_name.trim().is_empty() => {
            Ok(Upload {
                peer_id,
                file_name: file_name.trim().to_string(),
                content_type,
                bytes,
            })
        }
        _ => Err((StatusCode::BAD_REQUEST, "missing peer_id or file").into_response()),
    }
}

/// Run the type-specific extraction, returning `(text, source_lang)`. Failures
/// degrade to an empty body so the file still posts as a chip.
async fn extract_text(
    state: &AppState,
    kind: FileKind,
    bytes: Vec<u8>,
    content_type: &str,
    sender_lang: &str,
) -> (String, String) {
    match kind {
        FileKind::Audio => {
            match deepgram::transcribe_file(&state.http, &state.config, bytes, content_type).await {
                Ok((transcript, detected)) => {
                    let lang = detected.unwrap_or_else(|| sender_lang.to_string());
                    (transcript, lang)
                }
                Err(e) => {
                    tracing::error!("audio transcription failed: {e}");
                    (String::new(), sender_lang.to_string())
                }
            }
        }
        FileKind::Text => (
            String::from_utf8_lossy(&bytes).to_string(),
            sender_lang.to_string(),
        ),
        FileKind::Pdf => {
            // pdf_extract is synchronous + CPU-bound — run it off the async pool.
            let text = tokio::task::spawn_blocking(move || {
                pdf_extract::extract_text_from_mem(&bytes).unwrap_or_default()
            })
            .await
            .unwrap_or_default();
            (text, sender_lang.to_string())
        }
    }
}

/// Map a lowercased extension to a [`FileKind`]; `None` rejects the upload.
pub fn classify_ext(ext: &str) -> Option<FileKind> {
    match ext {
        "mp3" | "wav" => Some(FileKind::Audio),
        "txt" => Some(FileKind::Text),
        "pdf" => Some(FileKind::Pdf),
        _ => None,
    }
}

/// The lowercased extension of a file name (without the dot), or empty.
pub fn ext_of(name: &str) -> String {
    name.rsplit('.')
        .next()
        .filter(|e| !e.is_empty() && *e != name)
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default()
}

/// Pick a content type: trust a sane client-provided one, else derive from the
/// file kind. Returns a `'static str` so it can travel with the broadcast.
fn normalize_content_type(provided: Option<&str>, kind: FileKind) -> &'static str {
    // Only honor a provided type that matches the kind family; otherwise the
    // canonical type avoids a client mislabeling the bytes for Deepgram/storage.
    match kind {
        FileKind::Audio => match provided {
            Some(p)
                if p.eq_ignore_ascii_case("audio/wav") || p.eq_ignore_ascii_case("audio/x-wav") =>
            {
                "audio/wav"
            }
            Some(p)
                if p.eq_ignore_ascii_case("audio/mpeg") || p.eq_ignore_ascii_case("audio/mp3") =>
            {
                "audio/mpeg"
            }
            // Unknown/missing: default by the common case (mp3).
            _ => "audio/mpeg",
        },
        FileKind::Text => "text/plain",
        FileKind::Pdf => "application/pdf",
    }
}

/// Truncate to at most `max` characters on a char boundary (UTF-8 safe).
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// Validate the room code from the path (rooms are short user-chosen codes).
/// Mirrors `api::clean_room` (kept local to avoid a cross-module pub).
#[allow(clippy::result_large_err)]
fn clean_room(room: &str) -> Result<String, Response> {
    let r = room.trim();
    if r.is_empty() || r.len() > 64 {
        return Err((StatusCode::BAD_REQUEST, "invalid room").into_response());
    }
    Ok(r.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_extraction() {
        assert_eq!(ext_of("memo.MP3"), "mp3");
        assert_eq!(ext_of("notes.txt"), "txt");
        assert_eq!(ext_of("archive.tar.gz"), "gz");
        assert_eq!(ext_of("noext"), "");
        assert_eq!(ext_of(".hidden"), "hidden");
        assert_eq!(ext_of(""), "");
    }

    #[test]
    fn classification_covers_mvp_types_only() {
        assert_eq!(classify_ext("mp3"), Some(FileKind::Audio));
        assert_eq!(classify_ext("wav"), Some(FileKind::Audio));
        assert_eq!(classify_ext("txt"), Some(FileKind::Text));
        assert_eq!(classify_ext("pdf"), Some(FileKind::Pdf));
        assert_eq!(classify_ext("exe"), None);
        assert_eq!(classify_ext("png"), None);
        assert_eq!(classify_ext(""), None);
    }

    #[test]
    fn content_type_normalization() {
        assert_eq!(
            normalize_content_type(Some("audio/wav"), FileKind::Audio),
            "audio/wav"
        );
        assert_eq!(
            normalize_content_type(Some("audio/x-wav"), FileKind::Audio),
            "audio/wav"
        );
        assert_eq!(
            normalize_content_type(Some("audio/mpeg"), FileKind::Audio),
            "audio/mpeg"
        );
        // Missing/garbage audio type defaults to mp3.
        assert_eq!(normalize_content_type(None, FileKind::Audio), "audio/mpeg");
        assert_eq!(
            normalize_content_type(Some("application/octet-stream"), FileKind::Audio),
            "audio/mpeg"
        );
        assert_eq!(normalize_content_type(None, FileKind::Text), "text/plain");
        assert_eq!(
            normalize_content_type(Some("anything"), FileKind::Pdf),
            "application/pdf"
        );
    }

    #[test]
    fn truncation_is_utf8_safe() {
        assert_eq!(truncate_chars("hello", 10), "hello");
        assert_eq!(truncate_chars("hello", 3), "hel");
        // Multi-byte chars are not split.
        let s = "àèìòù";
        assert_eq!(truncate_chars(s, 2), "àè");
        assert_eq!(truncate_chars(s, 99), s);
    }

    #[test]
    fn clean_room_bounds() {
        assert!(clean_room("").is_err());
        assert!(clean_room(&"x".repeat(65)).is_err());
        assert_eq!(clean_room("  plaza  ").unwrap(), "plaza");
    }
}
