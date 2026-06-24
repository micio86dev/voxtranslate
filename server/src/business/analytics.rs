//! Org analytics summary (spec 0106, Phase-1 dashboard KPIs + charts).
//!
//! One admin-gated endpoint that aggregates the data the dashboard already owns —
//! `call_sessions` (volume/minutes/transcripts/recordings) and the org credit
//! ledger (spend by type) — into KPI totals plus small time/category series the
//! dashboard renders without a charting dependency.

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::FromRow;
use uuid::Uuid;

use crate::business::{db_err, require_pool, require_role, ADMIN};
use crate::middleware::AuthUser;
use crate::AppState;

#[derive(Deserialize, Default)]
pub struct AnalyticsQuery {
    /// Look-back window in days (default 30, clamped 1..=365).
    days: Option<i64>,
}

#[derive(FromRow, Serialize)]
struct Kpis {
    calls: i64,
    minutes: i64,
    transcripts: i64,
    recordings: i64,
}

#[derive(FromRow, Serialize)]
struct DayPoint {
    day: String,
    calls: i64,
    minutes: i64,
}

#[derive(FromRow, Serialize)]
struct TypeSpend {
    #[sqlx(rename = "type")]
    #[serde(rename = "type")]
    kind: String,
    spent: i64,
}

#[derive(FromRow, Serialize)]
struct ProjectStat {
    project_id: Uuid,
    name: String,
    calls: i64,
    minutes: i64,
}

/// `GET /api/business/organizations/{org_id}/analytics?days=30` (admin+).
/// Spend is financial data, so this matches the credits endpoint's ADMIN gate.
pub async fn summary(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Query(q): Query<AnalyticsQuery>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;

    let days = q.days.unwrap_or(30).clamp(1, 365);

    // KPI totals. Minutes come from ended_at − started_at (in-progress calls
    // contribute 0). `transcripts` = calls whose post-call transcript is ready.
    let kpis: Kpis = sqlx::query_as(
        "SELECT
             count(*)::bigint AS calls,
             COALESCE(SUM(CASE WHEN ended_at IS NOT NULL
                          THEN EXTRACT(EPOCH FROM (ended_at - started_at)) / 60 ELSE 0 END), 0)::bigint AS minutes,
             count(*) FILTER (WHERE transcript_status = 'ready')::bigint AS transcripts,
             count(*) FILTER (WHERE recording_storage_path IS NOT NULL)::bigint AS recordings
         FROM call_sessions
         WHERE org_id = $1 AND started_at >= now() - make_interval(days => $2::int)",
    )
    .bind(org_id)
    .bind(days)
    .fetch_one(pool)
    .await
    .map_err(db_err)?;

    // Spend by ledger type (consumption only → negative amounts, reported as a
    // positive number).
    let by_type: Vec<TypeSpend> = sqlx::query_as(
        "SELECT type, COALESCE(SUM(-amount), 0)::bigint AS spent
         FROM organization_credits_transactions
         WHERE org_id = $1 AND amount < 0 AND created_at >= now() - make_interval(days => $2::int)
         GROUP BY type
         ORDER BY spent DESC",
    )
    .bind(org_id)
    .bind(days)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    let credits_spent: i64 = by_type.iter().map(|t| t.spent).sum();

    // Calls + minutes per day (ascending) for the trend chart.
    let by_day: Vec<DayPoint> = sqlx::query_as(
        "SELECT to_char(date_trunc('day', started_at), 'YYYY-MM-DD') AS day,
                count(*)::bigint AS calls,
                COALESCE(SUM(CASE WHEN ended_at IS NOT NULL
                             THEN EXTRACT(EPOCH FROM (ended_at - started_at)) / 60 ELSE 0 END), 0)::bigint AS minutes
         FROM call_sessions
         WHERE org_id = $1 AND started_at >= now() - make_interval(days => $2::int)
         GROUP BY day
         ORDER BY day",
    )
    .bind(org_id)
    .bind(days)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;

    // Busiest projects in the window.
    let top_projects: Vec<ProjectStat> = sqlx::query_as(
        "SELECT p.id AS project_id, p.name AS name,
                count(*)::bigint AS calls,
                COALESCE(SUM(CASE WHEN cs.ended_at IS NOT NULL
                             THEN EXTRACT(EPOCH FROM (cs.ended_at - cs.started_at)) / 60 ELSE 0 END), 0)::bigint AS minutes
         FROM call_sessions cs
         JOIN projects p ON p.id = cs.project_id
         WHERE cs.org_id = $1 AND cs.started_at >= now() - make_interval(days => $2::int)
         GROUP BY p.id, p.name
         ORDER BY calls DESC, minutes DESC
         LIMIT 8",
    )
    .bind(org_id)
    .bind(days)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;

    Ok(Json(json!({
        "range_days": days,
        "kpis": {
            "calls": kpis.calls,
            "minutes": kpis.minutes,
            "transcripts": kpis.transcripts,
            "recordings": kpis.recordings,
            "credits_spent": credits_spent,
        },
        "credits_by_type": by_type,
        "calls_by_day": by_day,
        "top_projects": top_projects,
    }))
    .into_response())
}
