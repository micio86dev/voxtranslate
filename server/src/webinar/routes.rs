//! Webinar Mode HTTP API (F0-3): host CRUD under `/api/webinars` + the public,
//! auth-free `/api/w/{code}` lookup that guests hit from the join link / QR.
//!
//! Authorization mirrors the Business API: the server connects as the RLS-exempt
//! owning role, so per-request authz is enforced here via [`require_role`] (backed
//! by `get_user_org_role()`), plus an active-subscription gate — the host's org
//! pays (SPEC §1). The public endpoint returns NO host PII.

// Handlers return `Result<Response, Response>` so `?` short-circuits with a status
// code; the `Response` Err variant is intentionally large (mirrors `business`).
#![allow(clippy::result_large_err)]

use axum::extract::{Path, Query, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use std::collections::HashMap;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use uuid::Uuid;

use axum::Extension;

use crate::business::credits::org_subscription_active;
use crate::business::{bad_request, db_err, not_found, require_pool, require_role, MEMBER};
use crate::config::WebinarConfig;
use crate::db::Pool;
use crate::google_calendar::{self, EventInput};
use crate::google_oauth::{self, OauthError};
use crate::middleware::AuthUser;
use crate::webinar::guest::{guest_session, GuestId};
use crate::webinar::media::{media_auth, mint_publish_token, publish_url};
use crate::webinar::{
    create_webinar, find_by_code, find_by_id, join_url, playback_url, NewWebinar, Webinar,
};
use crate::AppState;

/// All webinar routes. Registered in `lib.rs::app` only when `config.webinar` is set.
pub fn routes() -> Router<AppState> {
    // The public participant lookup carries a guest-session cookie (F0-4); scope
    // that middleware to this route only, not the authenticated host CRUD.
    let public = Router::new()
        .route("/api/w/{code}", get(public_get))
        .route("/api/w/{code}/tts-session", get(tts_session))
        // Auto-translated chat (⑤): SEND (POST, host or guest) + history (GET).
        // Both ride the guest-session middleware so `Extension<GuestId>` is set.
        .route(
            "/api/w/{code}/chat",
            post(crate::webinar::chat::post_chat).get(crate::webinar::chat::list_chat),
        )
        .route(
            "/api/w/{code}/files",
            post(crate::webinar::files::upload_webinar_file).layer(
                axum::extract::DefaultBodyLimit::max(crate::files::MAX_BODY_BYTES),
            ),
        )
        // Transcript history for late-joining participants — public, no auth.
        // Returns utterances in chronological order so a late joiner can read
        // what was said before they arrived. Empty when `record_transcript` is off.
        .route("/api/w/{code}/transcript", get(list_public_transcript))
        // Public, auth-free discovery list of public + upcoming/live webinars (042).
        .route("/api/webinars/public", get(public_list))
        .layer(axum::middleware::from_fn(guest_session));
    Router::new()
        .route("/api/webinars", post(create).get(list))
        .route("/api/webinars/{id}", get(get_one).patch(patch))
        .route("/api/webinars/{id}/cancel", post(cancel))
        // Add a scheduled webinar to the host's Google Calendar (#7).
        .route("/api/webinars/{id}/calendar", post(add_to_calendar))
        // Soft-archive / restore (③) — hide from the active list, data preserved.
        .route("/api/webinars/{id}/archive", post(archive))
        .route("/api/webinars/{id}/unarchive", post(unarchive))
        // Transcript history for a completed webinar (recap screen).
        .route("/api/webinars/{id}/transcripts", get(get_transcripts))
        // AI recap (Phase B) — HOST-ONLY. Report (any language) + multilingual
        // participant recap emails, both async (client polls the job endpoint).
        .route(
            "/api/webinars/{id}/ai/report",
            post(ai_report).get(ai_report_latest),
        )
        .route("/api/webinars/{id}/ai/email", post(ai_email))
        .route("/api/webinars/{id}/ai/job/{job_id}", get(ai_job_status))
        // Host mints a short-lived tokenized WHIP publish URL to go on air (F1-1).
        .route("/api/webinars/{id}/go-live", post(go_live))
        // Lifecycle: host client reports on-air / off-air (F1-3).
        .route("/api/webinars/{id}/publish-started", post(publish_started))
        .route("/api/webinars/{id}/publish-stopped", post(publish_stopped))
        // Realtime subtitles (Fase 2) — host STT ingest WebSocket. Query-param JWT
        // auth (browsers can't set WS headers); see `webinar::stt`.
        .route("/api/webinars/{id}/stt", get(crate::webinar::stt::stt_ws))
        // MediaMTX external-auth hook (F1-2) — server-to-server, path-secret auth.
        .route("/internal/media-auth/{caller_secret}", post(media_auth))
        // Realtime presence (Fase 4) — public WebSocket keyed by webinar code.
        .route(
            "/api/w/{code}/presence",
            get(crate::webinar::presence::presence_ws),
        )
        .merge(public)
}

// ---- request bodies --------------------------------------------------------

#[derive(Deserialize)]
pub struct CreateWebinar {
    pub org_id: Uuid,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    pub source_language: String,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub record_video: bool,
    #[serde(default)]
    pub record_transcript: bool,
    #[serde(default)]
    pub voice_clone: bool,
    /// Turn on the auto-translated chat panel for this webinar (⑤). When set,
    /// every message is persisted (recorded) — gated by this flag, not
    /// `record_transcript`.
    #[serde(default)]
    pub chat_enabled: bool,
    /// `public` = discoverable via the public list; absent/`private` = link-only.
    /// Validated with [`valid_visibility`]; a bad value is a clean 400.
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(default)]
    pub scheduled_start: Option<DateTime<Utc>>,
    #[serde(default)]
    pub scheduled_end: Option<DateTime<Utc>>,
    /// Optional project this webinar belongs to (must be in the same org).
    #[serde(default)]
    pub project_id: Option<Uuid>,
    #[serde(default)]
    pub members_only: Option<bool>,
    /// Lead time (minutes) for the "starting soon" friend reminder on a PUBLIC
    /// scheduled webinar. Default 10, clamped 0..=1440 (mirrors scheduled meetings).
    #[serde(default)]
    pub reminder_minutes_before: Option<i32>,
}

#[derive(Deserialize)]
pub struct ListQuery {
    pub org_id: Uuid,
    /// `false`/absent = active webinars; `true` = the archived ones.
    #[serde(default)]
    pub archived: bool,
}

#[derive(Deserialize)]
pub struct PatchWebinar {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub record_video: Option<bool>,
    #[serde(default)]
    pub record_transcript: Option<bool>,
    #[serde(default)]
    pub voice_clone: Option<bool>,
    #[serde(default)]
    pub chat_enabled: Option<bool>,
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(default)]
    pub scheduled_start: Option<DateTime<Utc>>,
    #[serde(default)]
    pub scheduled_end: Option<DateTime<Utc>>,
    #[serde(default)]
    pub project_id: Option<Uuid>,
    #[serde(default)]
    pub members_only: Option<bool>,
    #[serde(default)]
    pub reminder_minutes_before: Option<i32>,
}

/// Default reminder lead time (minutes) when the host doesn't specify one; matches
/// the scheduled-meeting default.
const DEFAULT_REMINDER_MINUTES: i32 = 10;
/// Upper clamp on the reminder lead time (24h), same as scheduled meetings.
const MAX_REMINDER_MINUTES: i32 = 1440;

// ---- validation ------------------------------------------------------------

fn valid_title(raw: &str) -> Result<&str, Response> {
    let t = raw.trim();
    if t.is_empty() || t.chars().count() > 200 {
        return Err(bad_request("title is required (max 200 chars)"));
    }
    Ok(t)
}

pub(crate) fn valid_lang(raw: &str) -> Result<&str, Response> {
    let l = raw.trim();
    if l.is_empty()
        || l.chars().count() > 32
        || !l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(bad_request("source_language must be a language code"));
    }
    Ok(l)
}

/// Default tier is `enhanced` (SPEC §6). Validate before insert so a bad value is
/// a clean 400, not a CHECK-constraint 500.
fn valid_tier(raw: &Option<String>) -> Result<&'static str, Response> {
    match raw.as_deref().map(str::trim) {
        None | Some("") | Some("enhanced") => Ok("enhanced"),
        Some("standard") => Ok("standard"),
        _ => Err(bad_request("tier must be 'standard' or 'enhanced'")),
    }
}

