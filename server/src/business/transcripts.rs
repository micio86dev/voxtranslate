//! Diarized post-call transcripts (spec 0106): the async transcription job plus
//! read / translate (cached) / export endpoints. Distinct from the realtime
//! `transcript_events` pipeline — this works off the cloud recording.

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::FromRow;
use uuid::Uuid;

use crate::business::{
    audit, bad_request, credits, db_err, not_found, require_call_role, require_pool, MEMBER,
};
use crate::middleware::AuthUser;
use crate::transcripts::{ExportEvent, ExportParticipant, ExportSession, TranscriptExport};
use crate::{deepgram, pdf, AppState};

/// One diarized segment as stored in `transcripts.segments`.
#[derive(Serialize, Deserialize, Clone)]
pub struct Segment {
    pub speaker_id: String,
    pub speaker_name: String,
    pub text: String,
    pub start_ms: i64,
    pub end_ms: i64,
}

#[derive(FromRow)]
struct TranscriptRow {
    source_language: String,
    segments: Value,
    translations: Value,
    duration_seconds: Option<i32>,
    word_count: Option<i32>,
}

// ---- Async transcription job -------------------------------------------------

/// Kick off transcription of a finished recording in the background. Drives
/// `call_sessions.transcript_status`: caller has already set it to `processing`.
pub fn spawn_transcription(state: AppState, session_id: Uuid, org_id: Uuid, storage_path: String) {
    tokio::spawn(async move {
        if let Err(e) = process(&state, session_id, org_id, &storage_path).await {
            tracing::error!("transcription failed ({session_id}): {e}");
            if let Some(pool) = state.pool.as_ref() {
                let _ = sqlx::query(
                    "UPDATE call_sessions SET transcript_status = 'failed' WHERE id = $1",
                )
                .bind(session_id)
                .execute(pool)
                .await;
            }
        }
    });
}

