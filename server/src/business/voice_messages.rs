//! Project-attached voice messages (spec: B2B project voice notes).
//!
//! A Business/Enterprise member records a voice note straight onto a PROJECT (no
//! video call) from the app or the dashboard. The clip is transcribed (Deepgram
//! prerecorded), translated to the project's languages (Groq fan-out, charged to
//! the org pool — same as the post-call translate path), and lands in the
//! project's data history so the EXISTING semantic search + insights RAG consume
//! it: it materializes a lightweight `call_sessions` row tagged
//! `kind = 'voice_message'` plus a `transcripts` row, then is embedded via the
//! shared [`embed_and_store`] exactly like a real call. The audio artifact +
//! list metadata live in `project_voice_messages` (migration 032).
//!
//! Auth: an org member (`require_role` MEMBER) on an org with an active
//! Business/Enterprise subscription (`org_subscription_active`).

use std::collections::HashMap;

use axum::extract::{Multipart, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use serde_json::json;
use sqlx::FromRow;
use uuid::Uuid;

use crate::business::transcripts::{embed_and_store, Segment};
use crate::business::{
    bad_request, credits, db_err, forbidden, not_found, require_pool, require_role, MEMBER,
};
use crate::files::{classify_ext, content_type_for, ext_of, FileKind, MAX_BODY_BYTES};
use crate::middleware::AuthUser;
use crate::AppState;

/// `POST /api/business/organizations/{org_id}/projects/{project_id}/voice-messages`
/// — record a voice note onto a project. Member of an actively-subscribed org.
pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, project_id)): Path<(Uuid, Uuid)>,
    multipart: Multipart,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;

    // Project voice notes are a paid Business/Enterprise feature: require a live
    // (active, unlapsed) subscription — the same gate as cloud recording, so an
    // admin-gifted month stops unlocking it the moment it ends.
    if !credits::org_subscription_active(pool, org_id)
        .await
        .map_err(db_err)?
    {
        return Err(forbidden(
            "voice messages require an active Business or Enterprise subscription",
        ));
    }

    // The project must belong to this org and be active; fetch its languages.
    let default_languages: Option<Vec<String>> = sqlx::query_scalar(
        "SELECT default_languages FROM projects
         WHERE id = $1 AND org_id = $2 AND archived_at IS NULL",
    )
    .bind(project_id)
    .bind(org_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    let default_languages =
        default_languages.ok_or_else(|| bad_request("project does not belong to this org"))?;

    // Storage holds the audio artifact (the chat-files bucket; voice notes are
    // small chat-like clips, not the multi-GB call recordings).
    let Some(storage_client) = state.storage.as_ref() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "file upload is not configured",
        )
            .into_response());
    };

    // ---- Parse + validate the audio ----------------------------------------
    let Upload {
        file_name,
        bytes,
        duration_seconds,
    } = parse_upload(multipart).await?;
    if bytes.is_empty() {
        return Err(bad_request("empty file"));
    }
    let max_bytes = state
        .config
        .storage
        .as_ref()
        .map(|c| c.max_bytes)
        .unwrap_or(MAX_BODY_BYTES);
    if bytes.len() > max_bytes {
        return Err((StatusCode::PAYLOAD_TOO_LARGE, "file too large").into_response());
    }
    let ext = ext_of(&file_name);
    if classify_ext(&ext) != Some(FileKind::Audio) {
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "voice messages must be audio (webm, m4a, mp3, ogg, wav)",
        )
            .into_response());
    }
    let content_type = content_type_for(&ext);
    let size = bytes.len() as u64;

    let uploader_name: String = sqlx::query_scalar("SELECT name FROM users WHERE id = $1")
        .bind(user.user_id)
        .fetch_optional(pool)
        .await
        .map_err(db_err)?
        .unwrap_or_else(|| "Member".to_string());

    // Ids are generated up front so the credit ledger row can reference the
    // session even though it's persisted last.
    let session_id = Uuid::new_v4();
    let file_id = Uuid::new_v4();

    // ---- Transcribe (best-effort; failure → empty transcript, still posts) --
    let fallback_lang = default_languages
        .first()
        .cloned()
        .unwrap_or_else(|| "en".to_string());
    let (transcript, source_lang) = match crate::deepgram::transcribe_file(
        &state.http,
        &state.config,
        bytes.clone(),
        content_type,
    )
    .await
    {
        Ok((t, detected)) => (
            t.trim().to_string(),
            normalize_detected_lang(detected, &fallback_lang),
        ),
        Err(e) => {
            tracing::error!("voice message transcription failed: {e}");
            (String::new(), fallback_lang.clone())
        }
    };
    let word_count = transcript.split_whitespace().count() as i64;

    // ---- Upload the artifact (durable; before any charge so a storage failure
    //      never orphans a deduction) -----------------------------------------
    let object = crate::storage::object_path(&project_id.to_string(), &file_id.to_string(), &ext);
    if let Err(e) = storage_client.upload(&object, bytes, content_type).await {
        tracing::error!("voice message upload to storage failed: {e}");
        return Err((StatusCode::BAD_GATEWAY, "storage upload failed").into_response());
    }

    // ---- Translate to the project's languages, charging the org pool --------
    // Mirrors the post-call translate-on-demand path: 2 credits / 1000 words,
    // type 'translation'. Out of credits → save untranslated (the audio +
    // transcript stay useful), exactly like document uploads.
    let targets: Vec<String> = default_languages
        .iter()
        .filter(|l| l.as_str() != source_lang)
        .cloned()
        .collect();
    let mut translations: HashMap<String, String> = HashMap::new();
    let mut translated = false;
    let mut translate_blocked: Option<&'static str> = None;
    let cost = credits::translation_credits(word_count);

    if !transcript.is_empty() && !targets.is_empty() {
        let charge = if cost > 0 {
            credits::deduct_org_credits(
                pool,
                org_id,
                cost,
                "translation",
                Some(session_id),
                Some(user.user_id),
                "Project voice message translation",
            )
            .await
            .map_err(db_err)?
        } else {
            credits::OrgCharge::Charged { balance_after: 0 }
        };
        match charge {
            credits::OrgCharge::Insufficient { .. } => translate_blocked = Some("credits"),
            credits::OrgCharge::Charged { .. } => {
                let glossary = None; // project voice notes carry no room glossary
                let fanout = state
                    .translator
                    .translate_fanout(&transcript, &source_lang, &targets, glossary)
                    .await;
                // `translate_fanout` echoes the source language back; keep only the
                // actual target translations for the cache.
                translations = fanout
                    .into_iter()
                    .filter(|(k, _)| k.as_str() != source_lang)
                    .collect();
                if translations.is_empty() {
                    // Groq produced nothing — refund the deduction, like insights.
                    if cost > 0 {
                        let _ = credits::add_org_credits(
                            pool,
                            org_id,
                            cost,
                            "translation",
                            "Refund: project voice message translation failed",
                            None,
                        )
                        .await;
                    }
                    translate_blocked = Some("error");
                } else {
                    translated = true;
                }
            }
        }
    }

    // ---- Persist (one transaction): synthetic session + uploader participant +
    //      transcript + the voice-message artifact row ------------------------
    let segment = Segment {
        speaker_id: format!("vm-{}", user.user_id),
        speaker_name: uploader_name.clone(),
        text: transcript.clone(),
        start_ms: 0,
        end_ms: duration_seconds.map(|s| s as i64 * 1000).unwrap_or(0),
    };
    let segments_json = serde_json::to_value(std::slice::from_ref(&segment)).map_err(|e| {
        tracing::error!("voice message segment serialize failed: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, "serialize error").into_response()
    })?;
    let translations_json = serde_json::to_value(&translations).map_err(|e| {
        tracing::error!("voice message translations serialize failed: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, "serialize error").into_response()
    })?;

    let persisted = persist(
        pool,
        PersistArgs {
            session_id,
            file_id,
            org_id,
            project_id,
            uploader_id: user.user_id,
            uploader_name: &uploader_name,
            source_lang: &source_lang,
            segments_json: &segments_json,
            translations_json: &translations_json,
            object_path: &object,
            file_name: &file_name,
            content_type,
            size: size as i64,
            duration_seconds,
            word_count: word_count as i32,
            translated,
        },
    )
    .await;

    let (id, transcript_id) = match persisted {
        Ok(ids) => ids,
        Err(e) => {
            tracing::error!("voice message persist failed: {e}");
            // Refund any charge and drop the orphaned object so a DB failure
            // leaves no paid-but-lost note.
            if translated && cost > 0 {
                let _ = credits::add_org_credits(
                    pool,
                    org_id,
                    cost,
                    "translation",
                    "Refund: project voice message persist failed",
                    None,
                )
                .await;
            }
            let _ = storage_client.delete(&object).await;
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not save voice message",
            )
                .into_response());
        }
    };

    // ---- Index for semantic search + insights (best-effort, like post-call) -
    if !transcript.is_empty() {
        if let Err(e) = embed_and_store(
            &state,
            transcript_id,
            session_id,
            org_id,
            Some(project_id),
            std::slice::from_ref(&segment),
        )
        .await
        {
            tracing::warn!("voice message embedding failed ({session_id}): {e}");
        }
    }

    let translated_languages: Vec<String> = translations.keys().cloned().collect();
    Ok(Json(json!({
        "id": id,
        "session_id": session_id,
        "transcript_id": transcript_id,
        "project_id": project_id,
        "source_language": source_lang,
        "word_count": word_count,
        "duration_seconds": duration_seconds,
        "translated": translated,
        "translated_languages": translated_languages,
        // Why the transcript wasn't translated, when applicable: 'credits' (org out
        // of credits — note saved untranslated), 'error' (Groq failed, refunded).
        "translate_blocked": translate_blocked,
        "file_name": file_name,
        "content_type": content_type,
        "size": size,
    }))
    .into_response())
}

