//! What the business dashboard reads about Live Connect (spec 0121, S14).
//!
//! Three views, all scoped to one organisation and decided by role:
//! * **sessions** — the list, filterable by status, project, desk and date;
//! * **a session** — who took part, what was said (original and translation kept separate) and,
//!   for an admin, the diagnostic timeline;
//! * **usage** — what the conversations cost, grouped by desk, project or day, plus the wallet
//!   balance and what the conversations still open have cost so far (the dashboard polls it).
//!
//! Visibility follows the rule the rest of the business area uses for calls: an owner or admin
//! sees every conversation of the organisation; a plain member sees only the ones they took part
//! in. Money (usage) is for admins. A conversation a caller may not see answers exactly like one
//! that does not exist.
#![allow(clippy::result_large_err)]

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::finalize::room_sessions_sql;
use super::routes::api_err;
use crate::business::{db_err, require_pool, require_role, role_rank, ADMIN, MEMBER};
use crate::db::Pool;
use crate::middleware::AuthUser;
use crate::AppState;

const DEFAULT_PAGE: i64 = 25;
const MAX_PAGE: i64 = 100;
/// Longest window `usage` will aggregate over.
const MAX_USAGE_DAYS: i64 = 366;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/business/organizations/{org_id}/live/sessions",
            get(list_sessions),
        )
        .route(
            "/api/business/organizations/{org_id}/live/sessions/{id}",
            get(session_detail),
        )
        .route(
            "/api/business/organizations/{org_id}/live/usage",
            get(usage),
        )
}

fn not_found() -> Response {
    api_err(
        StatusCode::NOT_FOUND,
        "session_not_found",
        "conversation not found",
    )
}

/// True when the caller's role sees the whole organisation, not only their own conversations.
fn sees_all(role: &str) -> bool {
    role_rank(role) >= ADMIN
}

// ---- list ------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub status: Option<String>,
    pub project_id: Option<Uuid>,
    pub desk_id: Option<Uuid>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct SessionRow {
    id: Uuid,
    status: String,
    created_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    ended_at: Option<DateTime<Utc>>,
    end_reason: Option<String>,
    host_lang: String,
    desk_id: Option<Uuid>,
    desk_name: Option<String>,
    location_label: Option<String>,
    project_id: Option<Uuid>,
    project_name: Option<String>,
    guest_name: Option<String>,
    guest_lang: Option<String>,
    staff_name: Option<String>,
    duration_secs: Option<i64>,
    credits: i64,
    has_transcript: bool,
}

impl SessionRow {
    fn view(&self) -> serde_json::Value {
        json!({
            "id": self.id,
            "status": self.status,
            "created_at": self.created_at,
            "started_at": self.started_at,
            "ended_at": self.ended_at,
            "end_reason": self.end_reason,
            "duration_secs": self.duration_secs,
            "host_lang": self.host_lang,
            "desk": self.desk_id.map(|id| json!({ "id": id, "name": self.desk_name, "location_label": self.location_label })),
            "project": self.project_id.map(|id| json!({ "id": id, "name": self.project_name })),
            "guest": json!({ "name": self.guest_name, "lang": self.guest_lang }),
            "staff_name": self.staff_name,
            "credits": self.credits,
            "has_transcript": self.has_transcript,
        })
    }
}

const SESSION_SELECT: &str = "
    SELECT s.id, s.status, s.created_at, s.started_at, s.ended_at, s.end_reason, s.host_lang,
           s.desk_id, d.name AS desk_name, d.location_label,
           s.project_id, p.name AS project_name,
           (SELECT display_name FROM live_participants WHERE session_id = s.id AND role = 'guest') AS guest_name,
           (SELECT lang FROM live_participants WHERE session_id = s.id AND role = 'guest') AS guest_lang,
           (SELECT display_name FROM live_participants WHERE session_id = s.id AND role IN ('host', 'staff')) AS staff_name,
           CASE WHEN s.started_at IS NULL THEN NULL
                ELSE extract(epoch FROM (COALESCE(s.ended_at, now()) - s.started_at))::bigint END AS duration_secs,
           COALESCE((SELECT sum(t.amount) FROM live_usage_ticks t
                      WHERE t.session_id = s.id AND t.payer_kind = 'org'), 0)::bigint AS credits,
           EXISTS (SELECT 1 FROM transcripts tr WHERE tr.session_id = s.call_session_id) AS has_transcript
    FROM live_sessions s
    LEFT JOIN live_desks d ON d.id = s.desk_id
    LEFT JOIN projects p ON p.id = s.project_id";

/// A member sees a conversation only if they sat in one of its two seats.
const MEMBER_VISIBLE: &str =
    "EXISTS (SELECT 1 FROM live_participants mp WHERE mp.session_id = s.id AND mp.user_id = $2)";