/// Validate a webinar's visibility. Default is `private` (link-only); `public`
/// makes it discoverable via the public list. A bad value is a clean 400, not a
/// CHECK-constraint 500. Mirrors [`valid_lang`].
fn valid_visibility(raw: &Option<String>) -> Result<&'static str, Response> {
    match raw.as_deref().map(str::trim) {
        None | Some("") | Some("private") => Ok("private"),
        Some("public") => Ok("public"),
        _ => Err(bad_request("visibility must be 'public' or 'private'")),
    }
}

/// Clock-skew tolerance for the "start must be in the future" check: a start is
/// only rejected once it's clearly in the past (more than this before `now`), so a
/// small client/server clock drift never bounces a just-now schedule.
const SCHEDULE_PAST_TOLERANCE: ChronoDuration = ChronoDuration::minutes(2);

/// Validate an optional webinar schedule. Pure + `now`-injected so it's
/// deterministic and unit-testable without a live request. Mirrors the 400 style
/// of [`valid_lang`]/[`valid_visibility`].
///
/// Rules (only enforced for the fields that are present):
/// - `start`, when given, must be in the future — rejected only if it's clearly
///   past (`start < now - 2min`), tolerating benign clock skew.
/// - `end`, when given AND an effective `start` is known, must be strictly after
///   `start`. With no `start` to compare against, `end` alone is not validated here.
pub(crate) fn valid_schedule(
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Result<(), Response> {
    if let Some(start) = start {
        if start < now - SCHEDULE_PAST_TOLERANCE {
            return Err(bad_request("scheduled_start must be in the future"));
        }
    }
    if let (Some(start), Some(end)) = (start, end) {
        if end <= start {
            return Err(bad_request("scheduled_end must be after scheduled_start"));
        }
    }
    Ok(())
}

// ---- config guard + views --------------------------------------------------

fn require_cfg(state: &AppState) -> Result<&WebinarConfig, Response> {
    state
        .config
        .webinar
        .as_ref()
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "webinar not configured").into_response())
}

/// Host-facing view: management fields + provisioning URLs. Same org, so
/// `host_user_id` is not a cross-tenant leak.
fn host_view(w: &Webinar, app_base_url: &str, cfg: &WebinarConfig) -> Value {
    json!({
        "id": w.id,
        "org_id": w.org_id,
        "host_user_id": w.host_user_id,
        "code": w.code,
        "title": w.title,
        "description": w.description,
        "source_language": w.source_language,
        "tier": w.tier,
        "status": w.status,
        "scheduled_start": w.scheduled_start,
        "scheduled_end": w.scheduled_end,
        "actual_start": w.actual_start,
        "actual_end": w.actual_end,
        "record_video": w.record_video,
        "record_transcript": w.record_transcript,
        "voice_clone": w.voice_clone,
        "chat_enabled": w.chat_enabled,
        "visibility": w.visibility,
        "project_id": w.project_id,
        "reminder_minutes_before": w.reminder_minutes_before,
        "join_url": join_url(app_base_url, &w.code),
        "playback_url": playback_url(cfg, &w.code),
        "created_at": w.created_at,
        "archived_at": w.archived_at,
    })
}

/// Public view for guests: NO host PII (no email, `host_user_id`, or `org_id`).
fn public_view(w: &Webinar, app_base_url: &str, cfg: &WebinarConfig) -> Value {
    json!({
        "code": w.code,
        "title": w.title,
        "status": w.status,
        "source_language": w.source_language,
        "tier": w.tier,
        // Guests need this to decide whether to render the chat panel (⑤).
        "chat_enabled": w.chat_enabled,
        // A guest on /w/{code} may see whether the webinar is public or private.
        "visibility": w.visibility,
        "join_url": join_url(app_base_url, &w.code),
        "playback_url": playback_url(cfg, &w.code),
        // Avatar shown in the waiting overlay when the host hasn't gone live yet (043).
        "host_avatar_url": w.host_avatar_url,
        "members_only": w.members_only,
        // Timing for the viewer's on-screen counter: elapsed since `actual_start`,
        // and — when the webinar was scheduled with an end — a countdown to
        // `scheduled_end`. Both PII-free; null until the host goes live / if unscheduled.
        "actual_start": w.actual_start,
        "scheduled_end": w.scheduled_end,
    })
}

/// Public list item: the PII-free discoverable shape rendered on the home page.
/// A trimmed [`public_view`] — NO `playback_url` — plus a LIVE `viewers` count
/// read from the in-memory presence registry. NO host PII (no org_id /
/// host_user_id / email).
fn public_list_item(w: &Webinar, app_base_url: &str, viewers: usize) -> Value {
    json!({
        "code": w.code,
        "title": w.title,
        "status": w.status,
        "source_language": w.source_language,
        "tier": w.tier,
        "scheduled_start": w.scheduled_start,
        "join_url": join_url(app_base_url, &w.code),
        "viewers": viewers,
        "members_only": w.members_only,
    })
}

/// Fetch a webinar by id and enforce the caller's org role. 404 when the webinar
/// doesn't exist or the caller isn't a member of its org.
async fn require_webinar_role(
    pool: &Pool,
    id: Uuid,
    user_id: Uuid,
    min_rank: u8,
) -> Result<Webinar, Response> {
    let w = find_by_id(pool, id)
        .await
        .map_err(db_err)?
        .ok_or_else(|| not_found("webinar not found"))?;
    require_role(pool, w.org_id, user_id, min_rank).await?;
    Ok(w)
}

// ---- handlers --------------------------------------------------------------

/// Ensure a project belongs to the org (else 400). Mirrors `business/meetings`.
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

/// `POST /api/webinars` — the host creates a webinar for one of their orgs.
pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<CreateWebinar>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let cfg = require_cfg(&state)?;
    let title = valid_title(&body.title)?;
    let lang = valid_lang(&body.source_language)?;
    let tier = valid_tier(&body.tier)?;
    let visibility = valid_visibility(&body.visibility)?;
    // Integrity: a scheduled start must be in the future and end after start.
    valid_schedule(body.scheduled_start, body.scheduled_end, Utc::now())?;
    // Only a member of the org may host, and the org must have an active
    // subscription — the host's org pays (SPEC §1).
    require_role(pool, body.org_id, user.user_id, MEMBER).await?;
    if !org_subscription_active(pool, body.org_id)
        .await
        .map_err(db_err)?
    {
        return Err((StatusCode::PAYMENT_REQUIRED, "org subscription inactive").into_response());
    }
    if let Some(pid) = body.project_id {
        validate_project(pool, body.org_id, pid).await?;
    }
    let new = NewWebinar {
        org_id: body.org_id,
        host_user_id: user.user_id,
        title,
        description: body
            .description
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
        source_language: lang,
        tier,
        record_video: body.record_video,
        record_transcript: body.record_transcript,
        voice_clone: body.voice_clone,
        chat_enabled: body.chat_enabled,
        visibility,
        scheduled_start: body.scheduled_start,
        scheduled_end: body.scheduled_end,
        project_id: body.project_id,
        members_only: body.members_only.unwrap_or(false),
        reminder_minutes_before: body
            .reminder_minutes_before
            .unwrap_or(DEFAULT_REMINDER_MINUTES)
            .clamp(0, MAX_REMINDER_MINUTES),
    };
    let w = create_webinar(pool, &new, cfg.code_len)
        .await
        .map_err(db_err)?;
    Ok((
        StatusCode::CREATED,
        Json(host_view(&w, &state.config.app_base_url, cfg)),
    )
        .into_response())
}

/// `GET /api/webinars?org_id=…` — list an org's webinars (members only).
pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<ListQuery>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let cfg = require_cfg(&state)?;
    require_role(pool, q.org_id, user.user_id, MEMBER).await?;
    // Default lists the active webinars; `?archived=true` lists the archived ones.
    let rows: Vec<Webinar> = sqlx::query_as(
        "SELECT * FROM webinars
         WHERE org_id = $1 AND (archived_at IS NOT NULL) = $2
         ORDER BY created_at DESC",
    )
    .bind(q.org_id)
    .bind(q.archived)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    let views: Vec<Value> = rows
        .iter()
        .map(|w| host_view(w, &state.config.app_base_url, cfg))
        .collect();
    Ok(Json(views).into_response())
}

