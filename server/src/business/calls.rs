//! Room→org binding and call history (spec 0106).
//!
//! Binding a room code to an org/project (before the call) is what makes a call a
//! "business call": `TranscriptService::session_started` reads
//! `room_business_bindings` when it materializes the `call_sessions` row.

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::FromRow;
use uuid::Uuid;

use crate::business::{bad_request, db_err, require_pool, require_role, MEMBER};
use crate::invite::sanitize_room;
use crate::middleware::AuthUser;
use crate::AppState;

#[derive(Deserialize)]
pub struct BindRoom {
    org_id: Uuid,
    #[serde(default)]
    project_id: Option<Uuid>,
    #[serde(default)]
    cloud_recording_enabled: Option<bool>,
}

#[derive(FromRow, Serialize)]
struct RoomRow {
    id: Uuid,
    room: String,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    project_id: Option<Uuid>,
    transcript_status: String,
    has_recording: bool,
}

#[derive(Deserialize, Default)]
pub struct HistoryQuery {
    project_id: Option<Uuid>,
    page: Option<i64>,
    limit: Option<i64>,
    from: Option<String>,
    to: Option<String>,
}

/// `PATCH /api/rooms/{room}/business` — bind a room to an org/project + recording
/// intent before the call (member of the target org).
pub async fn bind(
    State(state): State<AppState>,
    user: AuthUser,
    Path(room): Path<String>,
    Json(body): Json<BindRoom>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, body.org_id, user.user_id, MEMBER).await?;
    let room = sanitize_room(&room).ok_or_else(|| bad_request("invalid room code"))?;

    if let Some(project_id) = body.project_id {
        let ok: Option<bool> =
            sqlx::query_scalar("SELECT true FROM projects WHERE id = $1 AND org_id = $2")
                .bind(project_id)
                .bind(body.org_id)
                .fetch_optional(pool)
                .await
                .map_err(db_err)?;
        if ok.is_none() {
            return Err(bad_request("project does not belong to this org"));
        }
    }

    sqlx::query(
        "INSERT INTO room_business_bindings
            (room, org_id, project_id, cloud_recording_enabled, created_by)
         VALUES ($1, $2, $3, COALESCE($4, FALSE), $5)
         ON CONFLICT (room) DO UPDATE SET
            org_id = EXCLUDED.org_id,
            project_id = EXCLUDED.project_id,
            cloud_recording_enabled = EXCLUDED.cloud_recording_enabled,
            updated_at = now()",
    )
    .bind(&room)
    .bind(body.org_id)
    .bind(body.project_id)
    .bind(body.cloud_recording_enabled)
    .bind(user.user_id)
    .execute(pool)
    .await
    .map_err(db_err)?;

    Ok(Json(json!({
        "room": room,
        "org_id": body.org_id,
        "project_id": body.project_id,
        "cloud_recording_enabled": body.cloud_recording_enabled.unwrap_or(false),
    }))
    .into_response())
}

/// `GET /api/business/organizations/{org_id}/rooms` — paginated call history (member).
pub async fn list_org_rooms(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Query(q): Query<HistoryQuery>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;
    history(pool, org_id, &q).await
}

/// `GET /api/business/organizations/{org_id}/projects/{project_id}/rooms` (member).
pub async fn list_project_rooms(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, project_id)): Path<(Uuid, Uuid)>,
    Query(mut q): Query<HistoryQuery>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;
    q.project_id = Some(project_id);
    history(pool, org_id, &q).await
}

async fn history(
    pool: &crate::db::Pool,
    org_id: Uuid,
    q: &HistoryQuery,
) -> Result<Response, Response> {
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let page = q.page.unwrap_or(1).max(1);
    let offset = (page - 1) * limit;
    let from = parse_ts(&q.from)?;
    let to = parse_ts(&q.to)?;

    let rows: Vec<RoomRow> = sqlx::query_as(
        "SELECT id, room, started_at, ended_at, project_id, transcript_status,
                (recording_storage_path IS NOT NULL) AS has_recording
         FROM call_sessions
         WHERE org_id = $1
           AND ($2::uuid IS NULL OR project_id = $2)
           AND ($3::timestamptz IS NULL OR started_at >= $3)
           AND ($4::timestamptz IS NULL OR started_at <= $4)
         ORDER BY started_at DESC
         LIMIT $5 OFFSET $6",
    )
    .bind(org_id)
    .bind(q.project_id)
    .bind(from)
    .bind(to)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;

    Ok(Json(json!({ "rooms": rows, "page": page, "limit": limit })).into_response())
}

fn parse_ts(raw: &Option<String>) -> Result<Option<DateTime<Utc>>, Response> {
    match raw.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => DateTime::parse_from_rfc3339(s)
            .map(|d| Some(d.with_timezone(&Utc)))
            .map_err(|_| bad_request("from/to must be RFC3339 timestamps")),
    }
}