pub async fn list_sessions(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Query(q): Query<ListQuery>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let role = require_role(pool, org_id, user.user_id, MEMBER).await?;
    if let Some(status) = q.status.as_deref() {
        if !["waiting", "active", "ended", "expired"].contains(&status) {
            return Err(api_err(
                StatusCode::BAD_REQUEST,
                "bad_status",
                "unknown status",
            ));
        }
    }
    let limit = q.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
    let offset = q.offset.unwrap_or(0).max(0);

    let visible = if sees_all(&role) {
        "($2::uuid IS NOT NULL)".to_string()
    } else {
        MEMBER_VISIBLE.to_string()
    };
    let filters = format!(
        " WHERE s.org_id = $1 AND {visible}
           AND ($3::text IS NULL OR s.status = $3)
           AND ($4::uuid IS NULL OR s.project_id = $4)
           AND ($5::uuid IS NULL OR s.desk_id = $5)
           AND ($6::timestamptz IS NULL OR s.created_at >= $6)
           AND ($7::timestamptz IS NULL OR s.created_at < $7)"
    );
    let rows: Vec<SessionRow> = sqlx::query_as(&format!(
        "{SESSION_SELECT}{filters} ORDER BY s.created_at DESC LIMIT $8 OFFSET $9"
    ))
    .bind(org_id)
    .bind(user.user_id)
    .bind(q.status.as_deref())
    .bind(q.project_id)
    .bind(q.desk_id)
    .bind(q.from)
    .bind(q.to)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    let total: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM live_sessions s{filters}"))
        .bind(org_id)
        .bind(user.user_id)
        .bind(q.status.as_deref())
        .bind(q.project_id)
        .bind(q.desk_id)
        .bind(q.from)
        .bind(q.to)
        .fetch_one(pool)
        .await
        .map_err(db_err)?;

    Ok(Json(json!({
        "sessions": rows.iter().map(SessionRow::view).collect::<Vec<_>>(),
        "total": total,
        "limit": limit,
        "offset": offset,
    }))
    .into_response())
}

// ---- one session -----------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct DetailQuery {
    /// Show each utterance's translation into this language, when one was captured.
    pub lang: Option<String>,
}

#[derive(sqlx::FromRow)]
struct UtteranceRow {
    ts: DateTime<Utc>,
    speaker_peer_id: String,
    speaker_name: String,
    original_lang: String,
    original_text: String,
    translations: serde_json::Value,
}

pub async fn session_detail(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, id)): Path<(Uuid, Uuid)>,
    Query(q): Query<DetailQuery>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let role = require_role(pool, org_id, user.user_id, MEMBER).await?;
    let admin = sees_all(&role);

    let visible = if admin {
        "($2::uuid IS NOT NULL)".to_string()
    } else {
        MEMBER_VISIBLE.to_string()
    };
    let head: Option<SessionRow> = sqlx::query_as(&format!(
        "{SESSION_SELECT} WHERE s.org_id = $1 AND s.id = $3 AND {visible}"
    ))
    .bind(org_id)
    .bind(user.user_id)
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    let head = head.ok_or_else(not_found)?;

    let participants: Vec<(Uuid, String, String, String)> = sqlx::query_as(
        "SELECT peer_id, role, display_name, lang FROM live_participants
         WHERE session_id = $1 ORDER BY joined_at",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;

    // What was said, across every room the conversation lived through. The original is stored
    // as spoken; a translation is only ever shown beside it, never in its place.
    let events: Vec<UtteranceRow> = sqlx::query_as(&format!(
        "SELECT te.ts, te.speaker_peer_id, te.speaker_name, te.original_lang, te.original_text, te.translations
         FROM transcript_events te
         WHERE te.event_type = 'speech' AND te.session_id IN ({})
         ORDER BY te.ts",
        room_sessions_sql("$1")
    ))
    .bind(id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    let wanted = q.lang.as_deref().map(str::to_ascii_lowercase);
    let first = events.first().map(|e| e.ts);
    let utterances: Vec<_> = events
        .iter()
        .map(|e| {
            let translation = wanted
                .as_deref()
                .and_then(|l| e.translations.get(l))
                .and_then(|v| v.as_str());
            json!({
                "at": e.ts,
                "offset_ms": first.map(|f| (e.ts - f).num_milliseconds()),
                "speaker_id": e.speaker_peer_id,
                "speaker_name": e.speaker_name,
                "original_lang": e.original_lang,
                "original": e.original_text,
                "translation": translation,
                "translations": e.translations,
            })
        })
        .collect();

    let mut body = json!({
        "session": head.view(),
        "participants": participants.iter().map(|(peer_id, role, name, lang)| {
            json!({ "peer_id": peer_id, "role": role, "display_name": name, "lang": lang })
        }).collect::<Vec<_>>(),
        "utterances": utterances,
    });
    // The diagnostic timeline is for administrators: it is about how the call went, not what was said.
    if admin {
        let events: Vec<(DateTime<Utc>, String, serde_json::Value)> = sqlx::query_as(
            "SELECT ts, kind, detail FROM live_session_events WHERE session_id = $1 ORDER BY ts, id LIMIT 200",
        )
        .bind(id)
        .fetch_all(pool)
        .await
        .map_err(db_err)?;
        body["events"] = json!(events
            .into_iter()
            .map(|(ts, kind, detail)| json!({ "at": ts, "kind": kind, "detail": detail }))
            .collect::<Vec<_>>());
    }
    Ok(Json(body).into_response())
}

// ---- usage -----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct UsageQuery {
    /// `desk` (default), `project` or `day`.
    pub group: Option<String>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
}

