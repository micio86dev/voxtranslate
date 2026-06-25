//! Activity / audit trail. Records who did what across the org's data so admins can
//! trace accidental damage. Writes are fire-and-forget (never block or fail the
//! triggering request); reads are admin/owner-gated and scoped to the caller's org
//! (a B2B admin sees only their own org's activity — never another org's).

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use crate::business::{db_err, require_pool, require_role, ADMIN};
use crate::db::Pool;
use crate::middleware::AuthUser;
use crate::AppState;

/// Record an audit event, asynchronously and best-effort. Always logged now (not
/// just in compliance mode) so every org keeps an activity history. Errors are
/// logged, never surfaced to the caller.
pub fn log_audit_event(
    pool: &Pool,
    org_id: Uuid,
    actor_id: Uuid,
    action: &str,
    resource_type: &str,
    resource_id: Uuid,
    metadata: serde_json::Value,
) {
    let pool = pool.clone();
    let action = action.to_string();
    let resource_type = resource_type.to_string();
    tokio::spawn(async move {
        if let Err(e) = sqlx::query(
            "INSERT INTO audit_logs (org_id, actor_id, action, resource_type, resource_id, metadata)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(org_id)
        .bind(actor_id)
        .bind(&action)
        .bind(&resource_type)
        .bind(resource_id)
        .bind(metadata)
        .execute(&pool)
        .await
        {
            tracing::error!("audit_logs insert failed: {e}");
        }
    });
}

#[derive(FromRow, Serialize)]
struct AuditRow {
    id: Uuid,
    actor_id: Option<Uuid>,
    actor_name: Option<String>,
    actor_email: Option<String>,
    action: String,
    resource_type: String,
    resource_id: Uuid,
    metadata: serde_json::Value,
    created_at: DateTime<Utc>,
}

#[derive(Deserialize, Default)]
pub struct AuditQuery {
    action: Option<String>,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    /// Free-text search over action / resource type / actor name / actor email.
    q: Option<String>,
    page: Option<i64>,
    limit: Option<i64>,
}

/// `GET /api/business/organizations/{org_id}/audit` — the org's activity log,
/// filterable + searchable. Admin/owner only; scoped to this org alone.
pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Query(q): Query<AuditQuery>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;

    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let page = q.page.unwrap_or(0).max(0);
    let offset = page * limit;
    let action = q.action.filter(|s| !s.trim().is_empty());
    let search =
        q.q.map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(|s| format!("%{s}%"));

    let rows: Vec<AuditRow> = sqlx::query_as(
        "SELECT a.id, a.actor_id, u.name AS actor_name, u.email AS actor_email,
                a.action, a.resource_type, a.resource_id, a.metadata, a.created_at
         FROM audit_logs a
         LEFT JOIN users u ON u.id = a.actor_id
         WHERE a.org_id = $1
           AND ($2::text IS NULL OR a.action = $2)
           AND ($3::timestamptz IS NULL OR a.created_at >= $3)
           AND ($4::timestamptz IS NULL OR a.created_at <= $4)
           AND ($5::text IS NULL OR a.action ILIKE $5 OR a.resource_type ILIKE $5
                OR u.name ILIKE $5 OR u.email ILIKE $5)
         ORDER BY a.created_at DESC
         LIMIT $6 OFFSET $7",
    )
    .bind(org_id)
    .bind(action.as_deref())
    .bind(q.from)
    .bind(q.to)
    .bind(search.as_deref())
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;

    Ok(Json(serde_json::json!({ "entries": rows, "page": page, "limit": limit })).into_response())
}
