//! Chat file upload + processing pipeline (spec 0018).
//!
//! `POST /api/rooms/{room}/files` accepts a multipart body (`peer_id` + `file`),
//! stores the bytes in Supabase Storage, extracts text from documents (txt-family
//! → UTF-8, pdf → text layer, docx → `word/document.xml`; other accepted formats
//! are stored-only), runs the **existing** translation fan-out only when text was
//! extracted, and broadcasts a `ChatMessage` carrying the file `attachment` plus
//! the translated text — so the whole chat pipeline (render, transcript
//! persistence, unread tracking) is reused unchanged.
//!
//! Authorization is room membership: the `peer_id` must be a live member of the
//! room (the same trust model as typed WebSocket chat), so the feature works for
//! guests too. No JWT is required.

use axum::extract::{Multipart, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rust_decimal::Decimal;
use uuid::Uuid;

use crate::protocol::{Attachment, ServerMessage};
use crate::transcripts::{EventKind, TranscriptEvent};
use crate::{db, now_unix, storage, AppState};

/// Hard ceiling on the multipart body (route-level). Comfortably above the default
/// 5 MiB file cap to leave room for multipart boundaries/headers; the handler
/// enforces the exact configured `storage.max_bytes` on the file itself.
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Cap on extracted text fed to the translation fan-out, to bound latency/cost
/// for very long documents. Truncation happens on a char boundary.
const MAX_TEXT_CHARS: usize = 4000;

/// What kind of processing an upload gets, derived from its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// Plain-text family (`.txt` / `.md` / `.csv` / `.log`) → decoded as UTF-8.
    Text,
    /// `.pdf` → text-layer extraction (no OCR).
    Pdf,
    /// `.docx` → text pulled from the zipped `word/document.xml`.
    Docx,
    /// Other accepted document formats (`.doc` / `.odt` / `.rtf` / `.xlsx` /
    /// `.pptx`) — stored + shared as an attachment, but NOT text-extracted, so
    /// they never hit the (paid) translation fan-out.
    Other,
    /// A recorded voice message (`.webm` / `.m4a` / `.ogg` / `.mp3` / …). Stored
    /// and shared like any attachment, transcribed via Deepgram's prerecorded API,
    /// and the transcript then rides the same translation fan-out as typed chat.
    Audio,
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
    headers: HeaderMap,
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

    // Per-IP throttle BEFORE reading/buffering the multipart body (issue #117): caps how
    // often one source can trigger a multi-MB in-memory upload buffer, independent of the
    // per-room:peer limit below (which only applies once the body is already parsed).
    let ip = crate::observability::client_ip(&headers);
    if !state.rate_limiter.allow(
        &format!("upload-ip:{ip}"),
        20,
        std::time::Duration::from_secs(60),
    ) {
        return (StatusCode::TOO_MANY_REQUESTS, "too many requests").into_response();
    }
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
        bytes,
    } = parsed;

    // ---- Authorize: the uploader must be a live member of the room ---------
    let Some(snapshot) = state.rooms.peer_snapshot(&room, &peer_id) else {
        return (StatusCode::FORBIDDEN, "not a member of this room").into_response();
    };

    // Throttle uploads per uploader (spec 0029) — each one drives Deepgram/Groq.
    if !state.rate_limiter.allow(
        &format!("upload:{room}:{peer_id}"),
        10,
        std::time::Duration::from_secs(60),
    ) {
        return (StatusCode::TOO_MANY_REQUESTS, "too many requests").into_response();
    }

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
            "unsupported file type (documents: txt, md, csv, pdf, docx, doc, odt, rtf, xlsx, pptx; \
             or voice messages: webm, m4a, mp3, ogg, wav)",
        )
            .into_response();
    };
    // Always derive the canonical content type from the extension, ignoring
    // whatever the client labeled the bytes.
    let content_type = content_type_for(&ext);

    let size = bytes.len() as u64;

    // The room's call-session id ties the file to the call lifetime.
    let session_id = state.rooms.session_id(&room).unwrap_or_else(Uuid::nil);

    // ---- Upload to Supabase Storage (must succeed) -------------------------
    let object = storage::object_path(&session_id.to_string(), &Uuid::new_v4().to_string(), &ext);
    if let Err(e) = storage_client
        .upload(&object, bytes.clone(), content_type)
        .await
    {
        tracing::error!("chat file upload to storage failed: {e}");
        return (StatusCode::BAD_GATEWAY, "storage upload failed").into_response();
    }
    // The bucket is private, so the chat link is a time-limited signed URL.
    let file_url = match storage_client.create_signed_url(&object).await {
        Ok(url) => url,
        Err(e) => {
            tracing::error!("chat file signed-url failed: {e}");
            return (StatusCode::BAD_GATEWAY, "storage sign failed").into_response();
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

    // ---- Get the translatable text (best-effort; failure → empty body) -----
    // Documents are text-extracted locally; a voice message is transcribed via
    // Deepgram's prerecorded API. Either way the result flows through the same
    // translation fan-out + persistence as typed chat below.
    let (text, source_lang) = if kind == FileKind::Audio {
        transcribe_audio(&state, bytes, content_type, &snapshot.lang).await
    } else {
        extract_text(kind, bytes, &snapshot.lang).await
    };
    let text = truncate_chars(text.trim(), MAX_TEXT_CHARS);

    // ---- Translate (paid Groq fan-out) — pay-to-translate policy --------------
    // Groq is only called when there's extracted text AND the upload is paid for:
    //   * unmonetized server (no billing) → free;
    //   * monetized → a signed-in user with credits is charged (cost + margin) and
    //     translated; a guest or out-of-credits user gets the file as a plain
    //     attachment, no Groq call → no cost. `blocked` carries the reason.
    let targets = if text.is_empty() {
        Vec::new()
    } else {
        state.rooms.get_room_languages(&room, &peer_id)
    };
    let mut blocked: Option<&'static str> = None;
    let do_translate = if text.is_empty() || targets.is_empty() {
        false
    } else if let (Some(bcfg), Some(billing)) =
        (state.config.billing.as_ref(), state.billing.as_ref())
    {
        // Resolve the signed-in uploader from an optional bearer token.
        let user_id = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .and_then(|tok| crate::auth::verify_jwt(&bcfg.jwt_secret, tok).ok())
            .and_then(|c| Uuid::parse_str(&c.sub).ok());
        match user_id {
            None => {
                blocked = Some("signin");
                false
            }
            Some(uid) => {
                let cost = upload_translate_cost(&bcfg.ai, targets.len());
                let charge_session = (!session_id.is_nil()).then_some(session_id);
                match billing
                    .deduct_feature(
                        uid,
                        charge_session,
                        "upload_translate",
                        cost,
                        &format!("Chat upload translation: {file_name} ({} langs)", targets.len()),
                        serde_json::json!({ "file": file_name, "ext": ext, "langs": targets.len() }),
                    )
                    .await
                {
                    Ok(_) => true,
                    Err(crate::billing::BillingError::InsufficientFunds) => {
                        blocked = Some("credits");
                        false
                    }
                    Err(e) => {
                        tracing::error!("upload translate charge failed: {e}");
                        false
                    }
                }
            }
        }
    } else {
        // No billing configured (self-hosted / dev) → translate for free.
        true
    };

    let translations: std::collections::HashMap<String, String> = if do_translate {
        let glossary = state.glossary.as_ref().and_then(|g| g.cached(&room));
        state
            .translator
            .translate_fanout(&text, &source_lang, &targets, glossary.as_deref())
            .await
    } else {
        std::collections::HashMap::new()
    };
    let translated = do_translate;

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
        // Whether the document text was translated; if not, why (so the uploader's
        // client can nudge: "signin" → sign in, "credits" → top up). `null` when
        // there was nothing to translate (stored-only format / no other languages).
        "translated": translated,
        "translate_blocked": blocked,
    }))
    .into_response()
}

