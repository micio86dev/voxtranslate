//! VoxTranslate for Business — org workspace API (spec 0106).
//!
//! Phase 2 PR-A: organizations, members, invites, and projects under
//! `/api/business/...`. Authorization is enforced here in the API layer (the
//! server connects as the RLS-exempt owning role; per-user DB RLS does not
//! apply) via [`require_role`], backed by the `get_user_org_role()` SQL helper
//! from migration 016. Org credits and consumer credits never mix.

// Handlers and guards return `Result<_, Response>` so `?` can short-circuit with a
// status code. `Response` is a large Err variant, but that's the whole point here
// (it carries the rejection), and the cascade covers the submodules too.
#![allow(clippy::result_large_err)]

pub mod audit;
pub mod members;
pub mod organizations;
pub mod projects;
pub mod routes;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use uuid::Uuid;

use crate::db::Pool;
use crate::AppState;

/// Role ranks for authorization checks — higher is more privileged.
pub const MEMBER: u8 = 1;
pub const ADMIN: u8 = 2;
pub const OWNER: u8 = 3;

/// Numeric rank of a role string; unknown / `guest` is the floor.
pub fn role_rank(role: &str) -> u8 {
    match role {
        "owner" => OWNER,
        "admin" => ADMIN,
        "member" => MEMBER,
        _ => 0,
    }
}

/// The DB pool, or 503 when auth/billing (and thus the database) isn't configured.
pub fn require_pool(state: &AppState) -> Result<&Pool, Response> {
    state.pool.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "business features not configured",
        )
            .into_response()
    })
}

/// Resolve the caller's role in the org and enforce a minimum rank. Non-members
/// get 404 (so org existence isn't leaked to outsiders); members below the
/// required rank get 403. Returns the caller's role on success.
pub async fn require_role(
    pool: &Pool,
    org_id: Uuid,
    user_id: Uuid,
    min_rank: u8,
) -> Result<String, Response> {
    let role: Option<String> = sqlx::query_scalar("SELECT get_user_org_role($1, $2)")
        .bind(org_id)
        .bind(user_id)
        .fetch_one(pool)
        .await
        .map_err(db_err)?;
    match role {
        None => Err(not_found("organization not found")),
        Some(r) if role_rank(&r) >= min_rank => Ok(r),
        Some(_) => Err(forbidden("insufficient role")),
    }
}

pub fn bad_request(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, msg.to_string()).into_response()
}

pub fn forbidden(msg: &str) -> Response {
    (StatusCode::FORBIDDEN, msg.to_string()).into_response()
}

pub fn not_found(msg: &str) -> Response {
    (StatusCode::NOT_FOUND, msg.to_string()).into_response()
}

pub fn conflict(msg: &str) -> Response {
    (StatusCode::CONFLICT, msg.to_string()).into_response()
}

/// Map a database error to a logged 500 (details never reach the client).
pub fn db_err(e: sqlx::Error) -> Response {
    tracing::error!("business db error: {e}");
    (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
}

/// True when the error is a Postgres unique-violation (SQLSTATE 23505).
pub fn is_unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23505")
}