pub async fn usage(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Query(q): Query<UsageQuery>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;

    let to = q.to.unwrap_or_else(Utc::now);
    let from = q.from.unwrap_or_else(|| to - Duration::days(30));
    if from >= to || to - from > Duration::days(MAX_USAGE_DAYS) {
        return Err(api_err(
            StatusCode::BAD_REQUEST,
            "bad_range",
            "the date range must be positive and at most a year",
        ));
    }
    // The grouping is chosen from a fixed list and never interpolated from input.
    let (key, label) = match q.group.as_deref().unwrap_or("desk") {
        "desk" => (
            "COALESCE(s.desk_id::text, '')",
            "COALESCE(d.name, '(no desk)')",
        ),
        "project" => (
            "COALESCE(s.project_id::text, '')",
            "COALESCE(p.name, '(no project)')",
        ),
        "day" => (
            "to_char(date_trunc('day', t.ts), 'YYYY-MM-DD')",
            "to_char(date_trunc('day', t.ts), 'YYYY-MM-DD')",
        ),
        _ => {
            return Err(api_err(
                StatusCode::BAD_REQUEST,
                "bad_group",
                "group must be desk, project or day",
            ))
        }
    };
    let rows: Vec<(String, String, i64, i64)> = sqlx::query_as(&format!(
        "SELECT {key} AS key, {label} AS label, count(DISTINCT t.session_id)::bigint, COALESCE(sum(t.amount), 0)::bigint
         FROM live_usage_ticks t
         JOIN live_sessions s ON s.id = t.session_id
         LEFT JOIN live_desks d ON d.id = s.desk_id
         LEFT JOIN projects p ON p.id = s.project_id
         WHERE s.org_id = $1 AND t.payer_kind = 'org' AND t.ts >= $2 AND t.ts < $3
         GROUP BY 1, 2 ORDER BY 4 DESC, 2"
    ))
    .bind(org_id)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    let (sessions, credits) = rows
        .iter()
        .fold((0, 0), |(s, c), (_, _, n, a)| (s + n, c + a));

    let balance: i32 =
        sqlx::query_scalar("SELECT credits_balance FROM organizations WHERE id = $1")
            .bind(org_id)
            .fetch_one(pool)
            .await
            .map_err(db_err)?;
    // What the conversations still open have cost so far — what the dashboard shows live.
    let active = open_sessions(pool, org_id).await.map_err(db_err)?;

    Ok(Json(json!({
        "group": q.group.as_deref().unwrap_or("desk"),
        "from": from,
        "to": to,
        "rows": rows.into_iter().map(|(key, label, sessions, credits)| {
            json!({ "key": key, "label": label, "sessions": sessions, "credits": credits })
        }).collect::<Vec<_>>(),
        "totals": { "sessions": sessions, "credits": credits },
        "balance": balance,
        "active": active,
    }))
    .into_response())
}

async fn open_sessions(pool: &Pool, org_id: Uuid) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    let rows: Vec<(Uuid, String, Option<String>, Option<DateTime<Utc>>, i64)> = sqlx::query_as(
        "SELECT s.id, s.status, d.name, s.started_at,
                COALESCE((SELECT sum(t.amount) FROM live_usage_ticks t
                           WHERE t.session_id = s.id AND t.payer_kind = 'org'), 0)::bigint
         FROM live_sessions s LEFT JOIN live_desks d ON d.id = s.desk_id
         WHERE s.org_id = $1 AND s.status IN ('waiting', 'active')
         ORDER BY s.created_at DESC LIMIT 100",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, status, desk, started_at, credits)| {
            json!({ "session_id": id, "status": status, "desk": desk, "started_at": started_at, "credits": credits })
        })
        .collect())
}