async fn process(
    state: &AppState,
    session_id: Uuid,
    org_id: Uuid,
    storage_path: &str,
) -> Result<(), String> {
    let pool = state.pool.as_ref().ok_or("no database")?;
    let recordings = state
        .recordings_storage
        .as_ref()
        .ok_or("no recordings storage")?;

    // Hand Deepgram a short-lived signed URL so it fetches the (large, video)
    // recording straight from storage — the server never downloads/buffers it.
    let media_url = recordings.create_signed_url(storage_path).await?;
    let result = deepgram::transcribe_url_diarized(&state.http, &state.config, &media_url).await?;

    let segments: Vec<Segment> = result
        .utterances
        .iter()
        .map(|u| Segment {
            speaker_id: u.speaker.to_string(),
            speaker_name: format!("Speaker {}", u.speaker + 1),
            text: u.text.clone(),
            start_ms: u.start_ms,
            end_ms: u.end_ms,
        })
        .collect();
    let source_language = result.language.unwrap_or_else(|| "en".to_string());
    let duration = result.duration_seconds.unwrap_or(0.0).round() as i64;
    let word_count: i64 = segments
        .iter()
        .map(|s| s.text.split_whitespace().count() as i64)
        .sum();
    let segments_json = serde_json::to_value(&segments).map_err(|e| e.to_string())?;

    // Exactly one transcript per session — replace any prior attempt.
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    sqlx::query("DELETE FROM transcripts WHERE session_id = $1")
        .bind(session_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    sqlx::query(
        "INSERT INTO transcripts
            (session_id, org_id, source_language, segments, duration_seconds, word_count, processed_at)
         VALUES ($1, $2, $3, $4, $5, $6, now())",
    )
    .bind(session_id)
    .bind(org_id)
    .bind(&source_language)
    .bind(&segments_json)
    .bind(duration as i32)
    .bind(word_count as i32)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    sqlx::query("UPDATE call_sessions SET transcript_status = 'ready' WHERE id = $1")
        .bind(session_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())?;

    // Charge transcription credits (best-effort; the transcript is already saved).
    let cost = credits::transcription_credits(duration);
    if cost > 0 {
        let _ = credits::deduct_org_credits(
            pool,
            org_id,
            cost,
            "transcription",
            Some(session_id),
            "diarized transcription",
        )
        .await;
    }
    Ok(())
}

// ---- Read / translate / export ----------------------------------------------

/// `GET /api/business/rooms/{session_id}/transcript` — transcript + status (member).
pub async fn get_transcript(
    State(state): State<AppState>,
    user: AuthUser,
    Path(session_id): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let (org_id, _) = require_call_role(pool, session_id, user.user_id, MEMBER).await?;

    let status: String =
        sqlx::query_scalar("SELECT transcript_status FROM call_sessions WHERE id = $1")
            .bind(session_id)
            .fetch_one(pool)
            .await
            .map_err(db_err)?;
    let row: Option<TranscriptRow> = sqlx::query_as(
        "SELECT source_language, segments, translations, duration_seconds, word_count
         FROM transcripts WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;

    audit::log_audit_event(
        pool,
        org_id,
        user.user_id,
        "transcript.view",
        "transcript",
        session_id,
        json!({}),
    );

    match row {
        Some(t) => {
            let translated: Vec<String> = t
                .translations
                .as_object()
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default();
            Ok(Json(json!({
                "status": status,
                "source_language": t.source_language,
                "segments": t.segments,
                "duration_seconds": t.duration_seconds,
                "word_count": t.word_count,
                "translated_languages": translated,
            }))
            .into_response())
        }
        None => Ok(Json(json!({ "status": status, "segments": [] })).into_response()),
    }
}

#[derive(Deserialize)]
pub struct TranslateReq {
    target_language: String,
}

/// `POST /api/business/rooms/{session_id}/transcript/translate` — on-demand,
/// cached translation (member). Cache hit costs 0 credits.
pub async fn translate(
    State(state): State<AppState>,
    user: AuthUser,
    Path(session_id): Path<Uuid>,
    Json(body): Json<TranslateReq>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let (org_id, _) = require_call_role(pool, session_id, user.user_id, MEMBER).await?;
    let target = valid_lang(&body.target_language)?;

    let t: TranscriptRow = sqlx::query_as(
        "SELECT source_language, segments, translations, duration_seconds, word_count
         FROM transcripts WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?
    .ok_or_else(|| not_found("transcript not ready"))?;

    // Original language → just flatten, no charge.
    if target == t.source_language {
        return Ok(Json(json!({
            "language": target,
            "text": flatten(&t.segments),
            "cached": true,
            "credits_deducted": 0,
        }))
        .into_response());
    }
    // Cache hit → no charge.
    if let Some(cached) = t.translations.get(&target).and_then(|v| v.as_str()) {
        return Ok(Json(json!({
            "language": target,
            "text": cached,
            "cached": true,
            "credits_deducted": 0,
        }))
        .into_response());
    }

    // Charge up front, then translate; refund if the translation call fails.
    let cost = credits::translation_credits(t.word_count.unwrap_or(0) as i64);
    match credits::deduct_org_credits(
        pool,
        org_id,
        cost,
        "translation",
        Some(session_id),
        "transcript translation",
    )
    .await
    .map_err(db_err)?
    {
        credits::OrgCharge::Insufficient { balance, required } => {
            return Err(insufficient(balance, required));
        }
        credits::OrgCharge::Charged { .. } => {}
    }

    let source_text = flatten(&t.segments);
    let map = state
        .translator
        .translate_fanout(
            &source_text,
            &t.source_language,
            std::slice::from_ref(&target),
            None,
        )
        .await;
    let translated = map.get(&target).cloned().unwrap_or_default();
    if translated.is_empty() {
        // Refund the charge — we produced nothing.
        let _ = credits::add_org_credits(pool, org_id, cost, "translation", "refund", None).await;
        return Err((StatusCode::BAD_GATEWAY, "translation failed").into_response());
    }

    sqlx::query(
        "UPDATE transcripts SET translations = jsonb_set(translations, $2::text[], to_jsonb($3::text), true)
         WHERE session_id = $1",
    )
    .bind(session_id)
    .bind(vec![target.clone()])
    .bind(&translated)
    .execute(pool)
    .await
    .map_err(db_err)?;

    Ok(Json(json!({
        "language": target,
        "text": translated,
        "cached": false,
        "credits_deducted": cost,
    }))
    .into_response())
}

#[derive(Deserialize, Default)]
pub struct ExportQuery {
    format: Option<String>,
    language: Option<String>,
}

/// `GET /api/business/rooms/{session_id}/transcript/export?format=txt|pdf&language=`
/// — download the transcript (member). TXT honours `language` (original or a
/// cached translation); PDF renders the original diarized transcript.
pub async fn export(
    State(state): State<AppState>,
    user: AuthUser,
    Path(session_id): Path<Uuid>,
    Query(q): Query<ExportQuery>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let (org_id, _) = require_call_role(pool, session_id, user.user_id, MEMBER).await?;

    let t: TranscriptRow = sqlx::query_as(
        "SELECT source_language, segments, translations, duration_seconds, word_count
         FROM transcripts WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?
    .ok_or_else(|| not_found("transcript not ready"))?;

    let format = q.format.as_deref().unwrap_or("txt");
    audit::log_audit_event(
        pool,
        org_id,
        user.user_id,
        "transcript.export",
        "transcript",
        session_id,
        json!({ "format": format }),
    );

    match format {
        "txt" => {
            let lang = q.language.as_deref().map(str::to_lowercase);
            let body = match lang {
                Some(l) if l != t.source_language => t
                    .translations
                    .get(&l)
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .ok_or_else(|| not_found("no translation for that language"))?,
                _ => flatten(&t.segments),
            };
            Ok((
                [
                    (
                        header::CONTENT_TYPE,
                        "text/plain; charset=utf-8".to_string(),
                    ),
                    (
                        header::CONTENT_DISPOSITION,
                        format!("attachment; filename=\"transcript-{session_id}.txt\""),
                    ),
                ],
                body,
            )
                .into_response())
        }
        "pdf" => {
            let meta: (String, DateTime<Utc>, Option<DateTime<Utc>>) = sqlx::query_as(
                "SELECT room, started_at, ended_at FROM call_sessions WHERE id = $1",
            )
            .bind(session_id)
            .fetch_one(pool)
            .await
            .map_err(db_err)?;
            let export = build_export(session_id, &meta, &t);
            let doc =
                pdf::build_pdf_doc(&export, chrono_tz::Tz::UTC, &t.source_language).to_string();
            let render = tokio::task::spawn_blocking(move || pdf::render_transcript_pdf(&doc))
                .await
                .map_err(|_| {
                    (StatusCode::INTERNAL_SERVER_ERROR, "pdf task failed").into_response()
                })?
                .map_err(|e| {
                    tracing::error!("transcript pdf render failed: {e}");
                    (StatusCode::INTERNAL_SERVER_ERROR, "pdf render failed").into_response()
                })?;
            Ok((
                [
                    (header::CONTENT_TYPE, "application/pdf".to_string()),
                    (
                        header::CONTENT_DISPOSITION,
                        format!("attachment; filename=\"transcript-{session_id}.pdf\""),
                    ),
                ],
                render.bytes,
            )
                .into_response())
        }
        _ => Err(bad_request("format must be txt or pdf")),
    }
}

// ---- helpers -----------------------------------------------------------------

fn parse_segments(segments: &Value) -> Vec<Segment> {
    serde_json::from_value(segments.clone()).unwrap_or_default()
}

/// Flatten diarized segments into `Speaker: text` lines.
fn flatten(segments: &Value) -> String {
    parse_segments(segments)
        .iter()
        .map(|s| format!("{}: {}", s.speaker_name, s.text))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build a `TranscriptExport` (PDF input) from the diarized transcript.
fn build_export(
    session_id: Uuid,
    meta: &(String, DateTime<Utc>, Option<DateTime<Utc>>),
    t: &TranscriptRow,
) -> TranscriptExport {
    let (room, started_at, ended_at) = (meta.0.clone(), meta.1, meta.2);
    let segments = parse_segments(&t.segments);

    let mut participants: Vec<ExportParticipant> = Vec::new();
    for s in &segments {
        if !participants.iter().any(|p| p.id == s.speaker_id) {
            participants.push(ExportParticipant {
                id: s.speaker_id.clone(),
                name: s.speaker_name.clone(),
                language: t.source_language.clone(),
            });
        }
    }

    let events: Vec<ExportEvent> = segments
        .iter()
        .map(|s| ExportEvent {
            kind: "speech".to_string(),
            ts: started_at + Duration::milliseconds(s.start_ms),
            speaker_id: s.speaker_id.clone(),
            speaker_name: s.speaker_name.clone(),
            lang: t.source_language.clone(),
            original: s.text.clone(),
            translations: std::collections::HashMap::new(),
        })
        .collect();

    TranscriptExport {
        session: ExportSession {
            id: session_id,
            room_name: room,
            started_at,
            ended_at,
            duration_seconds: t.duration_seconds.unwrap_or(0) as i64,
            participants,
        },
        events,
        bookmarks: Vec::new(),
        exported_at: Utc::now(),
    }
}

/// Validate a language code (lowercase letters / hyphen, ≤ 12 chars).
fn valid_lang(raw: &str) -> Result<String, Response> {
    let l = raw.trim().to_lowercase();
    if !l.is_empty() && l.len() <= 12 && l.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
        Ok(l)
    } else {
        Err(bad_request("invalid target_language"))
    }
}

fn insufficient(balance: i32, required: i32) -> Response {
    (
        StatusCode::PAYMENT_REQUIRED,
        Json(json!({
            "error": "insufficient_org_credits",
            "balance": balance,
            "required": required,
        })),
    )
        .into_response()
}
