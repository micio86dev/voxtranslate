//! Webinar dashboard data endpoints (Webinar Analytics, Phase D — server half).
//!
//! The dashboard read-side for broadcasts, mirroring the meet call endpoints in
//! [`crate::business::calls`]: an org-scoped webinar HISTORY list (+ project
//! variant) and a per-webinar DETAIL rollup. The org KPI extension lives in
//! [`crate::business::analytics`] (a `webinars` sub-object on the existing
//! summary response).
//!
//! Auth/scope mirrors the meet endpoints EXACTLY (`calls.rs`): `require_role`
//! gates the org at MEMBER, admins/owners see every webinar, and a plain member
//! is scoped to their own. A meet "own call" is one the member appears in
//! (`session_participants`); a webinar's audience is anonymous guests (no org
//! `user_id`), so the member-scoped analog is the webinar they HOSTED
//! (`host_user_id`). Archived webinars (`archived_at IS NOT NULL`) are excluded
//! unless explicitly asked, matching how the rooms list treats soft-deletes.

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::FromRow;
use uuid::Uuid;

use crate::business::{bad_request, db_err, not_found, require_pool, require_role, MEMBER};
use crate::middleware::AuthUser;
use crate::AppState;

/// Admins/owners see every org webinar; plain members are scoped to their own
/// (the ones they hosted). Mirrors `calls::is_org_admin`.
fn is_org_admin(role: &str) -> bool {
    matches!(role, "admin" | "owner")
}

#[derive(FromRow, Serialize)]
struct WebinarRow {
    id: Uuid,
    code: String,
    title: String,
    status: String,
    tier: String,
    project_id: Option<Uuid>,
    project_name: Option<String>,
    scheduled_start: Option<DateTime<Utc>>,
    actual_start: Option<DateTime<Utc>>,
    actual_end: Option<DateTime<Utc>>,
    /// Finalized broadcast length (from `webinar_sessions`); null until finalized.
    duration_seconds: Option<i32>,
    peak_viewers: Option<i32>,
    cost_credits: Option<i32>,
    /// True when at least one AI recap row exists for the webinar.
    has_report: bool,
}

#[derive(Deserialize, Default)]
pub struct HistoryQuery {
    project_id: Option<Uuid>,
    page: Option<i64>,
    limit: Option<i64>,
    from: Option<String>,
    to: Option<String>,
    /// One of the webinar `status` values ('scheduled'|'live'|'ended'|'cancelled');
    /// unknown/empty ⇒ no status filter.
    status: Option<String>,
    /// Include archived webinars too (default: exclude, mirroring the rooms list's
    /// soft-delete handling).
    #[serde(default)]
    include_archived: bool,
}

/// `GET /api/business/organizations/{org_id}/webinars` — paginated webinar history
/// (member). Mirrors `calls::list_org_rooms`.
pub async fn list_org_webinars(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Query(q): Query<HistoryQuery>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let role = require_role(pool, org_id, user.user_id, MEMBER).await?;
    history(pool, org_id, user.user_id, is_org_admin(&role), &q).await
}

