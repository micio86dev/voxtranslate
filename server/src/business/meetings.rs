//! Scheduled meetings for B2B orgs (spec: scheduled meetings, Phase 1b).
//!
//! A meeting is mirrored to the organizer's Google Calendar (source of truth for
//! time/attendees/RSVP) and pre-binds its room to the org/project via
//! `room_business_bindings`, so opening the room auto-links the call to the project
//! exactly like a live meet (`TranscriptService::session_started`). Invitees are org
//! members (resolved to emails) and/or external emails; Google sends native invites.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use crate::business::{bad_request, conflict, db_err, not_found, require_pool, require_role, MEMBER};
use crate::db::Pool;
use crate::google_calendar::{self, EventInput};
use crate::google_oauth::{self, OauthError};
use crate::invite::gen_room_code;
use crate::middleware::AuthUser;
use crate::AppState;

#[derive(FromRow, Serialize)]
pub struct MeetingRow {
    pub id: Uuid,
    pub org_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
    pub title: String,
    pub description: Option<String>,
    pub scheduled_at: DateTime<Utc>,
    pub end_at: DateTime<Utc>,
    pub timezone: String,
    pub room_code: String,
    pub join_url: String,
    pub status: String,
    pub reminder_minutes_before: i32,
    pub created_at: DateTime<Utc>,
}

#[derive(FromRow, Serialize)]
pub struct InviteeRow {
    pub user_id: Option<Uuid>,
    pub email: String,
    pub role: String,
    pub rsvp_status: String,
}

#[derive(Serialize)]
pub struct MeetingDetail {
    #[serde(flatten)]
    pub meeting: MeetingRow,
    pub invitees: Vec<InviteeRow>,
}

#[derive(Deserialize)]
pub struct MeetingInput {
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    pub scheduled_at: DateTime<Utc>,
    #[serde(default)]
    pub end_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub duration_minutes: Option<i64>,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub project_id: Option<Uuid>,
    #[serde(default)]
    pub reminder_minutes_before: Option<i32>,
    /// Invitees that are org members (resolved to their account email).
    #[serde(default)]
    pub invitee_user_ids: Vec<Uuid>,
    /// External invitees by email.
    #[serde(default)]
    pub invitee_emails: Vec<String>,
}

#[derive(Deserialize, Default)]
pub struct ListQuery {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
}

/// Resolved invitee: an account email (with user_id) or an external email.
struct ResolvedInvitee {
    user_id: Option<Uuid>,
    email: String,
}

fn end_or_default(scheduled_at: DateTime<Utc>, end_at: Option<DateTime<Utc>>, mins: Option<i64>) -> DateTime<Utc> {
    end_at.unwrap_or_else(|| scheduled_at + ChronoDuration::minutes(mins.unwrap_or(30).max(1)))
}

/// Resolve the invitee list: org-member ids → their emails (only members of THIS
/// org), plus validated external emails. Deduplicated by lowercased email.
async fn resolve_invitees(
    pool: &Pool,
    org_id: Uuid,
    user_ids: &[Uuid],
    emails: &[String],
) -> Result<Vec<ResolvedInvitee>, Response> {
    let mut out: Vec<ResolvedInvitee> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    if !user_ids.is_empty() {
        let rows: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT u.id, u.email
             FROM users u
             JOIN organization_members m ON m.user_id = u.id AND m.org_id = $1
             WHERE u.id = ANY($2)",
        )
        .bind(org_id)
        .bind(user_ids)
        .fetch_all(pool)
        .await
        .map_err(db_err)?;
        for (id, email) in rows {
            if seen.insert(email.to_lowercase()) {
                out.push(ResolvedInvitee {
                    user_id: Some(id),
                    email,
                });
            }
        }
    }

    for raw in emails {
        let email = raw.trim().to_string();
        if !crate::ai::email_draft::valid_email(&email) {
            return Err(bad_request("invalid invitee email"));
        }
        if seen.insert(email.to_lowercase()) {
            out.push(ResolvedInvitee {
                user_id: None,
                email,
            });
        }
    }
    Ok(out)
}

