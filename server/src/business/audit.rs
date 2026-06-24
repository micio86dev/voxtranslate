//! Compliance audit trail (spec 0106). Writes to `audit_logs`, but only for orgs
//! with `settings.compliance_mode = true`. Fire-and-forget so it never blocks or
//! fails the request that triggered it.

use uuid::Uuid;

use crate::db::Pool;

/// Record an audit event, asynchronously and best-effort. No-ops unless the org
/// is in compliance mode. Errors are logged, never surfaced to the caller.
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
        // Only orgs that opted into compliance mode keep an audit trail.
        let compliance: Option<bool> = sqlx::query_scalar(
            "SELECT (settings->>'compliance_mode')::boolean FROM organizations WHERE id = $1",
        )
        .bind(org_id)
        .fetch_optional(&pool)
        .await
        .ok()
        .flatten();
        if compliance != Some(true) {
            return;
        }
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
