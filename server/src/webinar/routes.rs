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
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use std::collections::HashMap;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
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
    #[serde(default)]
    pub scheduled_start: Option<DateTime<Utc>>,
    #[serde(default)]
    pub scheduled_end: Option<DateTime<Utc>>,
    /// Optional project this webinar belongs to (must be in the same org).
    #[serde(default)]
    pub project_id: Option<Uuid>,
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
    pub scheduled_start: Option<DateTime<Utc>>,
    #[serde(default)]
    pub scheduled_end: Option<DateTime<Utc>>,
    #[serde(default)]
    pub project_id: Option<Uuid>,
}

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
        "project_id": w.project_id,
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
        "join_url": join_url(app_base_url, &w.code),
        "playback_url": playback_url(cfg, &w.code),
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
        scheduled_start: body.scheduled_start,
        scheduled_end: body.scheduled_end,
        project_id: body.project_id,
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
    if let Some(pid) = body.project_id {
        validate_project(pool, w.org_id, pid).await?;
    }
    // COALESCE keeps the existing value for any omitted field.
    let updated: Webinar = sqlx::query_as(
        "UPDATE webinars SET
            title             = COALESCE($2, title),
            description       = COALESCE($3, description),
            tier              = COALESCE($4, tier),
            record_video      = COALESCE($5, record_video),
            record_transcript = COALESCE($6, record_transcript),
            voice_clone       = COALESCE($7, voice_clone),
            scheduled_start   = COALESCE($8, scheduled_start),
            scheduled_end     = COALESCE($9, scheduled_end),
            project_id        = COALESCE($10, project_id),
            updated_at        = now()
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
    .bind(body.scheduled_start)
    .bind(body.scheduled_end)
    .bind(body.project_id)
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
    sqlx::query(
        "UPDATE webinars SET status = 'ended', actual_end = now(), updated_at = now()
         WHERE id = $1 AND status = 'live'",
    )
    .bind(id)
    .execute(pool)
    .await
    .map_err(db_err)?;
    let current = find_by_id(pool, id).await.map_err(db_err)?.unwrap_or(w);
    Ok(Json(host_view(&current, &state.config.app_base_url, cfg)).into_response())
}

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