/// Fetch a meeting + its invitees, scoped to the org.
async fn load_detail(pool: &Pool, org_id: Uuid, meeting_id: Uuid) -> Result<MeetingDetail, Response> {
    let meeting: Option<MeetingRow> = sqlx::query_as(
        "SELECT id, org_id, project_id, title, description, scheduled_at, end_at, timezone,
                room_code, join_url, status, reminder_minutes_before, created_at
         FROM scheduled_meetings WHERE id = $1 AND org_id = $2",
    )
    .bind(meeting_id)
    .bind(org_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    let meeting = meeting.ok_or_else(|| not_found("meeting not found"))?;
    let invitees: Vec<InviteeRow> = sqlx::query_as(
        "SELECT user_id, email, role, rsvp_status FROM scheduled_meeting_invitees
         WHERE meeting_id = $1 ORDER BY created_at",
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    Ok(MeetingDetail { meeting, invitees })
}

fn private_props(meeting_id: Uuid, room_code: &str, project_id: Option<Uuid>) -> HashMap<String, String> {
    let mut p = HashMap::new();
    p.insert("vox_meeting_id".to_string(), meeting_id.to_string());
    p.insert("vox_room_code".to_string(), room_code.to_string());
    if let Some(pid) = project_id {
        p.insert("vox_project_id".to_string(), pid.to_string());
    }
    p
}

/// Map an OAuth/token error to a client response (409 prompts "connect calendar").
fn calendar_token_err(e: OauthError) -> Response {
    match e {
        OauthError::NoConnection | OauthError::NotConfigured => {
            conflict("connect your Google Calendar to schedule meetings")
        }
        other => {
            tracing::error!("calendar token error: {other}");
            (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "calendar error").into_response()
        }
    }
}

async fn validate_project(pool: &Pool, org_id: Uuid, project_id: Uuid) -> Result<(), Response> {
    let ok: Option<bool> =
        sqlx::query_scalar("SELECT true FROM projects WHERE id = $1 AND org_id = $2")
            .bind(project_id)
            .bind(org_id)
            .fetch_optional(pool)
            .await
            .map_err(db_err)?;
    if ok.is_none() {
        return Err(bad_request("project does not belong to this org"));
    }
    Ok(())
}

/// `POST /api/business/organizations/{org_id}/meetings`
pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Json(body): Json<MeetingInput>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;

    let title = body.title.trim().to_string();
    if title.is_empty() {
        return Err(bad_request("title is required"));
    }
    if let Some(pid) = body.project_id {
        validate_project(pool, org_id, pid).await?;
    }
    let end_at = end_or_default(body.scheduled_at, body.end_at, body.duration_minutes);
    if end_at <= body.scheduled_at {
        return Err(bad_request("end must be after start"));
    }
    let timezone = body
        .timezone
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("UTC")
        .to_string();
    let reminder = body.reminder_minutes_before.unwrap_or(10).clamp(0, 1440);
    let invitees = resolve_invitees(pool, org_id, &body.invitee_user_ids, &body.invitee_emails).await?;

    let meeting_id = Uuid::new_v4();
    let room_code = gen_room_code();
    let join_url = format!("{}/?room={}", state.config.app_base_url, room_code);

    // Calendar is the source of truth: create the event first (requires a connected
    // Google account). Bail before writing our row if there's no connection.
    let access = google_oauth::valid_access_token(&state, user.user_id)
        .await
        .map_err(calendar_token_err)?;
    let event = google_calendar::create_event(
        &state.http,
        &access,
        "primary",
        &EventInput {
            summary: title.clone(),
            description: body.description.clone(),
            start_rfc3339: body.scheduled_at.to_rfc3339(),
            end_rfc3339: end_at.to_rfc3339(),
            timezone: timezone.clone(),
            attendee_emails: invitees.iter().map(|i| i.email.clone()).collect(),
            private_props: private_props(meeting_id, &room_code, body.project_id),
        },
    )
    .await
    .map_err(|e| {
        tracing::error!("create calendar event failed: {e}");
        (axum::http::StatusCode::BAD_GATEWAY, "could not create the calendar event").into_response()
    })?;

    let mut tx = pool.begin().await.map_err(db_err)?;
    sqlx::query(
        "INSERT INTO scheduled_meetings
            (id, creator_user_id, org_id, project_id, title, description, scheduled_at, end_at,
             timezone, room_code, join_url, google_calendar_event_id, reminder_minutes_before)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)",
    )
    .bind(meeting_id)
    .bind(user.user_id)
    .bind(org_id)
    .bind(body.project_id)
    .bind(&title)
    .bind(&body.description)
    .bind(body.scheduled_at)
    .bind(end_at)
    .bind(&timezone)
    .bind(&room_code)
    .bind(&join_url)
    .bind(&event.id)
    .bind(reminder)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;

    for inv in &invitees {
        sqlx::query(
            "INSERT INTO scheduled_meeting_invitees (meeting_id, user_id, email)
             VALUES ($1, $2, $3) ON CONFLICT (meeting_id, email) DO NOTHING",
        )
        .bind(meeting_id)
        .bind(inv.user_id)
        .bind(&inv.email)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    }

    // Pre-bind the room so the call auto-links to the org/project on open (reuses the
    // same mechanism as PATCH /api/rooms/{room}/business).
    sqlx::query(
        "INSERT INTO room_business_bindings (room, org_id, project_id, cloud_recording_enabled, created_by)
         VALUES ($1, $2, $3, FALSE, $4)
         ON CONFLICT (room) DO UPDATE SET
            org_id = EXCLUDED.org_id, project_id = EXCLUDED.project_id, updated_at = now()",
    )
    .bind(&room_code)
    .bind(org_id)
    .bind(body.project_id)
    .bind(user.user_id)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    tx.commit().await.map_err(db_err)?;

    super::audit::log_audit_event(
        pool,
        org_id,
        user.user_id,
        "meeting.create",
        "meeting",
        meeting_id,
        serde_json::json!({ "title": title }),
    );

    // Notify invitees who have an account (external emails get Google's native invite).
    let data = serde_json::json!({
        "meeting_id": meeting_id, "room_code": room_code, "join_url": join_url,
        "scheduled_at": body.scheduled_at,
    });
    let ntitle = format!("Invitation: {title}");
    for inv in &invitees {
        if let Some(uid) = inv.user_id {
            crate::notifications::notify(
                &state,
                uid,
                "meeting_invited",
                &ntitle,
                "You've been invited to a meeting.",
                data.clone(),
            )
            .await;
        }
    }

    let detail = load_detail(pool, org_id, meeting_id).await?;
    Ok((axum::http::StatusCode::CREATED, Json(detail)).into_response())
}