/// `GET /api/business/organizations/{org_id}/projects/{project_id}/webinars`
/// (member). Mirrors `calls::list_project_rooms`.
pub async fn list_project_webinars(
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
    let status = normalize_status(q.status.as_deref());

    // Order/filter by the run's real timeline: a finalized webinar sorts by when it
    // started; a still-scheduled one by its scheduled slot (COALESCE), so upcoming
    // and past runs interleave sensibly, newest first. The 1:1 `webinar_sessions`
    // join supplies the finalized rollup columns (null until the webinar ends).
    let rows: Vec<WebinarRow> = sqlx::query_as(
        "SELECT w.id, w.code, w.title, w.status, w.tier, w.project_id,
                p.name AS project_name,
                w.scheduled_start, w.actual_start, w.actual_end,
                s.duration_seconds, s.peak_viewers, s.cost_credits,
                EXISTS (SELECT 1 FROM webinar_reports wr WHERE wr.webinar_id = w.id) AS has_report
         FROM webinars w
         LEFT JOIN projects p ON p.id = w.project_id
         LEFT JOIN webinar_sessions s ON s.webinar_id = w.id
         WHERE w.org_id = $1
           AND ($2::uuid IS NULL OR w.project_id = $2)
           AND ($3::timestamptz IS NULL
                OR COALESCE(w.actual_start, w.scheduled_start) >= $3)
           AND ($4::timestamptz IS NULL
                OR COALESCE(w.actual_start, w.scheduled_start) <= $4)
           AND ($7::text IS NULL OR w.status = $7)
           AND ($8 OR w.archived_at IS NULL)
           -- Non-admins only see webinars they hosted (the audience is anonymous
           -- guests, so participation can't scope like meet calls do).
           AND ($9 OR w.host_user_id = $10)
         ORDER BY COALESCE(w.actual_start, w.scheduled_start, w.created_at) DESC
         LIMIT $5 OFFSET $6",
    )
    .bind(org_id)
    .bind(q.project_id)
    .bind(from)
    .bind(to)
    .bind(limit)
    .bind(offset)
    .bind(status)
    .bind(q.include_archived)
    .bind(is_admin)
    .bind(viewer_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;

    Ok(Json(json!({ "webinars": rows, "page": page, "limit": limit })).into_response())
}

#[derive(FromRow, Serialize)]
struct SessionRollup {
    actual_start: DateTime<Utc>,
    actual_end: DateTime<Utc>,
    scheduled_start: Option<DateTime<Utc>>,
    scheduled_end: Option<DateTime<Utc>>,
    duration_seconds: i32,
    host_online_seconds: i32,
    peak_viewers: i32,
    translated_language_count: i32,
    cost_credits: i32,
}

#[derive(FromRow, Serialize)]
struct ParticipantRow {
    name: Option<String>,
    language_code: Option<String>,
    total_watch_seconds: i32,
    joined_at: DateTime<Utc>,
    /// Presence lifecycle: participants have no `left_at`; `last_seen` is the
    /// closest signal for when they dropped off.
    last_seen: DateTime<Utc>,
    approx_country: Option<String>,
}

#[derive(FromRow, Serialize)]
struct EmailStatusCount {
    status: String,
    count: i64,
}

/// `GET /api/business/organizations/{org_id}/webinars/{webinar_id}` — the detail
/// rollup: session record, participant list, report availability + langs, and an
/// email-log summary. No transcript body (the meet detail's list/rollup shape
/// omits it too; the transcript lives behind its own endpoint).
pub async fn get_webinar_detail(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, webinar_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let role = require_role(pool, org_id, user.user_id, MEMBER).await?;
    let is_admin = is_org_admin(&role);

    // The webinar header, scoped to the org. Non-admins only see their own (hosted)
    // webinars — same rule as the list. 404 (not 403) when out of scope so a
    // member can't probe which webinar ids exist.
    let header: Option<WebinarHeader> = sqlx::query_as(
        "SELECT w.id, w.code, w.title, w.description, w.status, w.tier,
                w.source_language, w.project_id, p.name AS project_name,
                w.host_user_id, w.scheduled_start, w.scheduled_end,
                w.actual_start, w.actual_end, w.archived_at, w.created_at
         FROM webinars w
         LEFT JOIN projects p ON p.id = w.project_id
         WHERE w.id = $1 AND w.org_id = $2 AND ($3 OR w.host_user_id = $4)",
    )
    .bind(webinar_id)
    .bind(org_id)
    .bind(is_admin)
    .bind(user.user_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    let header = header.ok_or_else(|| not_found("webinar not found"))?;

    // 1:1 finalized session rollup (null until the webinar ends).
    let session: Option<SessionRollup> = sqlx::query_as(
        "SELECT actual_start, actual_end, scheduled_start, scheduled_end,
                duration_seconds, host_online_seconds, peak_viewers,
                translated_language_count, cost_credits
         FROM webinar_sessions WHERE webinar_id = $1",
    )
    .bind(webinar_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;

    // Audience: most-watched first. `guest_id`-based rows are anonymous; surface a
    // signed-in user's name when we have one (else the dashboard shows "Guest").
    let participants: Vec<ParticipantRow> = sqlx::query_as(
        "SELECT u.name AS name, wp.language_code, wp.total_watch_seconds,
                wp.joined_at, wp.last_seen, wp.approx_country
         FROM webinar_participants wp
         LEFT JOIN users u ON u.id = wp.user_id
         WHERE wp.webinar_id = $1
         ORDER BY wp.total_watch_seconds DESC, wp.joined_at",
    )
    .bind(webinar_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;

    // Report availability + the languages a recap exists in (one row per language).
    let report_langs: Vec<String> =
        sqlx::query_scalar("SELECT lang FROM webinar_reports WHERE webinar_id = $1 ORDER BY lang")
            .bind(webinar_id)
            .fetch_all(pool)
            .await
            .map_err(db_err)?;

    // Recap-email send log, counted by delivery status ('sent'|'failed').
    let emails_by_status: Vec<EmailStatusCount> = sqlx::query_as(
        "SELECT status, count(*)::bigint AS count
         FROM webinar_emails WHERE webinar_id = $1
         GROUP BY status ORDER BY status",
    )
    .bind(webinar_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    let emails_total: i64 = emails_by_status.iter().map(|e| e.count).sum();

    Ok(Json(json!({
        "webinar": header,
        "session": session,
        "participants": participants,
        "report": {
            "available": !report_langs.is_empty(),
            "languages": report_langs,
        },
        "emails": {
            "total": emails_total,
            "by_status": emails_by_status,
        },
    }))
    .into_response())
}

#[derive(FromRow, Serialize)]
struct WebinarHeader {
    id: Uuid,
    code: String,
    title: String,
    description: Option<String>,
    status: String,
    tier: String,
    source_language: String,
    project_id: Option<Uuid>,
    project_name: Option<String>,
    host_user_id: Option<Uuid>,
    scheduled_start: Option<DateTime<Utc>>,
    scheduled_end: Option<DateTime<Utc>>,
    actual_start: Option<DateTime<Utc>>,
    actual_end: Option<DateTime<Utc>>,
    archived_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

/// Keep only a recognized webinar `status`; anything else (empty, garbage) ⇒
/// `None` (no filter). Guards the `w.status = $7` clause against arbitrary input.
fn normalize_status(raw: Option<&str>) -> Option<String> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) if matches!(s, "scheduled" | "live" | "ended" | "cancelled") => Some(s.to_string()),
        _ => None,
    }
}

fn parse_ts(raw: &Option<String>) -> Result<Option<DateTime<Utc>>, Response> {
    match raw.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => DateTime::parse_from_rfc3339(s)
            .map(|d| Some(d.with_timezone(&Utc)))
            .map_err(|_| bad_request("from/to must be RFC3339 timestamps")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_status_accepts_known_values() {
        for s in ["scheduled", "live", "ended", "cancelled"] {
            assert_eq!(normalize_status(Some(s)).as_deref(), Some(s));
        }
    }

    #[test]
    fn normalize_status_trims_whitespace() {
        assert_eq!(normalize_status(Some("  live  ")).as_deref(), Some("live"));
    }

    #[test]
    fn normalize_status_rejects_unknown_and_empty() {
        assert_eq!(normalize_status(Some("deleted")), None);
        assert_eq!(normalize_status(Some("")), None);
        assert_eq!(normalize_status(Some("   ")), None);
        assert_eq!(normalize_status(None), None);
        // No SQL-injection passthrough: arbitrary input is dropped, not bound.
        assert_eq!(normalize_status(Some("live' OR '1'='1")), None);
    }

    #[test]
    fn is_org_admin_matches_admin_and_owner_only() {
        assert!(is_org_admin("admin"));
        assert!(is_org_admin("owner"));
        assert!(!is_org_admin("member"));
        assert!(!is_org_admin("guest"));
        assert!(!is_org_admin(""));
    }

    #[test]
    fn parse_ts_none_and_empty_yield_none() {
        assert!(parse_ts(&None).unwrap().is_none());
        assert!(parse_ts(&Some(String::new())).unwrap().is_none());
        assert!(parse_ts(&Some("   ".to_string())).unwrap().is_none());
    }

    #[test]
    fn parse_ts_valid_rfc3339_parses() {
        let got = parse_ts(&Some("2026-07-15T10:00:00Z".to_string())).unwrap();
        assert!(got.is_some());
    }

    #[test]
    fn parse_ts_invalid_is_rejected() {
        assert!(parse_ts(&Some("not-a-date".to_string())).is_err());
        assert!(parse_ts(&Some("2026-07-15".to_string())).is_err());
    }
}