/// The fields we pull out of the multipart body.
struct Upload {
    peer_id: String,
    file_name: String,
    bytes: Vec<u8>,
}

/// Read the `peer_id` text field and the `file` part from the multipart body.
async fn parse_upload(mut multipart: Multipart) -> Result<Upload, Response> {
    let mut peer_id: Option<String> = None;
    let mut file_name: Option<String> = None;
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
                bytes,
            })
        }
        _ => Err((StatusCode::BAD_REQUEST, "missing peer_id or file").into_response()),
    }
}

/// Run the type-specific extraction, returning `(text, source_lang)`. Failures
/// degrade to an empty body so the file still posts as a chip.
async fn extract_text(kind: FileKind, bytes: Vec<u8>, sender_lang: &str) -> (String, String) {
    match kind {
        FileKind::Text => (
            String::from_utf8_lossy(&bytes).to_string(),
            sender_lang.to_string(),
        ),
        FileKind::Pdf => {
            // pdf_extract is synchronous + CPU-bound — run it off the async pool,
            // bounded by a timeout so a decompression-bomb PDF can't pin the request
            // (spec 0029). The blocking task may run on, but the request returns.
            // NB: the timeout bounds CPU/time exhaustion only — it does NOT stop a
            // genuine stack overflow, which aborts the whole process regardless. That
            // DoS (RUSTSEC-2026-0187) is closed at the root by lopdf >= 0.42, not by
            // this timeout or the size guard below.
            let handle = tokio::task::spawn_blocking(move || pdf_text_from_bytes(&bytes));
            let text = tokio::time::timeout(std::time::Duration::from_secs(15), handle)
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();
            (text, sender_lang.to_string())
        }
        FileKind::Docx => {
            // Same guarded off-thread pattern as PDF — unzip + parse is CPU-bound,
            // and capped against a zip bomb inside docx_extract.
            let handle = tokio::task::spawn_blocking(move || docx_extract(&bytes));
            let text = tokio::time::timeout(std::time::Duration::from_secs(15), handle)
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();
            (text, sender_lang.to_string())
        }
        // Stored + shared, but not extracted → no translation cost.
        FileKind::Other => (String::new(), sender_lang.to_string()),
        // Audio is transcribed by `transcribe_audio` (needs the HTTP client +
        // config), not here — guarded by the FileKind::Audio branch in upload_file.
        FileKind::Audio => (String::new(), sender_lang.to_string()),
    }
}

