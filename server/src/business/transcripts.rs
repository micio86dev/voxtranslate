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
use std::collections::{HashMap, HashSet};
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

    let mut segments: Vec<Segment> = result
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

    // Deepgram diarization labels voices by an integer cluster index ("Speaker
    // N"), with no idea who anyone is. Attribute each cluster to a real
    // participant by matching its utterances against the realtime
    // `transcript_events` captured during the call (which carry the actual
    // names). Clusters we can't confidently attribute keep their "Speaker N"
    // placeholder.
    let realtime = realtime_named_utterances(pool, session_id)
        .await
        .map_err(|e| e.to_string())?;
    let name_map = map_clusters_to_names(&segments, &realtime);
    for seg in &mut segments {
        if let Some(name) = name_map.get(&seg.speaker_id) {
            seg.speaker_name = name.clone();
        }
    }

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
    let transcript_id: Uuid = sqlx::query_scalar(
        "INSERT INTO transcripts
            (session_id, org_id, source_language, segments, duration_seconds, word_count, processed_at)
         VALUES ($1, $2, $3, $4, $5, $6, now())
         RETURNING id",
    )
    .bind(session_id)
    .bind(org_id)
    .bind(&source_language)
    .bind(&segments_json)
    .bind(duration as i32)
    .bind(word_count as i32)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    sqlx::query("UPDATE call_sessions SET transcript_status = 'ready' WHERE id = $1")
        .bind(session_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())?;

    // Index the transcript for semantic search (Business dashboard). Best-effort:
    // a failure must not fail transcription — search just omits this call until a
    // re-process or the `/internal/embeddings/backfill` run picks it up. No-op when
    // embeddings aren't configured. `project_id` scopes the result to its project.
    let project_id: Option<Uuid> =
        sqlx::query_scalar("SELECT project_id FROM call_sessions WHERE id = $1")
            .bind(session_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    if let Err(e) = embed_and_store(
        state,
        transcript_id,
        session_id,
        org_id,
        project_id,
        &segments,
    )
    .await
    {
        tracing::warn!("transcript embedding failed ({session_id}): {e}");
    }

    // Charge transcription credits (best-effort; the transcript is already saved).
    let cost = credits::transcription_credits(duration);
    if cost > 0 {
        let _ = credits::deduct_org_credits(
            pool,
            org_id,
            cost,
            "transcription",
            Some(session_id),
            None, // background job — no user actor
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
            let segments = segments_with_names(pool, session_id, &t.segments).await?;
            Ok(Json(json!({
                "status": status,
                "source": "recording",
                "source_language": t.source_language,
                "segments": segments,
                "duration_seconds": t.duration_seconds,
                "word_count": t.word_count,
                "translated_languages": translated,
            }))
            .into_response())
        }
        // No recording-derived transcript. Fall back to the realtime transcript
        // captured live during the call (`transcript_events`) so the dashboard shows
        // the same transcript the call app does — a business call that was never
        // cloud-recorded still has its live transcript, and the user expects to read
        // it here. `source: "live"` tells the UI to hide the recording-only tools
        // (translate/export/playback all read the `transcripts` row, which is absent).
        None => {
            let live = live_transcript(pool, session_id).await?;
            if live.segments.is_empty() {
                Ok(
                    Json(json!({ "status": status, "source": "live", "segments": [] }))
                        .into_response(),
                )
            } else {
                Ok(Json(json!({
                    "status": "ready",
                    "source": "live",
                    "source_language": live.source_language,
                    "segments": live.segments,
                    "duration_seconds": live.duration_seconds,
                    "word_count": live.word_count,
                    "translated_languages": [],
                }))
                .into_response())
            }
        }
    }
}

/// A transcript reconstructed from the realtime `transcript_events` of a call —
/// the fallback when no cloud recording was made (so no diarized `transcripts`
/// row exists). Only `speech` events become segments; chat is excluded.
struct LiveTranscript {
    source_language: String,
    segments: Vec<Segment>,
    duration_seconds: i32,
    word_count: i32,
}

