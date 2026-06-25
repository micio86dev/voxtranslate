//! Teams (spec 0106, Phase-2): named people-groups within an org, with flexible
//! many-to-many membership. A user can be in several teams; "moving" someone is
//! just add-to-one + remove-from-another. None of this touches call history
//! (attributed to sessions/projects), so reorganizing teams loses no data.
//!
//! Reads are MEMBER-gated; mutations are ADMIN-gated, mirroring projects/members.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use crate::business::{bad_request, db_err, not_found, require_pool, require_role, ADMIN, MEMBER};
use crate::middleware::AuthUser;
use crate::AppState;

#[derive(FromRow, Serialize)]
struct Team {
    id: Uuid,
    name: String,
    member_count: i64,
    created_at: DateTime<Utc>,
}

#[derive(FromRow, Serialize)]
struct TeamMemberRow {
    user_id: Uuid,
    name: String,
    email: String,
    avatar_url: Option<String>,
    joined_at: DateTime<Utc>,
}

#[derive(Deserialize)]
pub struct CreateTeam {
    name: String,
}

#[derive(Deserialize)]
pub struct PatchTeam {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
pub struct AddTeamMember {
    user_id: Uuid,
}

fn valid_name(raw: &str) -> Option<&str> {
    let n = raw.trim();
    (!n.is_empty() && n.chars().count() <= 120).then_some(n)
}

/// Resolve a team that belongs to this org, or 404. Keeps every team operation
/// tenant-scoped (a team_id from another org is invisible).
async fn team_in_org(pool: &crate::db::Pool, org_id: Uuid, team_id: Uuid) -> Result<(), Response> {
    let exists: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM teams WHERE id = $1 AND org_id = $2 AND archived_at IS NULL")
            .bind(team_id)
            .bind(org_id)
            .fetch_optional(pool)
            .await
            .map_err(db_err)?;
    exists
        .map(|_| ())
        .ok_or_else(|| not_found("team not found"))
}

/// `GET /api/business/organizations/{org_id}/teams` — teams + member counts (member).
pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;
    let rows: Vec<Team> = sqlx::query_as(
        "SELECT t.id, t.name,
                (SELECT count(*) FROM team_members tm WHERE tm.team_id = t.id)::bigint AS member_count,
                t.created_at
         FROM teams t
         WHERE t.org_id = $1 AND t.archived_at IS NULL
         ORDER BY t.created_at",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    Ok(Json(rows).into_response())
}

/// `POST /api/business/organizations/{org_id}/teams` — create (admin).
pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Json(body): Json<CreateTeam>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;
    let name =
        valid_name(&body.name).ok_or_else(|| bad_request("name is required (max 120 chars)"))?;

    let row: Team = sqlx::query_as(
        "WITH ins AS (
            INSERT INTO teams (org_id, name, created_by) VALUES ($1, $2, $3)
            RETURNING id, name, created_at
         )
         SELECT id, name, 0::bigint AS member_count, created_at FROM ins",
    )
    .bind(org_id)
    .bind(name)
    .bind(user.user_id)
    .fetch_one(pool)
    .await
    .map_err(db_err)?;
    super::audit::log_audit_event(
        pool,
        org_id,
        user.user_id,
        "team.create",
        "team",
        row.id,
        serde_json::json!({ "name": row.name }),
    );
    Ok((StatusCode::CREATED, Json(row)).into_response())
}

/// `PATCH /api/business/organizations/{org_id}/teams/{team_id}` — rename (admin).
pub async fn patch(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, team_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<PatchTeam>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;
    let name = match &body.name {
        Some(n) => valid_name(n).ok_or_else(|| bad_request("invalid name"))?,
        None => return Err(bad_request("nothing to update")),
    };
    let row: Option<Team> = sqlx::query_as(
        "WITH upd AS (
            UPDATE teams SET name = $3, updated_at = now()
            WHERE id = $1 AND org_id = $2
            RETURNING id, name, created_at
         )
         SELECT u.id, u.name,
                (SELECT count(*) FROM team_members tm WHERE tm.team_id = u.id)::bigint AS member_count,
                u.created_at
         FROM upd u",
    )
    .bind(team_id)
    .bind(org_id)
    .bind(name)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    let row = row.ok_or_else(|| not_found("team not found"))?;
    Ok(Json(row).into_response())
}

/// `DELETE /api/business/organizations/{org_id}/teams/{team_id}` (admin). Soft
/// delete — sets `archived_at` so an accidental delete is recoverable; the row and
/// its `team_members` stay in the DB. Call history is unaffected (not team-linked).
pub async fn delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, team_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;
    let row: Option<(String,)> = sqlx::query_as(
        "UPDATE teams SET archived_at = now(), updated_at = now()
         WHERE id = $1 AND org_id = $2 AND archived_at IS NULL RETURNING name",
    )
    .bind(team_id)
    .bind(org_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    let Some((name,)) = row else {
        return Err(not_found("team not found"));
    };
    super::audit::log_audit_event(
        pool,
        org_id,
        user.user_id,
        "team.delete",
        "team",
        team_id,
        serde_json::json!({ "name": name }),
    );
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `GET /api/business/organizations/{org_id}/teams/{team_id}/members` (member).
pub async fn list_members(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, team_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;
    team_in_org(pool, org_id, team_id).await?;
    let rows: Vec<TeamMemberRow> = sqlx::query_as(
        "SELECT tm.user_id, u.name, u.email, u.avatar_url, tm.joined_at
         FROM team_members tm
         JOIN users u ON u.id = tm.user_id
         WHERE tm.team_id = $1
         ORDER BY tm.joined_at",
    )
    .bind(team_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    Ok(Json(rows).into_response())
}

/// `POST /api/business/organizations/{org_id}/teams/{team_id}/members` — add an
/// existing org member to the team (admin). Idempotent; 400 if the target user is
/// not a member of this org.
pub async fn add_member(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, team_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<AddTeamMember>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;
    team_in_org(pool, org_id, team_id).await?;

    // Only org members can be on a team.
    let is_member: Option<String> = sqlx::query_scalar("SELECT get_user_org_role($1, $2)")
        .bind(org_id)
        .bind(body.user_id)
        .fetch_one(pool)
        .await
        .map_err(db_err)?;
    if is_member.is_none() {
        return Err(bad_request("user is not a member of this organization"));
    }

    sqlx::query(
        "INSERT INTO team_members (team_id, user_id, added_by)
         VALUES ($1, $2, $3)
         ON CONFLICT (team_id, user_id) DO NOTHING",
    )
    .bind(team_id)
    .bind(body.user_id)
    .bind(user.user_id)
    .execute(pool)
    .await
    .map_err(db_err)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `DELETE /api/business/organizations/{org_id}/teams/{team_id}/members/{user_id}`
/// — remove from the team (admin). Idempotent (204 even if absent).
pub async fn remove_member(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, team_id, target)): Path<(Uuid, Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;
    team_in_org(pool, org_id, team_id).await?;
    sqlx::query("DELETE FROM team_members WHERE team_id = $1 AND user_id = $2")
        .bind(team_id)
        .bind(target)
        .execute(pool)
        .await
        .map_err(db_err)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