/// `GET /api/business/organizations/{org_id}/projects/{project_id}/voice-messages`
/// — list a project's voice notes, newest first (member). Audio URLs are minted
/// on demand via the `audio-url` endpoint (signed URLs expire), so they're not
/// embedded here.
pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, project_id)): Path<(Uuid, Uuid)>,
    axum::extract::Query(q): axum::extract::Query<ListQuery>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let page = q.page.unwrap_or(1).max(1);
    let offset = (page - 1) * limit;

    let rows: Vec<VoiceMessageRow> = sqlx::query_as(
        "SELECT id, session_id, transcript_id, created_by_name, file_name, content_type,
                size_bytes, duration_seconds, source_language, word_count, translated, created_at
         FROM project_voice_messages
         WHERE org_id = $1 AND project_id = $2
         ORDER BY created_at DESC
         LIMIT $3 OFFSET $4",
    )
    .bind(org_id)
    .bind(project_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;

    Ok(Json(json!({ "voice_messages": rows, "page": page, "limit": limit })).into_response())
}

/// `GET …/voice-messages/{voice_message_id}/audio-url` — a short-lived signed
/// playback URL for the note's audio (member).
pub async fn audio_url(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, project_id, voice_message_id)): Path<(Uuid, Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;
    let Some(storage_client) = state.storage.as_ref() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "file upload is not configured",
        )
            .into_response());
    };

    let path: Option<String> = sqlx::query_scalar(
        "SELECT object_path FROM project_voice_messages
         WHERE id = $1 AND org_id = $2 AND project_id = $3",
    )
    .bind(voice_message_id)
    .bind(org_id)
    .bind(project_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    let path = path.ok_or_else(|| not_found("voice message not found"))?;

    let signed = storage_client.create_signed_url(&path).await.map_err(|e| {
        tracing::error!("voice message sign failed: {e}");
        (StatusCode::BAD_GATEWAY, "could not sign voice message").into_response()
    })?;
    Ok(Json(json!({ "url": signed })).into_response())
}