/// `GET /api/webinars/{id}` — one webinar (members only).
pub async fn get_one(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let cfg = require_cfg(&state)?;
    let w = require_webinar_role(pool, id, user.user_id, MEMBER).await?;
    Ok(Json(host_view(&w, &state.config.app_base_url, cfg)).into_response())
}

/// `PATCH /api/webinars/{id}` — edit title / toggles ONLY while `scheduled`.
pub async fn patch(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<PatchWebinar>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let cfg = require_cfg(&state)?;
    let w = require_webinar_role(pool, id, user.user_id, MEMBER).await?;
    if w.status != "scheduled" {
        return Err((
            StatusCode::CONFLICT,
            "webinar already started; fields are locked",
        )
            .into_response());
    }
    // Validate any provided fields up front.
    let title = match &body.title {
        Some(t) => Some(valid_title(t)?),
        None => None,
    };
    let tier = match body.tier {
        Some(_) => Some(valid_tier(&body.tier)?),
        None => None,
    };
    let visibility = match body.visibility {
        Some(_) => Some(valid_visibility(&body.visibility)?),
        None => None,
    };
    if let Some(pid) = body.project_id {
        validate_project(pool, w.org_id, pid).await?;
    }
    // Integrity of the (possibly partial) schedule edit. Only a *newly provided*
    // start is checked for being in the future — a stored start that has already
    // passed must not bounce an unrelated edit. The end (when provided) is always
    // validated against the effective start: the patched start if present, else the
    // stored one (COALESCE keeps that stored start), so a lone `scheduled_end` is
    // still checked against the existing start.
    let now = Utc::now();
    valid_schedule(body.scheduled_start, None, now)?;
    let effective_start = body.scheduled_start.or(w.scheduled_start);
    if let Some(end) = body.scheduled_end {
        if let Some(start) = effective_start {
            if end <= start {
                return Err(bad_request("scheduled_end must be after scheduled_start"));
            }
        }
    }
    // Clamp a provided reminder lead time to 0..=1440 (mirrors create + meetings).
    let reminder = body
        .reminder_minutes_before
        .map(|m| m.clamp(0, MAX_REMINDER_MINUTES));
    // COALESCE keeps the existing value for any omitted field.
    let updated: Webinar = sqlx::query_as(
        "UPDATE webinars SET
            title                   = COALESCE($2, title),
            description             = COALESCE($3, description),
            tier                    = COALESCE($4, tier),
            record_video            = COALESCE($5, record_video),
            record_transcript       = COALESCE($6, record_transcript),
            voice_clone             = COALESCE($7, voice_clone),
            chat_enabled            = COALESCE($8, chat_enabled),
            scheduled_start         = COALESCE($9, scheduled_start),
            scheduled_end           = COALESCE($10, scheduled_end),
            project_id              = COALESCE($11, project_id),
            visibility              = COALESCE($12, visibility),
            members_only            = COALESCE($13, members_only),
            reminder_minutes_before = COALESCE($14, reminder_minutes_before),
            updated_at              = now()
         WHERE id = $1
         RETURNING *",
    )
    .bind(id)
    .bind(title)
    .bind(body.description.as_deref())
    .bind(tier)
    .bind(body.record_video)
    .bind(body.record_transcript)
    .bind(body.voice_clone)
    .bind(body.chat_enabled)
    .bind(body.scheduled_start)
    .bind(body.scheduled_end)
    .bind(body.project_id)
    .bind(visibility)
    .bind(body.members_only)
    .bind(reminder)
    .fetch_one(pool)
    .await
    .map_err(db_err)?;
    Ok(Json(host_view(&updated, &state.config.app_base_url, cfg)).into_response())
}

