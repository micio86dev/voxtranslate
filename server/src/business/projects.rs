//! Project CRUD (spec 0106). Projects group an org's calls. Delete is soft
//! (`archived_at`) so historic calls keep a valid project reference.

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
struct Project {
    id: Uuid,
    name: String,
    description: Option<String>,
    default_languages: Vec<String>,
    archived_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

#[derive(Deserialize)]
pub struct CreateProject {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    default_languages: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub struct PatchProject {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    default_languages: Option<Vec<String>>,
}

const SELECT_COLS: &str = "id, name, description, default_languages, archived_at, created_at";

fn valid_name(raw: &str) -> Option<&str> {
    let n = raw.trim();
    (!n.is_empty() && n.chars().count() <= 120).then_some(n)
}

/// `GET /api/business/organizations/{org_id}/projects` — active projects (member).
pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;
    let rows: Vec<Project> = sqlx::query_as(&format!(
        "SELECT {SELECT_COLS} FROM projects
         WHERE org_id = $1 AND archived_at IS NULL ORDER BY created_at DESC"
    ))
    .bind(org_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    Ok(Json(rows).into_response())
}

/// `POST /api/business/organizations/{org_id}/projects` — create (member).
pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Json(body): Json<CreateProject>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;
    let name =
        valid_name(&body.name).ok_or_else(|| bad_request("name is required (max 120 chars)"))?;

    let row: Project = sqlx::query_as(&format!(
        "INSERT INTO projects (org_id, name, description, default_languages, created_by)
         VALUES ($1, $2, $3, COALESCE($4::text[], '{{}}'), $5)
         RETURNING {SELECT_COLS}"
    ))
    .bind(org_id)
    .bind(name)
    .bind(body.description.as_deref())
    .bind(body.default_languages)
    .bind(user.user_id)
    .fetch_one(pool)
    .await
    .map_err(db_err)?;
    Ok(Json(row).into_response())
}

/// `GET /api/business/organizations/{org_id}/projects/{project_id}` (member).
pub async fn get(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, project_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;
    let row: Option<Project> = sqlx::query_as(&format!(
        "SELECT {SELECT_COLS} FROM projects WHERE id = $1 AND org_id = $2"
    ))
    .bind(project_id)
    .bind(org_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    let row = row.ok_or_else(|| not_found("project not found"))?;
    Ok(Json(row).into_response())
}

/// `PATCH /api/business/organizations/{org_id}/projects/{project_id}` (admin/owner).
pub async fn patch(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, project_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<PatchProject>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;
    let name = match &body.name {
        Some(n) => Some(
            valid_name(n)
                .ok_or_else(|| bad_request("invalid name"))?
                .to_string(),
        ),
        None => None,
    };

    let row: Option<Project> = sqlx::query_as(&format!(
        "UPDATE projects
         SET name = COALESCE($3, name),
             description = COALESCE($4, description),
             default_languages = COALESCE($5::text[], default_languages),
             updated_at = now()
         WHERE id = $1 AND org_id = $2
         RETURNING {SELECT_COLS}"
    ))
    .bind(project_id)
    .bind(org_id)
    .bind(name)
    .bind(body.description.as_deref())
    .bind(body.default_languages)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    let row = row.ok_or_else(|| not_found("project not found"))?;
    Ok(Json(row).into_response())
}

/// `DELETE /api/business/organizations/{org_id}/projects/{project_id}` — soft delete (admin/owner).
pub async fn delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, project_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;
    let res = sqlx::query(
        "UPDATE projects SET archived_at = now(), updated_at = now()
         WHERE id = $1 AND org_id = $2 AND archived_at IS NULL",
    )
    .bind(project_id)
    .bind(org_id)
    .execute(pool)
    .await
    .map_err(db_err)?;
    if res.rows_affected() == 0 {
        return Err(not_found("project not found"));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}