#[derive(serde::Deserialize, Default)]
pub struct ListQuery {
    page: Option<i64>,
    limit: Option<i64>,
}

#[derive(FromRow, Serialize)]
struct VoiceMessageRow {
    id: Uuid,
    session_id: Uuid,
    transcript_id: Option<Uuid>,
    created_by_name: String,
    file_name: String,
    content_type: String,
    size_bytes: i64,
    duration_seconds: Option<i32>,
    source_language: String,
    word_count: Option<i32>,
    translated: bool,
    created_at: chrono::DateTime<chrono::Utc>,
}

/// The fields pulled from the multipart body: the audio `file` plus an optional
/// client-measured `duration_seconds` (the server can't cheaply probe it).
struct Upload {
    file_name: String,
    bytes: Vec<u8>,
    duration_seconds: Option<i32>,
}

async fn parse_upload(mut multipart: Multipart) -> Result<Upload, Response> {
    let mut file_name: Option<String> = None;
    let mut bytes: Option<Vec<u8>> = None;
    let mut duration_seconds: Option<i32> = None;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(_) => return Err((StatusCode::BAD_REQUEST, "malformed upload").into_response()),
        };
        match field.name() {
            Some("duration_seconds") => {
                duration_seconds = field
                    .text()
                    .await
                    .ok()
                    .and_then(|s| s.trim().parse::<i32>().ok())
                    .filter(|d| *d >= 0);
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
            _ => {}
        }
    }
    match (file_name, bytes) {
        (Some(file_name), Some(bytes)) if !file_name.trim().is_empty() => Ok(Upload {
            file_name: file_name.trim().to_string(),
            bytes,
            duration_seconds,
        }),
        _ => Err((StatusCode::BAD_REQUEST, "missing file").into_response()),
    }
}