async fn live_transcript(
    pool: &crate::db::Pool,
    session_id: Uuid,
) -> Result<LiveTranscript, Response> {
    let times: Option<(DateTime<Utc>, Option<DateTime<Utc>>)> =
        sqlx::query_as("SELECT started_at, ended_at FROM call_sessions WHERE id = $1")
            .bind(session_id)
            .fetch_optional(pool)
            .await
            .map_err(db_err)?;
    let (started_at, ended_at) = times.unwrap_or_else(|| (Utc::now(), None));

    let rows: Vec<(String, String, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT speaker_peer_id, speaker_name, original_text, ts
         FROM transcript_events
         WHERE session_id = $1 AND event_type = 'speech'
         ORDER BY ts",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;

    let source_language: String = sqlx::query_scalar(
        "SELECT lang FROM session_participants WHERE session_id = $1 ORDER BY joined_at LIMIT 1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?
    .unwrap_or_else(|| "en".to_string());

    let segments: Vec<Segment> = rows
        .into_iter()
        .map(|(speaker_id, speaker_name, text, ts)| {
            let start_ms = (ts - started_at).num_milliseconds().max(0);
            Segment {
                speaker_id,
                speaker_name,
                text,
                start_ms,
                end_ms: start_ms,
            }
        })
        .collect();
    let word_count: i32 = segments
        .iter()
        .map(|s| s.text.split_whitespace().count() as i32)
        .sum();
    let duration_seconds = (ended_at.unwrap_or(started_at) - started_at)
        .num_seconds()
        .clamp(0, i32::MAX as i64) as i32;

    Ok(LiveTranscript {
        source_language,
        segments,
        duration_seconds,
        word_count,
    })
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

    let segments = segments_with_names(pool, session_id, &t.segments).await?;

    // Original language → just flatten, no charge.
    if target == t.source_language {
        return Ok(Json(json!({
            "language": target,
            "text": flatten(&segments),
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
        Some(user.user_id),
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

    let source_text = flatten(&segments);
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
                _ => flatten(&segments_with_names(pool, session_id, &t.segments).await?),
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
            let segments = segments_with_names(pool, session_id, &t.segments).await?;
            let export = build_export(
                session_id,
                &meta,
                &t.source_language,
                t.duration_seconds.unwrap_or(0),
                &segments,
            );
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

// ---- Semantic-search embeddings ----------------------------------------------

/// Target words per embedding chunk. Diarized utterances are short; grouping
/// consecutive segments up to this size gives chunks coherent enough to embed
/// well while still precise enough to surface as a result snippet.
const CHUNK_TARGET_WORDS: usize = 250;

/// A contiguous run of transcript segments embedded as one unit.
pub struct Chunk {
    /// Joined plain text of the run — embedded and shown as the result snippet.
    pub content: String,
    /// First speaker in the run (a chunk may span speakers; this labels the snippet).
    pub speaker_name: String,
    pub start_ms: i64,
    pub end_ms: i64,
}

/// Group consecutive diarized segments into ~[`CHUNK_TARGET_WORDS`]-word chunks,
/// preserving order. Empty/whitespace segments are skipped; the final partial
/// chunk is always flushed.
pub fn chunk_segments(segments: &[Segment]) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut texts: Vec<&str> = Vec::new();
    let mut words = 0usize;
    let mut start_ms = 0i64;
    let mut end_ms = 0i64;
    let mut speaker = String::new();
    for seg in segments {
        let text = seg.text.trim();
        if text.is_empty() {
            continue;
        }
        if texts.is_empty() {
            start_ms = seg.start_ms;
            speaker = seg.speaker_name.clone();
        }
        texts.push(text);
        end_ms = seg.end_ms;
        words += text.split_whitespace().count();
        if words >= CHUNK_TARGET_WORDS {
            chunks.push(Chunk {
                content: texts.join(" "),
                speaker_name: std::mem::take(&mut speaker),
                start_ms,
                end_ms,
            });
            texts.clear();
            words = 0;
        }
    }
    if !texts.is_empty() {
        chunks.push(Chunk {
            content: texts.join(" "),
            speaker_name: speaker,
            start_ms,
            end_ms,
        });
    }
    chunks
}

/// Embed a transcript's chunks and (re)store them in `transcript_embeddings`.
/// Idempotent: deletes any prior rows for `transcript_id` first, so a re-process
/// or backfill is safe to repeat. No-op (returns 0) when embeddings aren't
/// configured. Shared by the post-transcription hook and the backfill endpoint.
pub async fn embed_and_store(
    state: &AppState,
    transcript_id: Uuid,
    session_id: Uuid,
    org_id: Uuid,
    project_id: Option<Uuid>,
    segments: &[Segment],
) -> Result<usize, String> {
    let Some(embedder) = state.embeddings.as_ref() else {
        return Ok(0);
    };
    let pool = state.pool.as_ref().ok_or("no database")?;
    let chunks = chunk_segments(segments);
    if chunks.is_empty() {
        return Ok(0);
    }
    let texts: Vec<String> = chunks.iter().map(|c| c.content.clone()).collect();
    let vectors = embedder.embed_batch(&texts).await?;
    if vectors.len() != chunks.len() {
        return Err("embedding count mismatch".to_string());
    }

    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    sqlx::query("DELETE FROM transcript_embeddings WHERE transcript_id = $1")
        .bind(transcript_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    for (i, (chunk, vec)) in chunks.iter().zip(vectors).enumerate() {
        sqlx::query(
            "INSERT INTO transcript_embeddings
                (transcript_id, session_id, org_id, project_id, chunk_index,
                 speaker_name, start_ms, end_ms, content, embedding)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(transcript_id)
        .bind(session_id)
        .bind(org_id)
        .bind(project_id)
        .bind(i as i32)
        .bind(&chunk.speaker_name)
        .bind(chunk.start_ms)
        .bind(chunk.end_ms)
        .bind(&chunk.content)
        .bind(pgvector::Vector::from(vec))
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    }
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(chunks.len())
}

// ---- helpers -----------------------------------------------------------------

fn parse_segments(segments: &Value) -> Vec<Segment> {
    serde_json::from_value(segments.clone()).unwrap_or_default()
}

/// Flatten diarized segments into `Speaker: text` lines.
fn flatten(segments: &[Segment]) -> String {
    segments
        .iter()
        .map(|s| format!("{}: {}", s.speaker_name, s.text))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build a `TranscriptExport` (PDF input) from the diarized transcript.
fn build_export(
    session_id: Uuid,
    meta: &(String, DateTime<Utc>, Option<DateTime<Utc>>),
    source_language: &str,
    duration_seconds: i32,
    segments: &[Segment],
) -> TranscriptExport {
    let (room, started_at, ended_at) = (meta.0.clone(), meta.1, meta.2);

    let mut participants: Vec<ExportParticipant> = Vec::new();
    for s in segments {
        if !participants.iter().any(|p| p.id == s.speaker_id) {
            participants.push(ExportParticipant {
                id: s.speaker_id.clone(),
                name: s.speaker_name.clone(),
                language: source_language.to_string(),
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
            lang: source_language.to_string(),
            original: s.text.clone(),
            translations: HashMap::new(),
        })
        .collect();

    TranscriptExport {
        session: ExportSession {
            id: session_id,
            room_name: room,
            started_at,
            ended_at,
            duration_seconds: duration_seconds as i64,
            participants,
        },
        events,
        bookmarks: Vec::new(),
        exported_at: Utc::now(),
    }
}

// ---- Real-name attribution for diarized clusters -----------------------------

/// How far a realtime event's timestamp may sit outside a diarized utterance's
/// window and still count as the same moment. Generous because the cloud
/// recording starts a beat after the call does (compositor + MediaRecorder
/// spin-up), so the two clocks share an origin only approximately.
const ATTRIBUTION_TIME_TOLERANCE_MS: i64 = 8_000;

/// A realtime speech event (from `transcript_events`), carrying the *real*
/// speaker name and the offset (ms from the call's `started_at`) at which it
/// was spoken — the evidence we attribute diarized clusters from.
struct NamedUtterance {
    name: String,
    text: String,
    offset_ms: i64,
}

/// `true` for the synthetic `"Speaker N"` labels diarization produces, i.e. the
/// ones we still want to replace with a real name.
fn is_placeholder_name(name: &str) -> bool {
    name.strip_prefix("Speaker ")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Content words of an utterance, normalized for matching: lowercased, split on
/// non-alphanumerics, single characters dropped (they carry no signal and would
/// match noisily). A set, since we score on *which* words are shared, not counts.
fn content_token_set(text: &str) -> HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= 2)
        .map(str::to_lowercase)
        .collect()
}

/// Map each diarized cluster id (`Segment.speaker_id`) to the real participant
/// name, inferred from the realtime transcript. For every diarized utterance we
/// find the realtime utterance it most likely *is* — most shared content words,
/// reinforced when the timestamps also line up — and let that vote for the
/// cluster's name. Each cluster takes its top-voted name.
///
/// Conservative by design: a vote needs either two shared words, or one shared
/// word *and* time agreement, so a lone incidental word ("yes", "okay") can't
/// mislabel a speaker. Clusters with no qualifying evidence are simply absent
/// from the map, so the caller keeps their "Speaker N" placeholder rather than
/// risk a confidently-wrong name.
fn map_clusters_to_names(
    segments: &[Segment],
    realtime: &[NamedUtterance],
) -> HashMap<String, String> {
    let rt: Vec<(&NamedUtterance, HashSet<String>)> = realtime
        .iter()
        .map(|u| (u, content_token_set(&u.text)))
        .collect();

    // votes[cluster][name] = summed confidence.
    let mut votes: HashMap<String, HashMap<String, i64>> = HashMap::new();
    for seg in segments {
        let seg_tokens = content_token_set(&seg.text);
        if seg_tokens.is_empty() {
            continue;
        }
        // The single best-matching realtime utterance for this segment.
        let mut best: Option<(&str, i64)> = None;
        for (u, u_tokens) in &rt {
            let shared = seg_tokens.intersection(u_tokens).count() as i64;
            if shared == 0 {
                continue;
            }
            let time_ok = u.offset_ms >= seg.start_ms - ATTRIBUTION_TIME_TOLERANCE_MS
                && u.offset_ms <= seg.end_ms + ATTRIBUTION_TIME_TOLERANCE_MS;
            if shared < 2 && !time_ok {
                continue; // one incidental word, clocks disagree → not enough.
            }
            let weight = shared * if time_ok { 2 } else { 1 };
            if best.is_none_or(|(_, w)| weight > w) {
                best = Some((u.name.as_str(), weight));
            }
        }
        if let Some((name, weight)) = best {
            *votes
                .entry(seg.speaker_id.clone())
                .or_default()
                .entry(name.to_string())
                .or_default() += weight;
        }
    }

    votes
        .into_iter()
        .filter_map(|(cluster, names)| {
            names
                .into_iter()
                .max_by_key(|(_, w)| *w)
                .map(|(name, _)| (cluster, name))
        })
        .collect()
}

/// Load the call's realtime speech events as [`NamedUtterance`]s (empty if the
/// call has no `started_at` yet or no realtime transcript).
async fn realtime_named_utterances(
    pool: &crate::db::Pool,
    session_id: Uuid,
) -> Result<Vec<NamedUtterance>, sqlx::Error> {
    let started_at: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT started_at FROM call_sessions WHERE id = $1")
            .bind(session_id)
            .fetch_optional(pool)
            .await?;
    let Some(started_at) = started_at else {
        return Ok(Vec::new());
    };
    let rows: Vec<(String, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT speaker_name, original_text, ts
         FROM transcript_events
         WHERE session_id = $1 AND event_type = 'speech'
         ORDER BY ts",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(name, text, ts)| NamedUtterance {
            name,
            text,
            offset_ms: (ts - started_at).num_milliseconds().max(0),
        })
        .collect())
}

/// Parse the stored diarized segments, attributing any that still carry a
/// `"Speaker N"` placeholder to a real name from the realtime transcript.
/// Transcripts produced after real-name attribution already carry real names,
/// so this is a no-op for them and skips the extra query — the placeholder check
/// gates it. This is what lets historical exports show real names without a data
/// migration.
async fn segments_with_names(
    pool: &crate::db::Pool,
    session_id: Uuid,
    segments_json: &Value,
) -> Result<Vec<Segment>, Response> {
    let mut segments = parse_segments(segments_json);
    if segments
        .iter()
        .any(|s| is_placeholder_name(&s.speaker_name))
    {
        let realtime = realtime_named_utterances(pool, session_id)
            .await
            .map_err(db_err)?;
        let name_map = map_clusters_to_names(&segments, &realtime);
        for seg in &mut segments {
            if is_placeholder_name(&seg.speaker_name) {
                if let Some(name) = name_map.get(&seg.speaker_id) {
                    seg.speaker_name = name.clone();
                }
            }
        }
    }
    Ok(segments)
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

#[cfg(test)]
mod chunk_tests {
    use super::*;

    fn seg(text: &str, start: i64, end: i64, speaker: &str) -> Segment {
        Segment {
            speaker_id: speaker.to_string(),
            speaker_name: speaker.to_string(),
            text: text.to_string(),
            start_ms: start,
            end_ms: end,
        }
    }

    #[test]
    fn short_transcript_is_one_chunk_with_full_span() {
        let segs = vec![
            seg("hello there", 0, 1000, "A"),
            seg("how are you", 1000, 2000, "B"),
        ];
        let chunks = chunk_segments(&segs);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].content, "hello there how are you");
        assert_eq!(chunks[0].speaker_name, "A"); // first speaker labels the chunk
        assert_eq!(chunks[0].start_ms, 0);
        assert_eq!(chunks[0].end_ms, 2000);
    }

    #[test]
    fn long_transcript_splits_on_word_budget() {
        // 6 segments × 50 words = 300 words > CHUNK_TARGET_WORDS (250) → splits.
        let fifty = "word ".repeat(50);
        let segs: Vec<Segment> = (0..6)
            .map(|i| seg(fifty.trim(), i * 1000, i * 1000 + 1000, "A"))
            .collect();
        let chunks = chunk_segments(&segs);
        assert!(chunks.len() >= 2, "expected a split, got {}", chunks.len());
        // Order/timestamps are preserved and contiguous.
        assert_eq!(chunks[0].start_ms, 0);
        assert!(chunks.last().unwrap().end_ms >= chunks[0].end_ms);
    }

    #[test]
    fn empty_segments_are_skipped() {
        let segs = vec![
            seg("   ", 0, 500, "A"),
            seg("real text", 500, 1500, "B"),
            seg("", 1500, 2000, "C"),
        ];
        let chunks = chunk_segments(&segs);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].content, "real text");
        assert_eq!(chunks[0].speaker_name, "B"); // first NON-empty segment
        assert_eq!(chunks[0].start_ms, 500);
    }

    #[test]
    fn all_empty_yields_no_chunks() {
        let segs = vec![seg("  ", 0, 100, "A"), seg("", 100, 200, "B")];
        assert!(chunk_segments(&segs).is_empty());
    }
}

#[cfg(test)]
mod naming_tests {
    use super::*;

    fn seg(speaker_id: &str, text: &str, start: i64, end: i64) -> Segment {
        Segment {
            speaker_id: speaker_id.to_string(),
            speaker_name: format!("Speaker {}", speaker_id.parse::<i64>().unwrap_or(0) + 1),
            text: text.to_string(),
            start_ms: start,
            end_ms: end,
        }
    }

    fn nu(name: &str, text: &str, offset_ms: i64) -> NamedUtterance {
        NamedUtterance {
            name: name.to_string(),
            text: text.to_string(),
            offset_ms,
        }
    }

    #[test]
    fn placeholder_detection() {
        assert!(is_placeholder_name("Speaker 1"));
        assert!(is_placeholder_name("Speaker 12"));
        assert!(!is_placeholder_name("Speaker"));
        assert!(!is_placeholder_name("Speaker "));
        assert!(!is_placeholder_name("Speaker A"));
        assert!(!is_placeholder_name("Alessandro"));
        assert!(!is_placeholder_name("Speaker 1 Smith"));
    }

    #[test]
    fn content_tokens_normalize_and_drop_singletons() {
        let toks = content_token_set("Hello, there! A B12 ok.");
        assert!(toks.contains("hello"));
        assert!(toks.contains("there"));
        assert!(toks.contains("b12"));
        assert!(toks.contains("ok"));
        assert!(!toks.contains("a")); // single char dropped
    }

    #[test]
    fn maps_two_clusters_to_real_names() {
        // Cluster "0" speaks the first line, cluster "1" the second; the
        // realtime transcript carries the real names for the same words.
        let segments = vec![
            seg(
                "0",
                "the quarterly revenue looks strong this period",
                0,
                4000,
            ),
            seg(
                "1",
                "agreed we should expand the sales team soon",
                5000,
                9000,
            ),
        ];
        let realtime = vec![
            nu(
                "Alessandro",
                "the quarterly revenue looks strong this period",
                200,
            ),
            nu("Maria", "agreed we should expand the sales team soon", 5200),
        ];
        let map = map_clusters_to_names(&segments, &realtime);
        assert_eq!(map.get("0"), Some(&"Alessandro".to_string()));
        assert_eq!(map.get("1"), Some(&"Maria".to_string()));
    }

    #[test]
    fn majority_vote_survives_a_noisy_utterance() {
        // Cluster "0" is Alessandro across three utterances; one short line
        // happens to share a word with Maria, but the majority still wins.
        let segments = vec![
            seg("0", "budget planning for the next fiscal year", 0, 3000),
            seg("0", "marketing spend needs careful review", 3000, 6000),
            seg("0", "team agreed", 6000, 7000),
        ];
        let realtime = vec![
            nu(
                "Alessandro",
                "budget planning for the next fiscal year",
                100,
            ),
            nu("Alessandro", "marketing spend needs careful review", 3100),
            nu("Maria", "the whole team agreed completely", 30000),
        ];
        let map = map_clusters_to_names(&segments, &realtime);
        assert_eq!(map.get("0"), Some(&"Alessandro".to_string()));
    }

    #[test]
    fn lone_incidental_word_without_time_agreement_does_not_map() {
        // The only overlap is a single common word, and the timestamps are far
        // apart — too weak to attribute, so the cluster is left unmapped.
        let segments = vec![seg("0", "please review the attached report", 0, 3000)];
        let realtime = vec![nu("Maria", "report submitted yesterday", 60_000)];
        let map = map_clusters_to_names(&segments, &realtime);
        assert!(map.get("0").is_none());
    }

    #[test]
    fn empty_realtime_maps_nothing() {
        let segments = vec![seg("0", "some words here", 0, 2000)];
        assert!(map_clusters_to_names(&segments, &[]).is_empty());
    }
}