/// `POST /api/webinars/{id}/cancel` — cancel a scheduled/live webinar.
pub async fn cancel(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let cfg = require_cfg(&state)?;
    let w = require_webinar_role(pool, id, user.user_id, MEMBER).await?;
    if w.status == "ended" || w.status == "cancelled" {
        return Err((StatusCode::CONFLICT, "webinar already ended").into_response());
    }
    let updated: Webinar = sqlx::query_as(
        "UPDATE webinars SET status = 'cancelled', updated_at = now() WHERE id = $1 RETURNING *",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .map_err(db_err)?;
    // Best-effort: drop the Google Calendar event if one was created (#7).
    if let Some(eid) = w.google_event_id.as_deref() {
        if let Ok(access) = google_oauth::valid_access_token(&state, user.user_id).await {
            let _ = google_calendar::delete_event(&state.http, &access, "primary", eid).await;
        }
    }
    Ok(Json(host_view(&updated, &state.config.app_base_url, cfg)).into_response())
}

/// `POST /api/webinars/{id}/go-live` — mint a short-lived, path-bound WHIP publish
/// URL (token) for the host to publish to (F1-1). Host (org member) only; the
/// live/ended state transition is driven separately (F1-3).
pub async fn go_live(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let cfg = require_cfg(&state)?;
    let w = require_webinar_role(pool, id, user.user_id, MEMBER).await?;
    if w.status == "ended" || w.status == "cancelled" {
        return Err((StatusCode::CONFLICT, "webinar has ended").into_response());
    }
    let path = format!("webinar/{}", w.code);
    let token = mint_publish_token(&cfg.auth_secret, &path, cfg.publish_token_ttl_secs);
    Ok(Json(json!({
        "publish_url": publish_url(&cfg.ingest_host, &w.code, &token),
        "expires_in": cfg.publish_token_ttl_secs,
    }))
    .into_response())
}

/// `POST /api/webinars/{id}/publish-started` — the host went on air (F1-3).
/// Idempotent: the first call flips scheduled→live and stamps `actual_start`;
/// repeats keep it live with the original start time.
///
/// This explicit signal from the host client is the primary lifecycle driver. A
/// grace window on an *unexpected* WHIP disconnect (so a transient blip doesn't
/// end the webinar) is the MediaMTX `runOnNotReady` backstop, wired with the box
/// in F1-0.
pub async fn publish_started(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let cfg = require_cfg(&state)?;
    let w = require_webinar_role(pool, id, user.user_id, MEMBER).await?;
    if w.status == "ended" || w.status == "cancelled" {
        return Err((StatusCode::CONFLICT, "webinar has already ended").into_response());
    }
    let updated: Webinar = sqlx::query_as(
        "UPDATE webinars
            SET status = 'live', actual_start = COALESCE(actual_start, now()), updated_at = now()
         WHERE id = $1 RETURNING *",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .map_err(db_err)?;
    // Go-live hook: an UNSCHEDULED public webinar has no lead time for the timed
    // scheduler to fire on, so we alert the host's friends at the actual live start.
    // Fire-and-forget + dedup (reminder_sent_at claimed atomically), so it runs once.
    if updated.visibility == "public"
        && updated.scheduled_start.is_none()
        && updated.reminder_sent_at.is_none()
    {
        if let Some(host_id) = updated.host_user_id {
            tokio::spawn(crate::notifications::notify_webinar_friends(
                state.clone(),
                updated.id,
                host_id,
                updated.code.clone(),
                "webinar_live",
            ));
        }
    }
    Ok(Json(host_view(&updated, &state.config.app_base_url, cfg)).into_response())
}

/// `POST /api/webinars/{id}/publish-stopped` — the host went off air (F1-3).
/// Ends a live webinar and stamps `actual_end`; idempotent for any other state.
pub async fn publish_stopped(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let cfg = require_cfg(&state)?;
    let w = require_webinar_role(pool, id, user.user_id, MEMBER).await?;
    // Only a live webinar transitions to ended; any other state is returned as-is.
    let rows = sqlx::query(
        "UPDATE webinars SET status = 'ended', actual_end = now(), updated_at = now()
         WHERE id = $1 AND status = 'live'",
    )
    .bind(id)
    .execute(pool)
    .await
    .map_err(db_err)?
    .rows_affected();
    let current = find_by_id(pool, id).await.map_err(db_err)?.unwrap_or(w);
    // First transition to ended (this call flipped it): finalize the session record +
    // bill the host's org. Best-effort — a failure here must not fail the host's
    // publish-stopped, and `finalize_webinar_run` is idempotent (UNIQUE webinar_id).
    if rows > 0 {
        if let Err(e) = finalize_webinar_run(pool, &current, &state.config).await {
            tracing::error!(webinar_id = %id, "webinar finalization failed: {e}");
        }
    }
    Ok(Json(host_view(&current, &state.config.app_base_url, cfg)).into_response())
}

/// Batch-finalize a finished webinar run: sweep its `webinar_events`, compute the
/// metrics + host cost via the pure [`metrics::finalize_webinar_metrics`], insert a
/// one-row `webinar_sessions` record, finalize each participant's
/// `total_watch_seconds`, and deduct the cost from the host's org.
///
/// Idempotent: the `webinar_sessions.webinar_id` UNIQUE constraint means a second
/// run (a repeated publish-stopped, or a retry) inserts nothing and charges nothing.
/// Defensive: a webinar that never went live (no `actual_start`) writes no record.
async fn finalize_webinar_run(
    pool: &Pool,
    w: &Webinar,
    config: &crate::config::Config,
) -> Result<(), sqlx::Error> {
    use crate::webinar::metrics::{
        finalize_webinar_metrics, WebinarEvent as MetricEvent, WebinarRates,
    };

    // Never went live → nothing to record or charge.
    let (Some(actual_start), Some(actual_end)) = (w.actual_start, w.actual_end) else {
        return Ok(());
    };

    // Idempotency guard: skip if this run was already finalized.
    let already: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM webinar_sessions WHERE webinar_id = $1")
            .bind(w.id)
            .fetch_optional(pool)
            .await?;
    if already.is_some() {
        return Ok(());
    }

    // Resolve the per-tier rates from config (keeps the pure function config-free).
    // The cost model mirrors the meet meter EXACTLY: per broadcast-minute we bill
    // `rate_per_minute × K` (K = distinct participant languages ≠ source_language);
    // K=0 ⇒ that interval is free. There is NO host-STT surcharge and NO separate TTS
    // factor — the per-tier rate already bundles STT + translation + TTS per stream.
    // Standard: consumer per-minute rate (`billing.pricing.user_rate_per_minute` =
    //   COST_PER_MINUTE × (1 + MARKUP)); fall back to the built-in default when
    //   billing isn't configured.
    // Enhanced: the Cartesia per-minute rate (cost_per_minute × (1 + markup)), which
    //   already bundles its STT + TTS; fall back to the Standard rate if unset.
    let standard_rate = config
        .billing
        .as_ref()
        .map(|b| b.pricing.user_rate_per_minute)
        .unwrap_or(WEBINAR_DEFAULT_STANDARD_RATE_USD);
    let rates = if w.tier == "enhanced" {
        match &config.cartesia {
            Some(c) => WebinarRates::enhanced(c.cost_per_minute * (1.0 + c.markup)),
            None => WebinarRates::enhanced(standard_rate),
        }
    } else {
        WebinarRates::standard(standard_rate)
    };

    // Load the event timeline for the cost sweep (join/leave, each join carries the
    // participant's language_code).
    let rows: Vec<(Uuid, String, Option<String>, DateTime<Utc>)> = sqlx::query_as(
        "SELECT participant_id, type, language_code, at
           FROM webinar_events
          WHERE webinar_id = $1 AND participant_id IS NOT NULL
          ORDER BY at",
    )
    .bind(w.id)
    .fetch_all(pool)
    .await?;
    let events: Vec<MetricEvent> = rows
        .into_iter()
        .map(|(participant_id, kind, language_code, at)| MetricEvent {
            participant_id,
            kind,
            language_code,
            at,
        })
        .collect();

    let m = finalize_webinar_metrics(
        &events,
        &w.source_language,
        &w.tier,
        actual_start,
        actual_end,
        rates,
    );

    // Insert the session record AND charge the host's org in ONE transaction, so a
    // crash/DB error between them can never leave a committed session row with no
    // charge (which the idempotency guard would then skip forever → free broadcast).
    // ON CONFLICT DO NOTHING makes the insert itself idempotent under a race (two
    // finalizations arriving together): only the first writer's row lands, and only it
    // proceeds to charge.
    let mut tx = pool.begin().await?;
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO webinar_sessions
            (webinar_id, org_id, host_user_id, actual_start, actual_end, duration_seconds,
             scheduled_start, scheduled_end, host_online_seconds, peak_viewers,
             translated_language_count, cost_credits)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
         ON CONFLICT (webinar_id) DO NOTHING
         RETURNING id",
    )
    .bind(w.id)
    .bind(w.org_id)
    .bind(w.host_user_id)
    .bind(actual_start)
    .bind(actual_end)
    .bind(m.duration_seconds as i32)
    .bind(w.scheduled_start)
    .bind(w.scheduled_end)
    .bind(m.host_online_seconds as i32)
    .bind(m.peak_viewers)
    .bind(m.translated_language_count)
    .bind(m.cost_credits)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(session_id) = inserted else {
        // Lost the race; the winning finalization owns the charge. Nothing was
        // written in this tx → roll it back and return.
        tx.rollback().await?;
        return Ok(());
    };

    // Bill the host's org INSIDE the same tx so the row + charge are atomic.
    // `session_id` on the ledger FKs to `call_sessions`, which this is NOT, so pass
    // None and reference the webinar/session in the description.
    if m.cost_credits > 0 {
        let desc = format!("webinar broadcast {} (session {})", w.code, session_id);
        let charge = crate::business::credits::deduct_org_credits_tx(
            &mut tx,
            w.org_id,
            m.cost_credits,
            "webinar",
            None,
            w.host_user_id,
            &desc,
        )
        .await?;
        if let crate::business::credits::OrgCharge::Insufficient { balance, required } = charge {
            // The broadcast already happened; we don't roll it back. Record the
            // shortfall for reconciliation rather than silently dropping the charge —
            // the session row still commits (with cost_credits set, but no ledger
            // deduction), matching the prior behavior of NOT reverting the broadcast.
            tracing::warn!(
                webinar_id = %w.id, org_id = %w.org_id, balance, required,
                "webinar host org has insufficient credits for finalized broadcast"
            );
        }
    }

    // Commit the session row + charge together.
    tx.commit().await?;

    // Finalize each participant's total_watch_seconds = Σ their in-window join→leave
    // spans (an open join with no leave runs to actual_end). Analytics only, so it
    // runs OUTSIDE the billing transaction (best-effort, after commit) — a failure
    // here must not undo the row + charge. Done in SQL over the event log so a re-join
    // accumulates correctly.
    if let Err(e) = sqlx::query(
        "WITH spans AS (
            SELECT participant_id,
                   at AS joined_at,
                   COALESCE(
                       LEAD(at) OVER (PARTITION BY participant_id ORDER BY at),
                       $2
                   ) AS ended_at,
                   type
              FROM webinar_events
             WHERE webinar_id = $1 AND participant_id IS NOT NULL
        )
        UPDATE webinar_participants p SET total_watch_seconds = COALESCE(agg.secs, 0)
          FROM (
            SELECT participant_id,
                   SUM(GREATEST(0, EXTRACT(EPOCH FROM (
                       LEAST(ended_at, $2) - GREATEST(joined_at, $3)
                   ))))::int AS secs
              FROM spans
             WHERE type = 'join'
             GROUP BY participant_id
          ) agg
         WHERE p.id = agg.participant_id AND p.webinar_id = $1",
    )
    .bind(w.id)
    .bind(actual_end)
    .bind(actual_start)
    .execute(pool)
    .await
    {
        tracing::warn!(webinar_id = %w.id, "webinar total_watch_seconds update failed: {e}");
    }

    tracing::info!(
        webinar_id = %w.id, session_id = %session_id,
        duration_seconds = m.duration_seconds, peak_viewers = m.peak_viewers,
        translated_language_count = m.translated_language_count, cost_credits = m.cost_credits,
        "webinar run finalized"
    );
    Ok(())
}

/// Fallback Standard per-minute rate (USD) when billing config is absent — the
/// built-in `COST_PER_MINUTE` (0.008) × (1 + `MARKUP_PERCENTAGE` 0.25) = 0.01/min.
const WEBINAR_DEFAULT_STANDARD_RATE_USD: f64 = 0.01;