/// Cap on the PDF bytes we hand to `pdf_extract` (a text-layer document that big
/// is not a normal chat attachment). The uploaded-file route already bounds input
/// before extraction — `SUPABASE_MAX_UPLOAD_BYTES` (default 5 MiB) per file, under
/// the 8 MiB `MAX_BODY_BYTES` multipart ceiling — so this 16 MiB cap is pure
/// defense-in-depth for the CPU-bound, deeply-nesting-sensitive parser
/// (RUSTSEC-2026-0187): an oversized buffer is skipped up front rather than fed to
/// the extractor. Sits above the worst-case admitted upload so it never rejects a
/// normally accepted file.
const MAX_PDF_EXTRACT_BYTES: usize = 16 * 1024 * 1024;

/// Whether a buffer of `len` bytes should be handed to the `pdf_extract` parser.
/// The guard: non-empty and within [`MAX_PDF_EXTRACT_BYTES`]. Split out as a pure
/// predicate so the size-gate is deterministically unit-testable (an oversized
/// document is otherwise indistinguishable from one the parser simply can't read).
fn should_extract_pdf(len: usize) -> bool {
    len != 0 && len <= MAX_PDF_EXTRACT_BYTES
}

/// Extract the text layer from PDF `bytes`, best-effort. An empty or oversized
/// buffer short-circuits to an empty string WITHOUT invoking the CPU-bound
/// `pdf_extract` parser; any extraction failure also degrades to empty (the file
/// still posts as an attachment). Pure + synchronous, so it's unit-testable and
/// safe to run under `spawn_blocking`.
fn pdf_text_from_bytes(bytes: &[u8]) -> String {
    if !should_extract_pdf(bytes.len()) {
        return String::new();
    }
    pdf_extract::extract_text_from_mem(bytes).unwrap_or_default()
}

/// Transcribe a recorded voice message via Deepgram's prerecorded API, returning
/// `(transcript, source_lang)`. The selected call tier drives the LIVE streaming
/// STT engine, but a discrete clip always uses Deepgram batch: it accepts the
/// MediaRecorder WebM/Opus (or m4a) container directly and auto-detects the
/// language. Any failure degrades to an empty transcript, so the clip still posts
/// as a playable attachment (just without translatable text), matching the
/// best-effort document path.
async fn transcribe_audio(
    state: &AppState,
    bytes: Vec<u8>,
    content_type: &str,
    sender_lang: &str,
) -> (String, String) {
    match crate::deepgram::transcribe_file(&state.http, &state.config, bytes, content_type).await {
        Ok((transcript, detected)) => (transcript, normalize_detected_lang(detected, sender_lang)),
        Err(e) => {
            tracing::error!("voice message transcription failed: {e}");
            (String::new(), sender_lang.to_string())
        }
    }
}