/// Args for [`persist`] — grouped to keep the call site readable (clippy's
/// too-many-arguments).
struct PersistArgs<'a> {
    session_id: Uuid,
    file_id: Uuid,
    org_id: Uuid,
    project_id: Uuid,
    uploader_id: Uuid,
    uploader_name: &'a str,
    source_lang: &'a str,
    segments_json: &'a serde_json::Value,
    translations_json: &'a serde_json::Value,
    object_path: &'a str,
    file_name: &'a str,
    content_type: &'a str,
    size: i64,
    duration_seconds: Option<i32>,
    word_count: i32,
    translated: bool,
}

/// Insert the synthetic session, the uploader's participant row (the visibility
/// hook for member search scope), the transcript, and the artifact row — all in
/// one transaction. Returns `(voice_message_id, transcript_id)`.
async fn persist(pool: &crate::db::Pool, a: PersistArgs<'_>) -> Result<(Uuid, Uuid), sqlx::Error> {
    let mut tx = pool.begin().await?;
    // Synthetic, project-bound session tagged so analytics/history exclude it from
    // call KPIs. started_at = ended_at = now() ⇒ zero minutes even if a filter is missed.
    sqlx::query(
        "INSERT INTO call_sessions
            (id, room, org_id, project_id, kind, transcript_status, ended_at)
         VALUES ($1, $2, $3, $4, 'voice_message', 'ready', now())",
    )
    .bind(a.session_id)
    .bind(format!("vm-{}", a.file_id))
    .bind(a.org_id)
    .bind(a.project_id)
    .execute(&mut *tx)
    .await?;

    // Uploader as a participant → makes the project "participated-in", so the
    // member retrieves their own note under the unchanged search/insights scope.
    sqlx::query(
        "INSERT INTO session_participants (session_id, peer_id, user_id, name, lang)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(a.session_id)
    .bind(format!("vm-{}", a.uploader_id))
    .bind(a.uploader_id)
    .bind(a.uploader_name)
    .bind(a.source_lang)
    .execute(&mut *tx)
    .await?;

    let transcript_id: Uuid = sqlx::query_scalar(
        "INSERT INTO transcripts
            (session_id, org_id, source_language, segments, translations,
             duration_seconds, word_count, processed_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, now())
         RETURNING id",
    )
    .bind(a.session_id)
    .bind(a.org_id)
    .bind(a.source_lang)
    .bind(a.segments_json)
    .bind(a.translations_json)
    .bind(a.duration_seconds)
    .bind(a.word_count)
    .fetch_one(&mut *tx)
    .await?;

    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO project_voice_messages
            (org_id, project_id, session_id, transcript_id, created_by, created_by_name,
             object_path, file_name, content_type, size_bytes, duration_seconds,
             source_language, word_count, translated)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
         RETURNING id",
    )
    .bind(a.org_id)
    .bind(a.project_id)
    .bind(a.session_id)
    .bind(transcript_id)
    .bind(a.uploader_id)
    .bind(a.uploader_name)
    .bind(a.object_path)
    .bind(a.file_name)
    .bind(a.content_type)
    .bind(a.size)
    .bind(a.duration_seconds)
    .bind(a.source_lang)
    .bind(a.word_count)
    .bind(a.translated)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok((id, transcript_id))
}

/// Normalize Deepgram's detected language to the app's short code: the primary
/// subtag, lowercased (e.g. "en-US" → "en"). Falls back to `fallback` when
/// detection is absent or empty. (Local copy of `files::normalize_detected_lang`
/// so the chat path stays untouched.)
fn normalize_detected_lang(detected: Option<String>, fallback: &str) -> String {
    detected
        .map(|l| l.split('-').next().unwrap_or(&l).to_ascii_lowercase())
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detected_lang_normalizes_or_falls_back() {
        assert_eq!(normalize_detected_lang(Some("en-US".into()), "it"), "en");
        assert_eq!(normalize_detected_lang(Some("ES".into()), "en"), "es");
        assert_eq!(normalize_detected_lang(None, "de"), "de");
        assert_eq!(normalize_detected_lang(Some(String::new()), "fr"), "fr");
    }

    #[test]
    fn only_audio_is_accepted() {
        // The endpoint is audio-only (documents go through the chat upload path).
        assert_eq!(classify_ext("webm"), Some(FileKind::Audio));
        assert_eq!(classify_ext("m4a"), Some(FileKind::Audio));
        assert_ne!(classify_ext("pdf"), Some(FileKind::Audio));
        assert_eq!(classify_ext("mp4"), None);
    }
}
