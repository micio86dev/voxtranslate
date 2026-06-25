//! Personal scheduled meetings for B2C users (spec: scheduled meetings, Phase 1b).
//!
//! Mirrors the B2B flow (`business::meetings`) without org/project scoping: a meeting
//! is the user's own Google Calendar event + a VoxTranslate room; invitees are
//! external emails. Phase 2 (friends) plugs an extra invitee source in here.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Deserialize;
use uuid::Uuid;

use crate::business::meetings::{build_rrule, InviteeRow, MeetingDetail, MeetingRow, Recurrence};
use crate::business::{bad_request, conflict, db_err, not_found, require_pool};
use crate::db::Pool;
use crate::google_calendar::{self, EventInput};
use crate::google_oauth::{self, OauthError};
use crate::invite::gen_room_code;
use crate::middleware::AuthUser;
use crate::AppState;

#[derive(Deserialize)]
pub struct CreateInput {
    title: String,
    #[serde(default)]
    description: Option<String>,
    scheduled_at: DateTime<Utc>,
    #[serde(default)]
    end_at: Option<DateTime<Utc>>,
    #[serde(default)]
    duration_minutes: Option<i64>,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    reminder_minutes_before: Option<i32>,
    #[serde(default)]
    invitee_emails: Vec<String>,
    #[serde(default)]
    recurrence: Option<Recurrence>,
}

#[derive(Deserialize, Default)]
pub struct ListQuery {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
}

fn calendar_token_err(e: OauthError) -> Response {
    match e {
        OauthError::NoConnection | OauthError::NotConfigured => {
            conflict("connect your Google Calendar to schedule meetings")
        }
        other => {
            tracing::error!("calendar token error: {other}");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "calendar error",
            )
                .into_response()
        }
    }
}

async fn load_detail(
    pool: &Pool,
    creator_id: Uuid,
    meeting_id: Uuid,
) -> Result<MeetingDetail, Response> {
    let meeting: Option<MeetingRow> = sqlx::query_as(
        "SELECT id, org_id, project_id, title, description, scheduled_at, end_at, timezone,
                room_code, join_url, status, reminder_minutes_before, recurrence, created_at
         FROM scheduled_meetings WHERE id = $1 AND creator_user_id = $2",
    )
    .bind(meeting_id)
    .bind(creator_id)
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

/// `POST /api/meetings`
pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<CreateInput>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let title = body.title.trim().to_string();
    if title.is_empty() {
        return Err(bad_request("title is required"));
    }
    let end_at = body.end_at.unwrap_or_else(|| {
        body.scheduled_at + ChronoDuration::minutes(body.duration_minutes.unwrap_or(30).max(1))
    });
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

    // Validate + dedup external emails.
    let mut emails: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for raw in &body.invitee_emails {
        let email = raw.trim().to_string();
        if !crate::ai::email_draft::valid_email(&email) {
            return Err(bad_request("invalid invitee email"));
        }
        if seen.insert(email.to_lowercase()) {
            emails.push(email);
        }
    }

    let meeting_id = Uuid::new_v4();
    let room_code = gen_room_code();
    let join_url = format!("{}/?room={}", state.config.app_base_url, room_code);

    let access = google_oauth::valid_access_token(&state, user.user_id)
        .await
        .map_err(calendar_token_err)?;
    let mut props = HashMap::new();
    props.insert("vox_meeting_id".to_string(), meeting_id.to_string());
    props.insert("vox_room_code".to_string(), room_code.clone());
    let rrule = body.recurrence.as_ref().and_then(build_rrule);
    let recurrence_str = rrule.as_ref().and_then(|v| v.first().cloned());
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
            attendee_emails: emails.clone(),
            private_props: props,
            recurrence: rrule,
        },
    )
    .await
    .map_err(|e| {
        tracing::error!("create calendar event failed: {e}");
        (
            axum::http::StatusCode::BAD_GATEWAY,
            "could not create the calendar event",
        )
            .into_response()
    })?;

    let mut tx = pool.begin().await.map_err(db_err)?;
    sqlx::query(
        "INSERT INTO scheduled_meetings
            (id, creator_user_id, title, description, scheduled_at, end_at, timezone,
             room_code, join_url, google_calendar_event_id, reminder_minutes_before, recurrence)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)",
    )
    .bind(meeting_id)
    .bind(user.user_id)
    .bind(&title)
    .bind(&body.description)
    .bind(body.scheduled_at)
    .bind(end_at)
    .bind(&timezone)
    .bind(&room_code)
    .bind(&join_url)
    .bind(&event.id)
    .bind(reminder)
    .bind(&recurrence_str)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    for email in &emails {
        sqlx::query(
            "INSERT INTO scheduled_meeting_invitees (meeting_id, email) VALUES ($1, $2)
             ON CONFLICT (meeting_id, email) DO NOTHING",
        )
        .bind(meeting_id)
        .bind(email)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    }
    tx.commit().await.map_err(db_err)?;

    let detail = load_detail(pool, user.user_id, meeting_id).await?;
    Ok((axum::http::StatusCode::CREATED, Json(detail)).into_response())
}

/// `GET /api/meetings?from=&to=`
pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<ListQuery>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let from = q
        .from
        .unwrap_or_else(|| Utc::now() - ChronoDuration::days(7));
    let to =
        q.to.unwrap_or_else(|| Utc::now() + ChronoDuration::days(90));
    let rows: Vec<MeetingRow> = sqlx::query_as(
        "SELECT id, org_id, project_id, title, description, scheduled_at, end_at, timezone,
                room_code, join_url, status, reminder_minutes_before, recurrence, created_at
         FROM scheduled_meetings
         WHERE creator_user_id = $1 AND scheduled_at >= $2 AND scheduled_at <= $3
         ORDER BY scheduled_at",
    )
    .bind(user.user_id)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    Ok(Json(rows).into_response())
}

/// `GET /api/meetings/{id}`
pub async fn get(
    State(state): State<AppState>,
    user: AuthUser,
    Path(meeting_id): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let detail = load_detail(pool, user.user_id, meeting_id).await?;
    Ok(Json(detail).into_response())
}

/// `POST /api/meetings/{id}/cancel`
pub async fn cancel(
    State(state): State<AppState>,
    user: AuthUser,
    Path(meeting_id): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    // Ensure ownership (404 otherwise).
    let _ = load_detail(pool, user.user_id, meeting_id).await?;
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
    sqlx::query(
        "UPDATE scheduled_meetings SET status = 'cancelled', updated_at = now() WHERE id = $1",
    )
    .bind(meeting_id)
    .execute(pool)
    .await
    .map_err(db_err)?;
    Ok(axum::http::StatusCode::NO_CONTENT.into_response())
}