/// Normalize Deepgram's detected language to the app's short code: the primary
/// subtag, lowercased (e.g. "en-US" → "en"). Falls back to `sender_lang` when
/// detection is absent or empty, so the transcript always has a source language
/// for the translation fan-out. Pure, for tests.
fn normalize_detected_lang(detected: Option<String>, sender_lang: &str) -> String {
    detected
        .map(|l| l.split('-').next().unwrap_or(&l).to_ascii_lowercase())
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| sender_lang.to_string())
}

/// Extract plain text from a `.docx` (a zip whose `word/document.xml` holds the
/// body as `<w:t>` runs). Best-effort: any error → empty string (the file still
/// posts as an attachment). Bounded against zip bombs by capping the decompressed
/// XML we read.
fn docx_extract(bytes: &[u8]) -> String {
    use std::io::Read;
    /// Cap on the decompressed `document.xml` we'll read (defends against a small
    /// docx that inflates to gigabytes).
    const MAX_XML_BYTES: u64 = 64 * 1024 * 1024;

    let reader = std::io::Cursor::new(bytes);
    let Ok(mut zip) = zip::ZipArchive::new(reader) else {
        return String::new();
    };
    let Ok(entry) = zip.by_name("word/document.xml") else {
        return String::new();
    };
    let mut xml = String::new();
    if entry.take(MAX_XML_BYTES).read_to_string(&mut xml).is_err() {
        return String::new();
    }

    // Walk the markup pulling each `<w:t>…</w:t>` run (unescaping its content as a
    // whole, so split entity events can't drop a `&amp;`) and turning `</w:p>`
    // paragraph ends into newlines so words don't run together across paragraphs.
    // All slices land on ASCII markers, so they're always UTF-8 char boundaries.
    let mut out = String::new();
    let mut s = xml.as_str();
    loop {
        let next_t = s.find("<w:t");
        let next_p = s.find("</w:p>");
        match (next_t, next_p) {
            (None, None) => break,
            // Paragraph break reached first → newline.
            (t, Some(p)) if t.is_none_or(|t| p < t) => {
                out.push('\n');
                s = &s[p + "</w:p>".len()..];
            }
            (Some(t), _) => {
                let after = &s[t..];
                // Only the text element `<w:t>` / `<w:t …>`, not `<w:tbl>`/`<w:tab>`/…
                if !matches!(after.as_bytes().get(4), Some(b'>' | b' ' | b'/')) {
                    s = &after["<w:t".len()..];
                    continue;
                }
                let Some(gt) = after.find('>') else { break };
                if after.as_bytes()[gt - 1] == b'/' {
                    s = &after[gt + 1..]; // self-closing <w:t/>
                    continue;
                }
                let content = &after[gt + 1..];
                let Some(end) = content.find("</w:t>") else {
                    break;
                };
                let raw = &content[..end];
                out.push_str(
                    &quick_xml::escape::unescape(raw).unwrap_or(std::borrow::Cow::Borrowed(raw)),
                );
                s = &content[end + "</w:t>".len()..];
            }
            _ => break,
        }
    }
    out
}