/// `POST /api/webinars/{id}/archive` — soft-archive (hide from the active list;
/// data preserved). Member only.
pub async fn archive(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let cfg = require_cfg(&state)?;
    require_webinar_role(pool, id, user.user_id, MEMBER).await?;
    let updated: Webinar = sqlx::query_as(
        "UPDATE webinars SET archived_at = now(), updated_at = now() WHERE id = $1 RETURNING *",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .map_err(db_err)?;
    Ok(Json(host_view(&updated, &state.config.app_base_url, cfg)).into_response())
}

/// `POST /api/webinars/{id}/unarchive` — restore an archived webinar. Member only.
pub async fn unarchive(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let cfg = require_cfg(&state)?;
    require_webinar_role(pool, id, user.user_id, MEMBER).await?;
    let updated: Webinar = sqlx::query_as(
        "UPDATE webinars SET archived_at = NULL, updated_at = now() WHERE id = $1 RETURNING *",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .map_err(db_err)?;
    Ok(Json(host_view(&updated, &state.config.app_base_url, cfg)).into_response())
}

/// `GET /api/w/{code}/transcript?limit=N` — transcript history for participants
/// (public, no auth). Utterances in chronological order so a late joiner can read
/// what was said before they arrived. Returns `[]` when `record_transcript` is off
/// (privacy default: nothing is persisted without explicit host opt-in).
/// Max 200 rows; default 100.
pub async fn list_public_transcript(
    State(state): State<AppState>,
    Path(code): Path<String>,
    axum::extract::Query(q): axum::extract::Query<crate::webinar::chat::HistoryQuery>,
    // Extension<GuestId> from the guest-session middleware (unused here, but the
    // middleware runs for the whole public sub-router).
    _guest: Extension<GuestId>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let w = find_by_code(pool, &code)
        .await
        .map_err(db_err)?
        .ok_or_else(|| not_found("webinar not found"))?;

    // Privacy gate: transcript not recorded → nothing to serve.
    if !w.record_transcript {
        return Ok(Json(json!([])).into_response());
    }

    // Clamp to 200 max (generous for catch-up; the host endpoint allows 2 000).
    let limit = q.limit.unwrap_or(100).clamp(1, 200);
    let rows: Vec<(String, String, Value, DateTime<Utc>)> = sqlx::query_as(
        "SELECT original_text, original_lang, translations, spoken_at
           FROM webinar_transcripts
          WHERE webinar_id = $1
          ORDER BY spoken_at ASC
          LIMIT $2",
    )
    .bind(w.id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;

    let view: Vec<Value> = rows
        .into_iter()
        .map(|(original, lang, translations, spoken_at)| {
            json!({
                "original": original,
                "lang": lang,
                "translations": translations,
                "spoken_at": spoken_at.to_rfc3339(),
            })
        })
        .collect();
    Ok(Json(view).into_response())
}

/// `GET /api/webinars/{id}/transcripts` — return up to 2 000 transcript rows for
/// a webinar, ordered chronologically. Members only (same gate as `get_one`).
/// Returns an empty array when `record_transcript` was not enabled.
pub async fn get_transcripts(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_webinar_role(pool, id, user.user_id, MEMBER).await?;
    let rows: Vec<serde_json::Value> = sqlx::query_as::<
        _,
        (
            String,
            String,
            serde_json::Value,
            chrono::DateTime<chrono::Utc>,
        ),
    >(
        "SELECT original_text, original_lang, translations, spoken_at
           FROM webinar_transcripts
          WHERE webinar_id = $1
          ORDER BY spoken_at ASC
          LIMIT 2000",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?
    .into_iter()
    .map(|(original_text, original_lang, translations, spoken_at)| {
        serde_json::json!({
            "original_text": original_text,
            "original_lang": original_lang,
            "translations": translations,
            "spoken_at": spoken_at,
        })
    })
    .collect();
    Ok(Json(rows).into_response())
}

// ---- AI recap: report + multilingual participant emails (Phase B) ----------
//
// HOST-ONLY (caller == webinar.host_user_id; 403 otherwise), mirroring the MEET
// AI system: the shared Groq generation (`crate::ai::report` / `email_draft`)
// and the async `ai_jobs` claim pattern, sourced from `webinar_transcripts` and
// charged to the host's ORG credit pool. NO sentiment analysis.

use crate::ai::jobs as ai_jobs;
use crate::transcripts::TranscriptExport;
use crate::webinar::ai as webinar_ai;

/// Validate an AI report/email target language (same shape as the meet report:
/// short, alnum + `-`). Empty/absent is caller's problem to default before here.
fn valid_ai_lang(raw: &str) -> Result<String, Response> {
    let l = raw.trim();
    if l.is_empty() || l.len() > 8 || !l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(bad_request("invalid lang"));
    }
    Ok(l.to_string())
}

/// Fetch a webinar and enforce HOST-ONLY access: the caller must be the webinar's
/// `host_user_id`. 404 when the webinar doesn't exist; 403 when the caller isn't
/// the host. Stricter than [`require_webinar_role`] (any org member) — the recap
/// endpoints are the host's alone (SPEC: host-only + the host org pays).
async fn require_webinar_host(pool: &Pool, id: Uuid, user_id: Uuid) -> Result<Webinar, Response> {
    let w = find_by_id(pool, id)
        .await
        .map_err(db_err)?
        .ok_or_else(|| not_found("webinar not found"))?;
    if w.host_user_id != Some(user_id) {
        return Err((StatusCode::FORBIDDEN, "host only").into_response());
    }
    Ok(w)
}

/// Load a webinar's transcript rows (original + translations) in `spoken_at`
/// order for AI assembly. Empty when `record_transcript` was off.
async fn load_transcript_rows(
    pool: &Pool,
    webinar_id: Uuid,
) -> Result<Vec<webinar_ai::TranscriptRow>, sqlx::Error> {
    let rows: Vec<(String, String, Value, DateTime<Utc>)> = sqlx::query_as(
        "SELECT original_text, original_lang, translations, spoken_at
           FROM webinar_transcripts
          WHERE webinar_id = $1
          ORDER BY spoken_at ASC
          LIMIT 20000",
    )
    .bind(webinar_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(original_text, original_lang, translations, spoken_at)| webinar_ai::TranscriptRow {
                original_text,
                original_lang,
                translations,
                spoken_at,
            },
        )
        .collect())
}

#[derive(Deserialize)]
pub struct WebinarReportRequest {
    /// Target language (any). Defaults to the webinar's source language.
    #[serde(default)]
    pub lang: Option<String>,
}

/// `POST /api/webinars/{id}/ai/report` — generate an AI recap report for a
/// finished webinar in `lang` (host-only). Assembles the transcript from
/// `webinar_transcripts` (target-language translation when present, else the
/// original), runs the shared MEET report generation, stores it (one per
/// (webinar, lang) — a re-run upserts, never re-charges), and charges the host's
/// org. Async job: a re-click joins the running job (`ai_jobs` claim pattern).
pub async fn ai_report(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<WebinarReportRequest>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_cfg(&state)?;
    let Some(billing_cfg) = state.config.billing.as_ref() else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "billing not configured").into_response());
    };
    let w = require_webinar_host(pool, id, user.user_id).await?;

    let lang = match body.lang.as_deref().map(str::trim) {
        Some(l) if !l.is_empty() => valid_ai_lang(l)?,
        _ => valid_ai_lang(webinar_ai::default_report_lang(&w))?,
    };

    // Assemble the transcript; refuse when there is nothing to report on.
    let rows = load_transcript_rows(pool, id).await.map_err(db_err)?;
    if rows.is_empty() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "webinar has no transcript to report on",
        )
            .into_response());
    }
    let export = webinar_ai::assemble_export(
        &rows,
        &lang,
        &w.title,
        w.actual_start.unwrap_or(w.created_at),
        w.actual_end,
    );
    let cost = webinar_ai::report_cost_credits(&billing_cfg.ai, export.session.duration_seconds);

    // One live slot per (webinar, lang): a re-click joins the running job.
    let job_id = match ai_jobs::claim_webinar(pool, id, user.user_id, "report", &lang).await {
        Ok(ai_jobs::Claim::Owned(job_id)) => {
            let st = state.clone();
            let (wid, host_org, host_user, lng) = (id, w.org_id, user.user_id, lang.clone());
            tokio::spawn(ai_jobs::run_webinar(pool.clone(), job_id, async move {
                run_webinar_report_inner(st, wid, host_org, host_user, export, cost, lng).await
            }));
            job_id
        }
        Ok(ai_jobs::Claim::AlreadyRunning(job_id)) => job_id,
        Err(e) => {
            tracing::error!("webinar report job claim failed: {e}");
            return Err(db_err(e));
        }
    };
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "job_id": job_id, "status": "pending" })),
    )
        .into_response())
}

