//! Cloud recording (spec 0106): the browser uploads the finished call **video**
//! DIRECTLY to the private `recordings` bucket via a one-shot signed upload URL —
//! the server never proxies the bytes (a long call is ~1 GB). `complete` then
//! records the object path, charges the org pool, and kicks off transcription
//! (Deepgram fetches the recording by signed URL). Plus a signed playback URL.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::business::{
    audit, bad_request, credits, db_err, forbidden, not_found, require_call_role, require_pool,
    transcripts, MEMBER,
};
use crate::middleware::AuthUser;
use crate::storage::SupabaseStorage;
use crate::AppState;

/// `recordings_storage` or a 503 — recording is dormant unless SUPABASE_* is set.
fn store(state: &AppState) -> Result<&SupabaseStorage, Response> {
    state.recordings_storage.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "recording storage not configured",
        )
            .into_response()
    })
}

/// `POST /api/business/rooms/{session_id}/recording/upload-url` — issue a one-shot
/// signed URL the browser PUTs the recording straight to (member of the call's
/// org). Only for calls that actually have cloud recording enabled — which is set
/// exclusively by the subscription-gated room binding, so this is also where the
/// "recording is a paid feature" rule is enforced for the upload path.
pub async fn upload_url(
    State(state): State<AppState>,
    user: AuthUser,
    Path(session_id): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let (org_id, _) = require_call_role(pool, session_id, user.user_id, MEMBER).await?;
    let recordings = store(&state)?;

    // Authorize against the call's frozen flag OR the room's *current* binding.
    // `session_started` snapshots `cloud_recording_enabled` into `call_sessions` once
    // (ON CONFLICT DO NOTHING) and the session id is stable while the room stays
    // occupied — so a call that materialized before recording was turned on would
    // otherwise be permanently un-recordable even after the host enables it. Falling
    // back to the live binding fixes that without weakening the paid-feature gate:
    // the binding can only be set to true by `bind`, which already requires an active
    // subscription. The LEFT JOIN keeps consumer rooms (no binding) unchanged.
    let enabled: Option<bool> = sqlx::query_scalar(
        "SELECT COALESCE(cs.cloud_recording_enabled, FALSE)
             OR COALESCE(rb.cloud_recording_enabled, FALSE)
         FROM call_sessions cs
         LEFT JOIN room_business_bindings rb ON rb.room = cs.room
         WHERE cs.id = $1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    if enabled != Some(true) {
        return Err(forbidden("cloud recording is not enabled for this call"));
    }

    let object_path = format!("{org_id}/{session_id}/{}.webm", Uuid::new_v4());
    let url = recordings
        .create_signed_upload_url(&object_path)
        .await
        .map_err(|e| {
            tracing::error!("recording sign-upload failed: {e}");
            (StatusCode::BAD_GATEWAY, "could not create upload url").into_response()
        })?;
    Ok(Json(json!({ "upload_url": url, "object_path": object_path })).into_response())
}

#[derive(Deserialize)]
pub struct CompleteRecording {
    /// The exact path returned by `upload-url` (validated to this org+call below).
    object_path: String,
    #[serde(default)]
    duration_seconds: i64,
}

/// `POST /api/business/rooms/{session_id}/recording/complete` — after the direct
/// upload, record the object path, charge the org (1 credit/min, round up), and
/// enqueue transcription.
pub async fn complete(
    State(state): State<AppState>,
    user: AuthUser,
    Path(session_id): Path<Uuid>,
    Json(body): Json<CompleteRecording>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let (org_id, _) = require_call_role(pool, session_id, user.user_id, MEMBER).await?;

    // The path must be exactly the namespace we issued for THIS org + call — never
    // a client-chosen path into another tenant's space.
    let prefix = format!("{org_id}/{session_id}/");
    if !body.object_path.starts_with(&prefix) || body.object_path.contains("..") {
        return Err(bad_request("invalid recording path"));
    }
    let object_path = body.object_path;
    let duration_seconds = body.duration_seconds.max(0);

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
    // transcribe (revert the status), and tell the caller to top up.
    let cost = credits::recording_credits(duration_seconds);
    match credits::deduct_org_credits(
        pool,
        org_id,
        cost,
        "recording",
        Some(session_id),
        Some(user.user_id),
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
    let recordings = store(&state)?;

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