/// Map a lowercased extension to a [`FileKind`]; `None` rejects the upload.
pub fn classify_ext(ext: &str) -> Option<FileKind> {
    match ext {
        "txt" | "md" | "csv" | "log" => Some(FileKind::Text),
        "pdf" => Some(FileKind::Pdf),
        "docx" => Some(FileKind::Docx),
        "doc" | "odt" | "rtf" | "xlsx" | "pptx" => Some(FileKind::Other),
        // Voice messages: MediaRecorder emits webm/opus (Chrome/Firefox/Android)
        // or m4a/mp4 (Safari/iOS); the rest cover attached audio files. All are
        // transcribed via Deepgram batch.
        "webm" | "ogg" | "oga" | "opus" | "m4a" | "mp3" | "wav" | "aac" | "flac" => {
            Some(FileKind::Audio)
        }
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

/// The canonical content type for an allowed (already `classify_ext`-validated)
/// extension. Returns a `'static str` so it can travel with the broadcast;
/// derived purely from the extension so a client can't mislabel the bytes.
pub fn content_type_for(ext: &str) -> &'static str {
    match ext {
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "doc" => "application/msword",
        "odt" => "application/vnd.oasis.opendocument.text",
        "rtf" => "application/rtf",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        // Audio — every value starts with `audio/`, which is what the chat client
        // keys the inline <audio> player off.
        "webm" => "audio/webm",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "m4a" => "audio/mp4",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        _ => "application/octet-stream",
    }
}

/// Credits charged to translate an uploaded document into `n_langs` target
/// languages: `base + per_lang × n` (rounded to 6 dp, matching the ledger).
/// Pure so the pricing is unit-tested without a DB/billing service.
pub fn upload_translate_cost(ai: &crate::config::AiConfig, n_langs: usize) -> Decimal {
    Decimal::try_from(ai.upload_translate_base + ai.upload_translate_per_lang * n_langs as f64)
        .unwrap_or(Decimal::ZERO)
        .round_dp(6)
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
    fn classification_covers_documents_and_audio() {
        // Extractable → translated.
        assert_eq!(classify_ext("txt"), Some(FileKind::Text));
        assert_eq!(classify_ext("md"), Some(FileKind::Text));
        assert_eq!(classify_ext("csv"), Some(FileKind::Text));
        assert_eq!(classify_ext("pdf"), Some(FileKind::Pdf));
        assert_eq!(classify_ext("docx"), Some(FileKind::Docx));
        // Accepted but stored-only (no extraction → no translation cost).
        assert_eq!(classify_ext("doc"), Some(FileKind::Other));
        assert_eq!(classify_ext("odt"), Some(FileKind::Other));
        assert_eq!(classify_ext("xlsx"), Some(FileKind::Other));
        assert_eq!(classify_ext("pptx"), Some(FileKind::Other));
        // Voice messages → transcribed via Deepgram, then translated.
        assert_eq!(classify_ext("webm"), Some(FileKind::Audio));
        assert_eq!(classify_ext("m4a"), Some(FileKind::Audio));
        assert_eq!(classify_ext("mp3"), Some(FileKind::Audio));
        assert_eq!(classify_ext("ogg"), Some(FileKind::Audio));
        assert_eq!(classify_ext("wav"), Some(FileKind::Audio));
        // Video and everything else is still rejected.
        assert_eq!(classify_ext("mp4"), None);
        assert_eq!(classify_ext("exe"), None);
        assert_eq!(classify_ext("png"), None);
        assert_eq!(classify_ext(""), None);
    }

    #[test]
    fn upload_translate_cost_is_base_plus_per_lang() {
        let ai = crate::config::AiConfig::test_default(); // base 0.01, per_lang 0.005
        assert_eq!(
            upload_translate_cost(&ai, 0),
            Decimal::try_from(0.01).unwrap()
        );
        assert_eq!(
            upload_translate_cost(&ai, 1),
            Decimal::try_from(0.015).unwrap()
        );
        // A document fanned out to 3 languages.
        assert_eq!(
            upload_translate_cost(&ai, 3),
            Decimal::try_from(0.025).unwrap()
        );
    }

    #[test]
    fn content_types_by_extension() {
        assert_eq!(content_type_for("txt"), "text/plain");
        assert_eq!(content_type_for("md"), "text/markdown");
        assert_eq!(content_type_for("pdf"), "application/pdf");
        assert_eq!(
            content_type_for("docx"),
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        );
        assert_eq!(content_type_for("rtf"), "application/rtf");
        // Audio MIME types must start with `audio/` so the client renders a player.
        assert_eq!(content_type_for("webm"), "audio/webm");
        assert_eq!(content_type_for("m4a"), "audio/mp4");
        assert_eq!(content_type_for("mp3"), "audio/mpeg");
        assert!(content_type_for("ogg").starts_with("audio/"));
    }

    #[test]
    fn detected_lang_normalizes_to_primary_subtag() {
        // Regional tags collapse to the lowercased primary subtag.
        assert_eq!(normalize_detected_lang(Some("en-US".into()), "it"), "en");
        assert_eq!(normalize_detected_lang(Some("IT".into()), "en"), "it");
        assert_eq!(normalize_detected_lang(Some("es".into()), "en"), "es");
        // Absent / empty detection falls back to the sender's language.
        assert_eq!(normalize_detected_lang(None, "de"), "de");
        assert_eq!(normalize_detected_lang(Some(String::new()), "fr"), "fr");
    }

    #[test]
    fn docx_extract_pulls_text_runs() {
        // Build a minimal .docx (zip with word/document.xml) in memory.
        use std::io::Write;
        let xml = r#"<?xml version="1.0"?><w:document xmlns:w="x"><w:body>
            <w:p><w:r><w:t>Hello</w:t></w:r><w:r><w:t xml:space="preserve"> world</w:t></w:r></w:p>
            <w:p><w:r><w:t>Second &amp; line</w:t></w:r></w:p>
            </w:body></w:document>"#;
        let mut buf = Vec::new();
        {
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            zw.start_file::<_, ()>(
                "word/document.xml",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            zw.write_all(xml.as_bytes()).unwrap();
            zw.finish().unwrap();
        }
        let text = docx_extract(&buf);
        assert!(text.contains("Hello world"), "got: {text:?}");
        assert!(text.contains("Second & line"), "got: {text:?}");
        // A non-zip / garbage input degrades to empty, never panics.
        assert_eq!(docx_extract(b"not a zip"), "");
    }

    #[test]
    fn pdf_size_guard_gates_extraction() {
        // The size gate is the defense-in-depth for the CPU-bound parser
        // (RUSTSEC-2026-0187). Test the predicate directly so the boundary is
        // deterministic (an oversized document is otherwise indistinguishable from
        // one the parser simply can't read — both yield empty text).
        assert!(!should_extract_pdf(0)); // empty → skip
        assert!(should_extract_pdf(1)); // smallest non-empty → extract
        assert!(should_extract_pdf(MAX_PDF_EXTRACT_BYTES)); // exactly at cap → extract
        assert!(!should_extract_pdf(MAX_PDF_EXTRACT_BYTES + 1)); // just over → skip
    }

    #[test]
    fn pdf_over_cap_is_skipped_without_extraction() {
        // An oversized buffer is short-circuited to empty WITHOUT handing the
        // (potentially malicious) bytes to `pdf_extract`. Building a 16 MiB+ vec
        // and getting an instant empty result proves the guard fired: a real parse
        // of a buffer this size would be measurably slow, not instant.
        let oversized = vec![b'%'; MAX_PDF_EXTRACT_BYTES + 1];
        assert_eq!(
            pdf_text_from_bytes(&oversized),
            "",
            "oversized PDF must be skipped, returning empty without extraction"
        );

        // An empty buffer is likewise skipped (never handed to the parser).
        assert_eq!(pdf_text_from_bytes(&[]), "");

        // A small, valid PDF is NOT skipped: it reaches the extractor and its text
        // layer is recovered — proving the guard gates on size, not a blanket
        // reject, and that the pdf-extract 0.12 upgrade still extracts text.
        let pdf = minimal_text_pdf("Guard me");
        assert!(
            pdf_text_from_bytes(&pdf).contains("Guard me"),
            "a small valid PDF must still extract after the pdf-extract 0.12 upgrade"
        );
    }

    /// Build a minimal, valid single-page PDF whose content stream draws `text`,
    /// so `pdf_extract` can recover it. Enough structure (xref + trailer) for the
    /// parser to walk the document; used to prove small valid PDFs still extract.
    fn minimal_text_pdf(text: &str) -> Vec<u8> {
        let content = format!("BT /F1 24 Tf 72 720 Td ({text}) Tj ET");
        let objs: Vec<String> = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
             /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                .into(),
            format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            ),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
        ];

        let mut pdf = String::from("%PDF-1.4\n");
        let mut offsets = Vec::with_capacity(objs.len());
        for (i, body) in objs.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.push_str(&format!("{} 0 obj\n{body}\nendobj\n", i + 1));
        }
        let xref_start = pdf.len();
        pdf.push_str(&format!("xref\n0 {}\n", objs.len() + 1));
        pdf.push_str("0000000000 65535 f \n");
        for off in &offsets {
            pdf.push_str(&format!("{off:010} 00000 n \n"));
        }
        pdf.push_str(&format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_start}\n%%EOF",
            objs.len() + 1
        ));
        pdf.into_bytes()
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
