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

use crate::business::{bad_request, db_err, forbidden, require_pool, require_role, MEMBER};
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
    project_name: Option<String>,
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
    /// Comma-separated participant user-ids. OR filter: keep calls that included
    /// ANY of them. Empty/garbage ⇒ no member filter.
    member_ids: Option<String>,
}

/// Admins/owners see every org call; plain members are scoped to their own.
fn is_org_admin(role: &str) -> bool {
    matches!(role, "admin" | "owner")
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

    // Cloud recording is a paid Business/Enterprise feature: require a live (active
    // and unlapsed) subscription on the target org. The pre-join UI gates this too;
    // this is the server-side enforcement so the gate can't be bypassed. Using the
    // shared date-aware check means an admin-gifted subscription stops unlocking
    // recording the moment its gifted month ends.
    if body.cloud_recording_enabled == Some(true)
        && !crate::business::credits::org_subscription_active(pool, body.org_id)
            .await
            .map_err(db_err)?
    {
        return Err(forbidden(
            "cloud recording requires an active Business or Enterprise subscription",
        ));
    }

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

/// `GET /api/rooms/{room}/business` — the room's existing org/project binding, so the
/// pre-join UI can pre-select them (e.g. when joining a scheduled meeting via link)
/// instead of clobbering the project with an empty selection on connect. 404 when the
/// room isn't bound or the caller isn't a member of the bound org.
pub async fn get_binding(
    State(state): State<AppState>,
    user: AuthUser,
    Path(room): Path<String>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let room = sanitize_room(&room).ok_or_else(|| bad_request("invalid room code"))?;
    let row: Option<(Uuid, Option<Uuid>)> =
        sqlx::query_as("SELECT org_id, project_id FROM room_business_bindings WHERE room = $1")
            .bind(&room)
            .fetch_optional(pool)
            .await
            .map_err(db_err)?;
    // Unbound room (the normal case for any standard room), or the caller isn't a
    // member of the bound org → return an explicit "unbound" 200, not 404/403. The
    // pre-join UI probes this for EVERY room, so a 404 here logged a scary console
    // error on every non-business call; the null payload also avoids leaking a
    // binding's existence to non-members.
    let unbound = || Json(json!({ "org_id": null, "project_id": null })).into_response();
    let Some((org_id, project_id)) = row else {
        return Ok(unbound());
    };
    if require_role(pool, org_id, user.user_id, MEMBER)
        .await
        .is_err()
    {
        return Ok(unbound());
    }
    Ok(Json(json!({ "org_id": org_id, "project_id": project_id })).into_response())
}

/// `GET /api/business/organizations/{org_id}/rooms` — paginated call history (member).
pub async fn list_org_rooms(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Query(q): Query<HistoryQuery>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let role = require_role(pool, org_id, user.user_id, MEMBER).await?;
    history(pool, org_id, user.user_id, is_org_admin(&role), &q).await
}

/// `GET /api/business/organizations/{org_id}/projects/{project_id}/rooms` (member).
pub async fn list_project_rooms(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, project_id)): Path<(Uuid, Uuid)>,
    Query(mut q): Query<HistoryQuery>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let role = require_role(pool, org_id, user.user_id, MEMBER).await?;
    q.project_id = Some(project_id);
    history(pool, org_id, user.user_id, is_org_admin(&role), &q).await
}

async fn history(
    pool: &crate::db::Pool,
    org_id: Uuid,
    viewer_id: Uuid,
    is_admin: bool,
    q: &HistoryQuery,
) -> Result<Response, Response> {
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let page = q.page.unwrap_or(1).max(1);
    let offset = (page - 1) * limit;
    let from = parse_ts(&q.from)?;
    let to = parse_ts(&q.to)?;
    // Comma-separated participant ids → OR filter; drop unparseable/empty ⇒ no filter.
    let member_ids: Option<Vec<Uuid>> = q
        .member_ids
        .as_deref()
        .map(|s| {
            s.split(',')
                .filter_map(|x| Uuid::parse_str(x.trim()).ok())
                .collect::<Vec<Uuid>>()
        })
        .filter(|v| !v.is_empty());

    let rows: Vec<RoomRow> = sqlx::query_as(
        // `transcript_status` tracks the recording-derived transcript. A call that
        // was never cloud-recorded still has a realtime transcript (the detail view
        // falls back to it), so report 'live' here too — otherwise the list shows
        // '—' for a call whose transcript opens fine, which reads as a bug.
        "SELECT cs.id, cs.room, cs.started_at, cs.ended_at, cs.project_id,
                p.name AS project_name,
                CASE
                    WHEN cs.transcript_status IN ('ready', 'processing', 'failed')
                        THEN cs.transcript_status
                    WHEN EXISTS (SELECT 1 FROM transcript_events te
                                 WHERE te.session_id = cs.id AND te.event_type = 'speech')
                        THEN 'live'
                    ELSE cs.transcript_status
                END AS transcript_status,
                (cs.recording_storage_path IS NOT NULL) AS has_recording
         FROM call_sessions cs
         LEFT JOIN projects p ON p.id = cs.project_id
         WHERE cs.org_id = $1 AND cs.kind = 'call'
           AND ($2::uuid IS NULL OR cs.project_id = $2)
           AND ($3::timestamptz IS NULL OR cs.started_at >= $3)
           AND ($4::timestamptz IS NULL OR cs.started_at <= $4)
           -- Non-admins only see calls they participated in.
           AND ($7 OR EXISTS (
                 SELECT 1 FROM session_participants sp
                 WHERE sp.session_id = cs.id AND sp.user_id = $8))
           -- Optional participant filter (any of the selected members).
           AND ($9::uuid[] IS NULL OR EXISTS (
                 SELECT 1 FROM session_participants spm
                 WHERE spm.session_id = cs.id AND spm.user_id = ANY($9)))
         ORDER BY cs.started_at DESC
         LIMIT $5 OFFSET $6",
    )
    .bind(org_id)
    .bind(q.project_id)
    .bind(from)
    .bind(to)
    .bind(limit)
    .bind(offset)
    .bind(is_admin)
    .bind(viewer_id)
    .bind(member_ids.as_deref())
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