/// Background body of [`ai_report`]: generate → charge the host org → persist,
/// returning the JSON the poll endpoint serves. User-favorable failure policy
/// mirrors the meet report: Groq failure never charges; a persist error after a
/// charge still returns the markdown.
async fn run_webinar_report_inner(
    state: AppState,
    webinar_id: Uuid,
    org_id: Uuid,
    host_user_id: Uuid,
    export: TranscriptExport,
    cost_credits: i32,
    lang: String,
) -> Result<serde_json::Value, ai_jobs::JobFailure> {
    let (Some(pool), Some(cfg)) = (state.pool.as_ref(), state.config.billing.as_ref()) else {
        return Err(ai_jobs::JobFailure::new("unavailable"));
    };
    let ai = &cfg.ai;

    // Structured report; the section headings are localized into `lang` by the
    // shared prompt. No guidelines from this endpoint.
    let (markdown, model) = match crate::ai::report::generate_report(
        &state.groq,
        ai,
        &export,
        "structured",
        &lang,
        None,
    )
    .await
    {
        Ok(out) => out,
        Err(e) => {
            tracing::error!("webinar report generation failed: {e}");
            return Err(ai_jobs::JobFailure::new("groq"));
        }
    };

    // Charge the host org (idempotency: the upsert below is keyed on (webinar,
    // lang); a re-run replaces the row but we still charge per successful
    // generation — the claim dedup prevents a rapid double-click double-charge).
    if cost_credits > 0 {
        let desc = format!("webinar AI report ({lang}) — {webinar_id}");
        match crate::business::credits::deduct_org_credits(
            pool,
            org_id,
            cost_credits,
            "webinar_ai_report",
            None,
            Some(host_user_id),
            &desc,
        )
        .await
        {
            Ok(crate::business::credits::OrgCharge::Charged { .. }) => {}
            Ok(crate::business::credits::OrgCharge::Insufficient { balance, required }) => {
                return Err(ai_jobs::JobFailure::with_payload(
                    "insufficient_credits",
                    json!({ "error": "insufficient_credits", "balance": balance, "required": required }),
                ));
            }
            Err(e) => {
                // Our own infra error after generation → deliver the report free.
                tracing::error!("webinar report org charge failed — delivering free: {e}");
            }
        }
    }

    // Persist (upsert on (webinar, lang) so a regenerate replaces the stored one).
    let row: Result<(Uuid, DateTime<Utc>), sqlx::Error> = sqlx::query_as(
        "INSERT INTO webinar_reports
             (webinar_id, user_id, format, lang, markdown, model, cost_credits)
         VALUES ($1, $2, 'structured', $3, $4, $5, $6)
         ON CONFLICT (webinar_id, lang) DO UPDATE
           SET markdown = EXCLUDED.markdown, model = EXCLUDED.model,
               cost_credits = EXCLUDED.cost_credits, user_id = EXCLUDED.user_id,
               updated_at = now()
         RETURNING id, created_at",
    )
    .bind(webinar_id)
    .bind(host_user_id)
    .bind(&lang)
    .bind(&markdown)
    .bind(&model)
    .bind(cost_credits)
    .fetch_one(pool)
    .await;

    let mut v = json!({
        "lang": lang,
        "markdown": markdown,
        "model": model,
        "cost_credits": cost_credits,
    });
    match row {
        Ok((report_id, created_at)) => {
            v["id"] = json!(report_id);
            v["created_at"] = json!(created_at);
        }
        Err(e) => {
            // Charged but couldn't persist — deliver the markdown anyway.
            tracing::error!("webinar report insert failed after charge: {e}");
        }
    }
    Ok(v)
}

/// `GET /api/webinars/{id}/ai/report` — the latest stored report for a webinar
/// (host-only). Optional `?lang=` returns that language's report; without it, the
/// most recent. 200 + null when none exists yet.
pub async fn ai_report_latest(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_webinar_host(pool, id, user.user_id).await?;
    let row: Option<(Uuid, String, String, String, i32, DateTime<Utc>)> = match q.get("lang") {
        Some(lang) => sqlx::query_as(
            "SELECT id, lang, markdown, model, cost_credits, created_at
               FROM webinar_reports WHERE webinar_id = $1 AND lang = $2",
        )
        .bind(id)
        .bind(lang)
        .fetch_optional(pool)
        .await
        .map_err(db_err)?,
        None => sqlx::query_as(
            "SELECT id, lang, markdown, model, cost_credits, created_at
               FROM webinar_reports WHERE webinar_id = $1
              ORDER BY created_at DESC LIMIT 1",
        )
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(db_err)?,
    };
    let body = match row {
        Some((rid, lang, markdown, model, cost, created_at)) => json!({
            "id": rid, "lang": lang, "markdown": markdown, "model": model,
            "cost_credits": cost, "created_at": created_at,
        }),
        None => Value::Null,
    };
    Ok(Json(body).into_response())
}

/// `GET /api/webinars/{id}/ai/job/{job_id}` — poll a webinar AI job (host-only).
pub async fn ai_job_status(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, job_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_webinar_host(pool, id, user.user_id).await?;
    match ai_jobs::get_webinar(pool, job_id, id).await {
        Ok(Some(job)) => Ok(Json(json!({
            "id": job.id,
            "status": job.status,
            "error": job.error,
            "result": job.result_json,
            "created_at": job.created_at,
        }))
        .into_response()),
        Ok(None) => Err(not_found("no such job")),
        Err(e) => Err(db_err(e)),
    }
}

/// `POST /api/webinars/{id}/ai/email` — generate + send a recap email to EACH
/// participant in THEIR language (host-only). For every participant with a
/// resolvable account email: language = `users.language` (logged-in) else
/// `webinar_participants.language_code` (guest) else the webinar's source
/// language. Content is generated ONCE per language (grouped), then sent via
/// Resend to each recipient. Dedup per (webinar, participant) so a re-run never
/// double-sends. Guests without an email are skipped. Charges the host org the
/// per-email cost for each email sent. Async job.
pub async fn ai_email(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_cfg(&state)?;
    if state.config.billing.is_none() {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "billing not configured").into_response());
    }
    if state.resend.is_none() {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "email not configured").into_response());
    }
    let w = require_webinar_host(pool, id, user.user_id).await?;

    // Something to recap on: no transcript → nothing to send.
    let rows = load_transcript_rows(pool, id).await.map_err(db_err)?;
    if rows.is_empty() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "webinar has no transcript to recap",
        )
            .into_response());
    }

    // One live slot per webinar email fan-out (params_key is empty — the run
    // covers every participant), so a re-click joins the running job.
    let job_id = match ai_jobs::claim_webinar(pool, id, user.user_id, "email", "").await {
        Ok(ai_jobs::Claim::Owned(job_id)) => {
            let st = state.clone();
            tokio::spawn(ai_jobs::run_webinar(pool.clone(), job_id, async move {
                run_webinar_email_inner(st, w, user.user_id).await
            }));
            job_id
        }
        Ok(ai_jobs::Claim::AlreadyRunning(job_id)) => job_id,
        Err(e) => {
            tracing::error!("webinar email job claim failed: {e}");
            return Err(db_err(e));
        }
    };
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "job_id": job_id, "status": "pending" })),
    )
        .into_response())
}