/// Notify the creator + every invitee that has an account.
async fn notify_participants(
    state: &AppState,
    meeting_id: Uuid,
    creator_id: Uuid,
    kind: &str,
    title: &str,
    body: &str,
    data: serde_json::Value,
) {
    let Some(pool) = state.pool.as_ref() else {
        return;
    };
    let mut ids: Vec<Uuid> = vec![creator_id];
    if let Ok(rows) = sqlx::query_scalar::<_, Uuid>(
        "SELECT user_id FROM scheduled_meeting_invitees WHERE meeting_id = $1 AND user_id IS NOT NULL",
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await
    {
        ids.extend(rows);
    }
    ids.sort();
    ids.dedup();
    for uid in ids {
        crate::notifications::notify(state, uid, kind, title, body, data.clone()).await;
    }
}

/// `GET /api/business/organizations/{org_id}/meetings?from=&to=` — list from our
/// mirror (kept in sync on writes). Google Calendar remains authoritative for edits.
pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Query(q): Query<ListQuery>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;
    let from = q.from.unwrap_or_else(|| Utc::now() - ChronoDuration::days(30));
    let to = q.to.unwrap_or_else(|| Utc::now() + ChronoDuration::days(90));
    let rows: Vec<MeetingRow> = sqlx::query_as(
        "SELECT id, org_id, project_id, title, description, scheduled_at, end_at, timezone,
                room_code, join_url, status, reminder_minutes_before, created_at
         FROM scheduled_meetings
         WHERE org_id = $1 AND scheduled_at >= $2 AND scheduled_at <= $3
         ORDER BY scheduled_at",
    )
    .bind(org_id)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    Ok(Json(rows).into_response())
}

/// `GET /api/business/organizations/{org_id}/meetings/{id}`
pub async fn get(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, meeting_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;
    let detail = load_detail(pool, org_id, meeting_id).await?;
    Ok(Json(detail).into_response())
}

