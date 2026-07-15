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

use crate::business::{db_err, not_found, require_pool, require_role, ADMIN};
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

#[derive(FromRow, Serialize)]
struct Collaborator {
    name: String,
    calls: i64,
}

#[derive(FromRow, Serialize)]
struct WebinarKpis {
    /// Finalized broadcasts in the window (webinars with a session record).
    webinars_hosted: i64,
    /// Largest single-broadcast peak audience.
    peak_viewers_max: i64,
    /// Σ broadcast wall-clock hours.
    total_broadcast_hours: f64,
    /// Σ audience watch hours across all participants of those broadcasts.
    total_watch_hours: f64,
    /// Σ webinar-session cost (org-credit currency, 100 = $1). This is the cost of
    /// the broadcasts, matching what `webinar_sessions.cost_credits` recorded — the
    /// authoritative per-run charge, not the ledger sum (an insufficient-balance run
    /// still records `cost_credits` but skips the ledger deduction, per migration 048).
    webinar_spend: i64,
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
         WHERE org_id = $1 AND kind = 'call'
           AND started_at >= now() - make_interval(days => $2::int)",
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
         WHERE org_id = $1 AND kind = 'call'
           AND started_at >= now() - make_interval(days => $2::int)
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
         WHERE cs.org_id = $1 AND cs.kind = 'call'
           AND cs.started_at >= now() - make_interval(days => $2::int)
         GROUP BY p.id, p.name
         ORDER BY calls DESC, minutes DESC
         LIMIT 8",
    )
    .bind(org_id)
    .bind(days)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;

    // Webinar KPIs for the same window (spec: Webinar Analytics, Phase D). Added as
    // a `webinars` sub-object rather than a breaking reshape of `kpis`. Broadcasts
    // are counted from the finalized `webinar_sessions` rollup (one row per run);
    // watch hours sum every participant of those runs. Left joins keep a broadcast
    // with zero recorded participants counted (its watch hours are just 0).
    let webinars: WebinarKpis = sqlx::query_as(
        "SELECT
             count(*)::bigint AS webinars_hosted,
             COALESCE(MAX(s.peak_viewers), 0)::bigint AS peak_viewers_max,
             COALESCE(SUM(s.duration_seconds), 0)::double precision / 3600.0 AS total_broadcast_hours,
             COALESCE(SUM(s.cost_credits), 0)::bigint AS webinar_spend,
             COALESCE((
                 SELECT SUM(wp.total_watch_seconds)
                 FROM webinar_participants wp
                 JOIN webinar_sessions s2 ON s2.webinar_id = wp.webinar_id
                 WHERE s2.org_id = $1
                   AND s2.created_at >= now() - make_interval(days => $2::int)
             ), 0)::double precision / 3600.0 AS total_watch_hours
         FROM webinar_sessions s
         WHERE s.org_id = $1
           AND s.created_at >= now() - make_interval(days => $2::int)",
    )
    .bind(org_id)
    .bind(days)
    .fetch_one(pool)
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
        "webinars": webinars,
    }))
    .into_response())
}

/// `GET /api/business/organizations/{org_id}/members/{user_id}/analytics?days=30`
/// (admin+). Per-member trend: time in calls, who they collaborated with, and the
/// credits they personally triggered (since 019, ledger rows carry an actor).
pub async fn member_summary(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, target)): Path<(Uuid, Uuid)>,
    Query(q): Query<AnalyticsQuery>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;
    let days = q.days.unwrap_or(30).clamp(1, 365);

    // The target must be a member of this org.
    let who: Option<(String, String)> = sqlx::query_as(
        "SELECT u.name, u.email FROM users u
         JOIN organization_members m ON m.user_id = u.id
         WHERE m.org_id = $1 AND u.id = $2",
    )
    .bind(org_id)
    .bind(target)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    let (name, email) = who.ok_or_else(|| not_found("member not found"))?;

    // Calls joined + minutes in calls (open rows fall back to the call's end / now).
    let (calls, minutes): (i64, i64) = sqlx::query_as(
        "SELECT count(DISTINCT sp.session_id)::bigint,
                COALESCE(SUM(EXTRACT(EPOCH FROM
                    (COALESCE(sp.left_at, cs.ended_at, now()) - sp.joined_at)) / 60), 0)::bigint
         FROM session_participants sp
         JOIN call_sessions cs ON cs.id = sp.session_id
         WHERE cs.org_id = $1 AND cs.kind = 'call' AND sp.user_id = $2
           AND sp.joined_at >= now() - make_interval(days => $3::int)",
    )
    .bind(org_id)
    .bind(target)
    .bind(days)
    .fetch_one(pool)
    .await
    .map_err(db_err)?;

    // Credits this member personally triggered, bucketed by type.
    let by_type: Vec<TypeSpend> = sqlx::query_as(
        "SELECT type, COALESCE(SUM(-amount), 0)::bigint AS spent
         FROM organization_credits_transactions
         WHERE org_id = $1 AND actor_id = $2 AND amount < 0
           AND created_at >= now() - make_interval(days => $3::int)
         GROUP BY type
         ORDER BY spent DESC",
    )
    .bind(org_id)
    .bind(target)
    .bind(days)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    let credits_spent: i64 = by_type.iter().map(|t| t.spent).sum();

    // Who they shared calls with most (by distinct shared sessions). `IS DISTINCT
    // FROM` excludes the member's own (re-joined) rows and includes guests.
    let collaborators: Vec<Collaborator> = sqlx::query_as(
        "SELECT other.name AS name, count(DISTINCT other.session_id)::bigint AS calls
         FROM session_participants me
         JOIN session_participants other
           ON other.session_id = me.session_id AND other.user_id IS DISTINCT FROM me.user_id
         JOIN call_sessions cs ON cs.id = me.session_id
         WHERE cs.org_id = $1 AND cs.kind = 'call' AND me.user_id = $2
           AND me.joined_at >= now() - make_interval(days => $3::int)
         GROUP BY other.name
         ORDER BY calls DESC, name
         LIMIT 10",
    )
    .bind(org_id)
    .bind(target)
    .bind(days)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;

    Ok(Json(json!({
        "range_days": days,
        "user": { "id": target, "name": name, "email": email },
        "calls": calls,
        "minutes_in_calls": minutes,
        "credits_spent": credits_spent,
        "credits_by_type": by_type,
        "collaborators": collaborators,
    }))
    .into_response())
}