/// Background body of [`ai_email`]: plan targets → per-language generate → send
/// via Resend → dedup-persist + charge, once per email actually sent. Returns an
/// aggregate summary (counts + per-language breakdown) — NEVER any recipient
/// address (privacy). User-favorable: a Groq/Resend failure for one participant
/// doesn't sink the batch; only successfully-sent, newly-deduped emails charge.
async fn run_webinar_email_inner(
    state: AppState,
    w: Webinar,
    host_user_id: Uuid,
) -> Result<serde_json::Value, ai_jobs::JobFailure> {
    let (Some(pool), Some(cfg), Some(resend)) = (
        state.pool.as_ref(),
        state.config.billing.as_ref(),
        state.resend.as_ref(),
    ) else {
        return Err(ai_jobs::JobFailure::new("unavailable"));
    };
    let ai = &cfg.ai;
    let per_email_cost = webinar_ai::email_cost_credits(ai);
    let fallback_lang = w.source_language.clone();

    // Resolve every participant's language + account email, server-side.
    // `(participant_id, users.language, participant.language_code, users.email)`.
    type ParticipantEmailRow = (Uuid, Option<String>, Option<String>, Option<String>);
    let prows: Vec<ParticipantEmailRow> = sqlx::query_as(
        "SELECT p.id, u.language, p.language_code, u.email
           FROM webinar_participants p
           LEFT JOIN users u ON u.id = p.user_id
          WHERE p.webinar_id = $1",
    )
    .bind(w.id)
    .fetch_all(pool)
    .await
    .map_err(|e| {
        tracing::error!("webinar participant load failed: {e}");
        ai_jobs::JobFailure::new("db")
    })?;
    let participants: Vec<webinar_ai::ParticipantRow> = prows
        .into_iter()
        .map(
            |(participant_id, user_language, participant_language, email)| {
                webinar_ai::ParticipantRow {
                    participant_id,
                    user_language,
                    participant_language,
                    email,
                }
            },
        )
        .collect();

    let targets = webinar_ai::plan_email_targets(&participants, &fallback_lang);
    if targets.is_empty() {
        return Ok(json!({
            "sent": 0, "skipped": 0, "failed": 0,
            "note": "no participants with a resolvable email address",
        }));
    }

    // Transcript, assembled per language below (a translation may differ per lang).
    let rows = load_transcript_rows(pool, w.id).await.map_err(|e| {
        tracing::error!("webinar transcript load failed: {e}");
        ai_jobs::JobFailure::new("db")
    })?;

    let grouped = webinar_ai::group_targets_by_lang(&targets);
    let tagline_base = &state.config.app_base_url;

    let mut sent = 0i64;
    let mut skipped = 0i64;
    let mut failed = 0i64;
    let mut per_lang: serde_json::Map<String, Value> = serde_json::Map::new();

    for (lang, lang_targets) in &grouped {
        // Generate the recap email ONCE for this language (content is shared).
        let export = webinar_ai::assemble_export(
            &rows,
            lang,
            &w.title,
            w.actual_start.unwrap_or(w.created_at),
            w.actual_end,
        );
        let (draft, _model) = match crate::ai::email_draft::generate_draft(
            &state.groq,
            ai,
            &export,
            None,
            None,
            lang,
            None,
        )
        .await
        {
            Ok(out) => out,
            Err(e) => {
                tracing::error!("webinar recap email generation failed for {lang}: {e}");
                failed += lang_targets.len() as i64;
                per_lang.insert(lang.clone(), json!({ "failed": lang_targets.len() }));
                continue;
            }
        };

        // Branded shell (built once per language); body_html is rebuilt from the
        // model's plain text via the escaping helper — never raw model HTML.
        let inner_html = crate::ai::email_draft::text_to_html(&draft.body_text);
        let tagline = crate::email_template::tagline(lang);
        let html = crate::email_template::render_html(&crate::email_template::EmailLayout {
            app_base_url: tagline_base,
            preheader: &draft.subject,
            heading: None,
            body_html: &inner_html,
            button: None,
            tagline,
        });
        let text =
            crate::email_template::render_text(&draft.body_text, None, tagline_base, tagline);

        let mut lang_sent = 0i64;
        let mut lang_skipped = 0i64;
        let mut lang_failed = 0i64;

        for t in lang_targets {
            // Dedup + charge atomically per (webinar, participant): claim the row
            // FIRST (ON CONFLICT DO NOTHING). If we didn't insert, this
            // participant was already emailed — skip (no re-send, no re-charge).
            // A prior `failed` row is reclaimed (retry); a `sent` row is not (the
            // WHERE guard makes the upsert a no-op → no RETURNING → skipped).
            let claimed: Option<(Uuid,)> = match sqlx::query_as(
                "INSERT INTO webinar_emails (webinar_id, participant_id, lang, subject, status)
                 VALUES ($1, $2, $3, $4, 'sent')
                 ON CONFLICT (webinar_id, participant_id) DO UPDATE
                   SET lang = EXCLUDED.lang, subject = EXCLUDED.subject,
                       status = 'sent', error = NULL, resend_id = NULL
                   WHERE webinar_emails.status = 'failed'
                 RETURNING id",
            )
            .bind(w.id)
            .bind(t.participant_id)
            .bind(lang)
            .bind(&draft.subject)
            .fetch_optional(pool)
            .await
            {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!("webinar_emails claim failed for {}: {e}", t.participant_id);
                    lang_failed += 1;
                    continue;
                }
            };
            let Some((email_row_id,)) = claimed else {
                lang_skipped += 1;
                continue; // already sent to this participant
            };

            let outbound = crate::email::OutboundEmail {
                to: vec![t.email.clone()],
                cc: vec![],
                subject: draft.subject.clone(),
                html: html.clone(),
                text: text.clone(),
            };
            match resend.send(&outbound).await {
                Ok(resend_id) => {
                    let _ = sqlx::query("UPDATE webinar_emails SET resend_id = $2 WHERE id = $1")
                        .bind(email_row_id)
                        .bind(&resend_id)
                        .execute(pool)
                        .await;
                    // Charge the host org per delivered email.
                    if per_email_cost > 0 {
                        let desc = format!("webinar AI recap email ({lang}) — {}", w.id);
                        if let Err(e) = crate::business::credits::deduct_org_credits(
                            pool,
                            w.org_id,
                            per_email_cost,
                            "webinar_ai_email",
                            None,
                            Some(host_user_id),
                            &desc,
                        )
                        .await
                        {
                            tracing::error!("webinar recap email org charge failed: {e}");
                        }
                    }
                    lang_sent += 1;
                }
                Err(e) => {
                    tracing::error!(
                        "webinar recap email send failed for {}: {e}",
                        t.participant_id
                    );
                    // Flip the claimed row to failed so a later re-run retries it
                    // (the UNIQUE row otherwise blocks a retry forever).
                    let _ = sqlx::query(
                        "UPDATE webinar_emails SET status = 'failed', error = $2 WHERE id = $1",
                    )
                    .bind(email_row_id)
                    .bind(e.chars().take(500).collect::<String>())
                    .execute(pool)
                    .await;
                    lang_failed += 1;
                }
            }
        }

        sent += lang_sent;
        skipped += lang_skipped;
        failed += lang_failed;
        per_lang.insert(
            lang.clone(),
            json!({ "sent": lang_sent, "skipped": lang_skipped, "failed": lang_failed }),
        );
    }

    Ok(json!({
        "sent": sent,
        "skipped": skipped,
        "failed": failed,
        "languages": Value::Object(per_lang),
    }))
}

/// Map a Google OAuth error to a client response (409 tells the client to connect
/// their Google Calendar first).
fn calendar_token_err(e: OauthError) -> Response {
    match e {
        OauthError::NotConfigured | OauthError::NoConnection => {
            (StatusCode::CONFLICT, "connect your Google Calendar first").into_response()
        }
        other => {
            tracing::error!("webinar calendar token error: {other}");
            (StatusCode::BAD_GATEWAY, "calendar error").into_response()
        }
    }
}

/// `POST /api/webinars/{id}/calendar` — create (or re-sync) a Google Calendar event
/// for a scheduled webinar on the host's primary calendar, carrying the join link,
/// and store the event id (#7). Host (org member) only; needs a scheduled start and a
/// connected Google Calendar (409 otherwise).
pub async fn add_to_calendar(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let w = require_webinar_role(pool, id, user.user_id, MEMBER).await?;
    let Some(start) = w.scheduled_start else {
        return Err(bad_request("webinar has no scheduled start"));
    };
    let end = w.scheduled_end.unwrap_or(start + ChronoDuration::hours(1));
    let join = join_url(&state.config.app_base_url, &w.code);
    let description = match w.description.as_deref() {
        Some(d) if !d.trim().is_empty() => format!("{d}\n\nJoin: {join}"),
        _ => format!("Join: {join}"),
    };

    let access = google_oauth::valid_access_token(&state, user.user_id)
        .await
        .map_err(calendar_token_err)?;

    let mut props = HashMap::new();
    props.insert("webinar_id".to_string(), w.id.to_string());
    props.insert("webinar_code".to_string(), w.code.clone());
    let input = EventInput {
        summary: w.title.clone(),
        description: Some(description),
        start_rfc3339: start.to_rfc3339(),
        end_rfc3339: end.to_rfc3339(),
        timezone: "UTC".to_string(),
        attendee_emails: vec![],
        private_props: props,
        recurrence: None,
    };

    // Re-syncing an already-scheduled webinar updates the existing event.
    let event = match w.google_event_id.as_deref() {
        Some(eid) => {
            google_calendar::update_event(&state.http, &access, "primary", eid, &input).await
        }
        None => google_calendar::create_event(&state.http, &access, "primary", &input).await,
    }
    .map_err(|e| {
        tracing::error!("webinar calendar event failed: {e}");
        (
            StatusCode::BAD_GATEWAY,
            "could not create the calendar event",
        )
            .into_response()
    })?;

    sqlx::query("UPDATE webinars SET google_event_id = $1, updated_at = now() WHERE id = $2")
        .bind(&event.id)
        .bind(id)
        .execute(pool)
        .await
        .map_err(db_err)?;

    Ok(Json(json!({
        "google_event_id": event.id,
        "html_link": event.html_link,
        "join_url": join,
    }))
    .into_response())
}