/// `PATCH /api/business/organizations/{org_id}/meetings/{id}` — replace core fields +
/// invitees and patch the Google Calendar event.
pub async fn update(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, meeting_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<MeetingInput>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;

    let existing = load_detail(pool, org_id, meeting_id).await?;
    if existing.meeting.status != "scheduled" {
        return Err(conflict("meeting is not editable"));
    }
    let title = body.title.trim().to_string();
    if title.is_empty() {
        return Err(bad_request("title is required"));
    }
    if let Some(pid) = body.project_id {
        validate_project(pool, org_id, pid).await?;
    }
    let end_at = end_or_default(body.scheduled_at, body.end_at, body.duration_minutes);
    let timezone = body
        .timezone
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(existing.meeting.timezone.as_str())
        .to_string();
    let reminder = body
        .reminder_minutes_before
        .unwrap_or(existing.meeting.reminder_minutes_before)
        .clamp(0, 1440);
    let invitees = resolve_invitees(pool, org_id, &body.invitee_user_ids, &body.invitee_emails).await?;

    // Update the Calendar event (source of truth) first.
    let event_id: Option<String> = sqlx::query_scalar(
        "SELECT google_calendar_event_id FROM scheduled_meetings WHERE id = $1",
    )
    .bind(meeting_id)
    .fetch_one(pool)
    .await
    .map_err(db_err)?;
    if let Some(event_id) = event_id {
        let access = google_oauth::valid_access_token(&state, user.user_id)
            .await
            .map_err(calendar_token_err)?;
        google_calendar::update_event(
            &state.http,
            &access,
            "primary",
            &event_id,
            &EventInput {
                summary: title.clone(),
                description: body.description.clone(),
                start_rfc3339: body.scheduled_at.to_rfc3339(),
                end_rfc3339: end_at.to_rfc3339(),
                timezone: timezone.clone(),
                attendee_emails: invitees.iter().map(|i| i.email.clone()).collect(),
                private_props: private_props(meeting_id, &existing.meeting.room_code, body.project_id),
            },
        )
        .await
        .map_err(|e| {
            tracing::error!("update calendar event failed: {e}");
            (axum::http::StatusCode::BAD_GATEWAY, "could not update the calendar event").into_response()
        })?;
    }

    let mut tx = pool.begin().await.map_err(db_err)?;
    sqlx::query(
        "UPDATE scheduled_meetings SET
            title = $2, description = $3, scheduled_at = $4, end_at = $5, timezone = $6,
            project_id = $7, reminder_minutes_before = $8,
            reminder_sent_at = NULL, updated_at = now()
         WHERE id = $1",
    )
    .bind(meeting_id)
    .bind(&title)
    .bind(&body.description)
    .bind(body.scheduled_at)
    .bind(end_at)
    .bind(&timezone)
    .bind(body.project_id)
    .bind(reminder)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    sqlx::query("DELETE FROM scheduled_meeting_invitees WHERE meeting_id = $1")
        .bind(meeting_id)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    for inv in &invitees {
        sqlx::query(
            "INSERT INTO scheduled_meeting_invitees (meeting_id, user_id, email)
             VALUES ($1, $2, $3) ON CONFLICT (meeting_id, email) DO NOTHING",
        )
        .bind(meeting_id)
        .bind(inv.user_id)
        .bind(&inv.email)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    }
    // Keep the room binding's project in sync.
    sqlx::query("UPDATE room_business_bindings SET project_id = $2, updated_at = now() WHERE room = $1")
        .bind(&existing.meeting.room_code)
        .bind(body.project_id)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    tx.commit().await.map_err(db_err)?;

    let creator: Uuid =
        sqlx::query_scalar("SELECT creator_user_id FROM scheduled_meetings WHERE id = $1")
            .bind(meeting_id)
            .fetch_one(pool)
            .await
            .unwrap_or(user.user_id);
    let data = serde_json::json!({
        "meeting_id": meeting_id, "room_code": existing.meeting.room_code,
        "join_url": existing.meeting.join_url, "scheduled_at": body.scheduled_at,
    });
    notify_participants(
        &state,
        meeting_id,
        creator,
        "meeting_updated",
        &format!("Updated: {title}"),
        "A meeting was updated.",
        data,
    )
    .await;

    let detail = load_detail(pool, org_id, meeting_id).await?;
    Ok(Json(detail).into_response())
}

/// `POST /api/business/organizations/{org_id}/meetings/{id}/cancel` — delete the
/// Calendar event (notifies attendees) and mark the meeting cancelled (kept for history).
pub async fn cancel(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, meeting_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;
    let detail = load_detail(pool, org_id, meeting_id).await?;

    if let Some(event_id) = sqlx::query_scalar::<_, Option<String>>(
        "SELECT google_calendar_event_id FROM scheduled_meetings WHERE id = $1",
    )
    .bind(meeting_id)
    .fetch_one(pool)
    .await
    .map_err(db_err)?
    {
        if let Ok(access) = google_oauth::valid_access_token(&state, user.user_id).await {
            if let Err(e) =
                google_calendar::delete_event(&state.http, &access, "primary", &event_id).await
            {
                tracing::warn!("delete calendar event failed (continuing): {e}");
            }
        }
    }

    sqlx::query("UPDATE scheduled_meetings SET status = 'cancelled', updated_at = now() WHERE id = $1")
        .bind(meeting_id)
        .execute(pool)
        .await
        .map_err(db_err)?;

    let creator: Uuid =
        sqlx::query_scalar("SELECT creator_user_id FROM scheduled_meetings WHERE id = $1")
            .bind(meeting_id)
            .fetch_one(pool)
            .await
            .unwrap_or(user.user_id);
    notify_participants(
        &state,
        meeting_id,
        creator,
        "meeting_cancelled",
        &format!("Cancelled: {}", detail.meeting.title),
        "A meeting was cancelled.",
        serde_json::json!({ "meeting_id": meeting_id, "room_code": detail.meeting.room_code }),
    )
    .await;
    super::audit::log_audit_event(
        pool,
        org_id,
        user.user_id,
        "meeting.cancel",
        "meeting",
        meeting_id,
        serde_json::json!({ "title": detail.meeting.title }),
    );
    Ok(axum::http::StatusCode::NO_CONTENT.into_response())
}
