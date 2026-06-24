//! Cloud recording (spec 0106): upload a finished call's audio to the private
//! `recordings` bucket, charge the org pool, and kick off transcription; plus a
//! signed playback URL. The `session_id` path param is the persistent call id.

use axum::extract::{Multipart, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use uuid::Uuid;

use crate::business::{
    audit, bad_request, credits, db_err, not_found, require_call_role, require_pool, transcripts,
    MEMBER,
};
use crate::middleware::AuthUser;
use crate::AppState;

/// Body cap for a recording upload (≈200 MiB of Opus/WebM is hours of audio).
pub const RECORDING_MAX_BYTES: usize = 200 * 1024 * 1024;

/// `POST /api/business/rooms/{session_id}/recording/complete` — multipart upload
/// of the call audio (member). Stores it privately, deducts org credits
/// (1/min, round up), and enqueues transcription.
pub async fn complete(
    State(state): State<AppState>,
    user: AuthUser,
    Path(session_id): Path<Uuid>,
    mut multipart: Multipart,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let (org_id, _) = require_call_role(pool, session_id, user.user_id, MEMBER).await?;
    let recordings = state.recordings_storage.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "recording storage not configured",
        )
            .into_response()
    })?;

    let mut duration_seconds: i64 = 0;
    let mut bytes: Option<Vec<u8>> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| bad_request("malformed upload"))?
    {
        match field.name() {
            Some("duration_seconds") => {
                duration_seconds = field
                    .text()
                    .await
                    .ok()
                    .and_then(|s| s.trim().parse().ok())
                    .unwrap_or(0);
            }
            Some("file") => {
                bytes = field.bytes().await.ok().map(|b| b.to_vec());
            }
            _ => {}
        }
    }
    let bytes = bytes.ok_or_else(|| bad_request("missing audio file"))?;
    if bytes.is_empty() {
        return Err(bad_request("empty audio file"));
    }

    let object_path = format!("{org_id}/{session_id}/{}.webm", Uuid::new_v4());
    recordings
        .upload(&object_path, bytes, "audio/webm")
        .await
        .map_err(|e| {
            tracing::error!("recording upload failed: {e}");
            (StatusCode::BAD_GATEWAY, "recording upload failed").into_response()
        })?;

    sqlx::query(
        "UPDATE call_sessions
         SET recording_storage_path = $2, transcript_status = 'processing'
         WHERE id = $1",
    )
    .bind(session_id)
    .bind(&object_path)
    .execute(pool)
    .await
    .map_err(db_err)?;

    // Charge for the recording. If the org can't cover it, keep the file but don't
    // transcribe (and revert the status), and tell the caller to top up.
    let cost = credits::recording_credits(duration_seconds);
    match credits::deduct_org_credits(
        pool,
        org_id,
        cost,
        "recording",
        Some(session_id),
        "cloud recording",
    )
    .await
    .map_err(db_err)?
    {
        credits::OrgCharge::Insufficient { balance, required } => {
            sqlx::query("UPDATE call_sessions SET transcript_status = 'none' WHERE id = $1")
                .bind(session_id)
                .execute(pool)
                .await
                .map_err(db_err)?;
            return Err((
                StatusCode::PAYMENT_REQUIRED,
                Json(json!({
                    "error": "insufficient_org_credits",
                    "balance": balance,
                    "required": required,
                })),
            )
                .into_response());
        }
        credits::OrgCharge::Charged { .. } => {}
    }

    transcripts::spawn_transcription(state.clone(), session_id, org_id, object_path.clone());

    Ok(Json(json!({
        "recording_path": object_path,
        "credits_deducted": cost,
        "transcript_status": "processing",
    }))
    .into_response())
}

/// `GET /api/business/rooms/{session_id}/recording/url` — 1h signed playback URL (member).
pub async fn url(
    State(state): State<AppState>,
    user: AuthUser,
    Path(session_id): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let (org_id, _) = require_call_role(pool, session_id, user.user_id, MEMBER).await?;
    let recordings = state.recordings_storage.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "recording storage not configured",
        )
            .into_response()
    })?;

    let path: Option<String> =
        sqlx::query_scalar("SELECT recording_storage_path FROM call_sessions WHERE id = $1")
            .bind(session_id)
            .fetch_one(pool)
            .await
            .map_err(db_err)?;
    let path = path.ok_or_else(|| not_found("no recording for this call"))?;

    let signed = recordings.create_signed_url(&path).await.map_err(|e| {
        tracing::error!("recording sign failed: {e}");
        (StatusCode::BAD_GATEWAY, "could not sign recording").into_response()
    })?;

    audit::log_audit_event(
        pool,
        org_id,
        user.user_id,
        "recording.play",
        "recording",
        session_id,
        json!({}),
    );

    Ok(Json(json!({ "url": signed, "expires_in": 3600 })).into_response())
}