/// `GET /api/w/{code}` — public, auth-free resolution for guests. NO host PII.
/// Echoes the guest's `guest_id` (F0-4) so the client can mirror it to localStorage.
pub async fn public_get(
    State(state): State<AppState>,
    Extension(guest): Extension<GuestId>,
    Path(code): Path<String>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let cfg = require_cfg(&state)?;
    let w = find_by_code(pool, &code)
        .await
        .map_err(db_err)?
        .ok_or_else(|| not_found("webinar not found"))?;
    // A cancelled webinar is not publicly resolvable.
    if w.status == "cancelled" {
        return Err(not_found("webinar not found"));
    }
    let mut body = public_view(&w, &state.config.app_base_url, cfg);
    if let Some(obj) = body.as_object_mut() {
        obj.insert("guest_id".into(), json!(guest.0));
    }
    Ok(Json(body).into_response())
}

/// `GET /api/webinars/public` — the public, auth-free discovery list. Returns
/// every `public`, upcoming-or-live, non-archived webinar (the client renders
/// these on the home page next to the public rooms). Each item is PII-free and
/// carries a LIVE audience `viewers` count. Private, cancelled/ended, and
/// archived webinars are never listed. Envelope: `{ "webinars": [ … ] }`.
///
/// Auth is OPTIONAL. Logged-in callers additionally have their own orgs'
/// webinars excluded (they manage those via the host hub, not the discovery
/// list). Unauthenticated guests receive the full public list.
pub async fn public_list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_cfg(&state)?;

    // Optional auth: extract caller_id when a valid JWT is present so we can
    // exclude their own orgs' webinars from the discovery list.
    let caller_id: Option<Uuid> = state.config.billing.as_ref().and_then(|b| {
        headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .and_then(|tok| crate::auth::verify_jwt(&b.jwt_secret, tok).ok())
            .and_then(|c| Uuid::parse_str(&c.sub).ok())
    });

    let rows: Vec<Webinar> = if let Some(uid) = caller_id {
        sqlx::query_as(
            "SELECT * FROM webinars
             WHERE visibility = 'public'
               AND status IN ('scheduled', 'live')
               AND archived_at IS NULL
               AND org_id NOT IN (
                   SELECT org_id FROM organization_members WHERE user_id = $1
               )
             ORDER BY (status = 'live') DESC, scheduled_start ASC NULLS LAST, created_at DESC
             LIMIT 100",
        )
        .bind(uid)
        .fetch_all(pool)
        .await
        .map_err(db_err)?
    } else {
        sqlx::query_as(
            "SELECT * FROM webinars
             WHERE visibility = 'public'
               AND status IN ('scheduled', 'live')
               AND archived_at IS NULL
             ORDER BY (status = 'live') DESC, scheduled_start ASC NULLS LAST, created_at DESC
             LIMIT 100",
        )
        .fetch_all(pool)
        .await
        .map_err(db_err)?
    };
    let items: Vec<Value> = rows
        .iter()
        .map(|w| {
            let viewers = state.webinar_presence.count(&w.code);
            public_list_item(w, &state.config.app_base_url, viewers)
        })
        .collect();
    Ok(Json(json!({ "webinars": items })).into_response())
}

/// `GET /api/w/{code}/tts-session` — mint a short-lived Cartesia TTS-only token for
/// Enhanced-tier webinar participants (including unauthenticated guests). Rate-limited
/// per webinar code. Returns 422 for Standard webinars (they use browser TTS), 404 if
/// the webinar doesn't exist, 410 if it's not active, 503 if Cartesia is unconfigured.
pub async fn tts_session(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let cartesia = state.config.cartesia.as_ref().ok_or_else(|| {
        (StatusCode::SERVICE_UNAVAILABLE, "cartesia not configured").into_response()
    })?;

    // Rate-limit: 20 mints per code per minute to prevent token farming.
    if !state
        .rate_limiter
        .allow(&format!("wbr-tts:{code}"), 20, Duration::from_secs(60))
    {
        return Err((StatusCode::TOO_MANY_REQUESTS, "too many requests").into_response());
    }

    let webinar: Option<Webinar> =
        sqlx::query_as("SELECT * FROM webinars WHERE code = $1 AND archived_at IS NULL LIMIT 1")
            .bind(&code)
            .fetch_optional(pool)
            .await
            .map_err(db_err)?;

    let webinar =
        webinar.ok_or_else(|| (StatusCode::NOT_FOUND, "webinar not found").into_response())?;

    if webinar.tier != "enhanced" {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "standard webinars use browser TTS",
        )
            .into_response());
    }

    if !matches!(webinar.status.as_str(), "scheduled" | "live") {
        return Err((StatusCode::GONE, "webinar is not active").into_response());
    }

    let (token, expires_at) = crate::api::mint_cartesia_token_tts_only(&state.http, cartesia)
        .await
        .map_err(|e| {
            tracing::error!("webinar tts-session mint failed: {e}");
            (StatusCode::BAD_GATEWAY, "cartesia unavailable").into_response()
        })?;

    Ok(Json(serde_json::json!({
        "token": token,
        "expires_at": expires_at,
        "cartesia_version": cartesia.version,
        "tts": {
            "endpoint": cartesia.tts_endpoint,
            "model": cartesia.tts_model,
        },
        "default_voice_id": cartesia.default_voice_id,
    }))
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn schedule_none_none_is_ok() {
        let now = utc("2030-01-01T00:00:00Z");
        assert!(valid_schedule(None, None, now).is_ok());
    }

    #[test]
    fn schedule_clearly_past_start_rejected() {
        let now = utc("2030-01-01T12:00:00Z");
        // 10 minutes before now — clearly past (beyond the 2-minute tolerance).
        let start = utc("2030-01-01T11:50:00Z");
        let err = valid_schedule(Some(start), None, now).unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn schedule_future_start_ok() {
        let now = utc("2030-01-01T12:00:00Z");
        let start = utc("2030-01-01T13:00:00Z");
        assert!(valid_schedule(Some(start), None, now).is_ok());
    }

    #[test]
    fn schedule_start_within_tolerance_ok() {
        let now = utc("2030-01-01T12:00:00Z");
        // 1 minute in the past — inside the 2-minute clock-skew tolerance.
        let start = utc("2030-01-01T11:59:00Z");
        assert!(valid_schedule(Some(start), None, now).is_ok());
    }

    #[test]
    fn schedule_end_before_start_rejected() {
        let now = utc("2030-01-01T00:00:00Z");
        let start = utc("2030-01-01T13:00:00Z");
        let end = utc("2030-01-01T12:00:00Z");
        let err = valid_schedule(Some(start), Some(end), now).unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn schedule_end_equal_to_start_rejected() {
        let now = utc("2030-01-01T00:00:00Z");
        let start = utc("2030-01-01T13:00:00Z");
        assert!(valid_schedule(Some(start), Some(start), now).is_err());
    }

    #[test]
    fn schedule_end_after_start_ok() {
        let now = utc("2030-01-01T00:00:00Z");
        let start = utc("2030-01-01T13:00:00Z");
        let end = utc("2030-01-01T14:00:00Z");
        assert!(valid_schedule(Some(start), Some(end), now).is_ok());
    }

    #[test]
    fn schedule_end_only_no_start_is_ok() {
        // No effective start to compare against → end alone can't be validated here.
        let now = utc("2030-01-01T00:00:00Z");
        let end = utc("2030-01-01T14:00:00Z");
        assert!(valid_schedule(None, Some(end), now).is_ok());
    }
}
