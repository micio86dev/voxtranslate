//! Configurable notifications (spec: scheduled meetings, Phase 1d).
//!
//! One entry point — [`notify`] — fans an event out to the channels the user has
//! left enabled: an in-app row (the bell/history), a branded email (Resend), and a
//! VAPID web-push to each registered browser. Push is additionally suppressed during
//! the user's quiet hours. A background [`run_reminder_scheduler`] loop emits
//! `meeting_reminder` events when a meeting's lead time is reached.

use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Timelike, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;
use web_push::{ContentEncoding, SubscriptionInfo, VapidSignatureBuilder, WebPushMessageBuilder};

use crate::business::{db_err, require_pool};
use crate::config::PushConfig;
use crate::email::OutboundEmail;
use crate::email_template::{render_html, tagline, EmailButton, EmailLayout};
use crate::middleware::AuthUser;
use crate::AppState;

/// Notification types (Phase 1). Friend/presence types plug in here in Phase 2.
pub const TYPES: [&str; 4] = [
    "meeting_invited",
    "meeting_reminder",
    "meeting_updated",
    "meeting_cancelled",
];
pub const CHANNELS: [&str; 3] = ["push", "email", "in_app"];

fn esc_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// --- Fan-out -------------------------------------------------------------------

/// Deliver a notification to `user_id` across every enabled channel. Best-effort:
/// a failure on one channel never blocks the others, and the call never panics.
pub async fn notify(
    state: &AppState,
    user_id: Uuid,
    kind: &str,
    lang: &str,
    title: &str,
    body: &str,
    data: Value,
) {
    let Some(pool) = state.pool.as_ref() else {
        return;
    };

    // Per-(type,channel) overrides; absence = enabled.
    let prefs: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT type, channel, enabled FROM notification_preferences WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let disabled = |channel: &str| {
        prefs
            .iter()
            .any(|(t, c, e)| t == kind && c == channel && !*e)
    };

    let setting: Option<(Option<i16>, Option<i16>, String)> = sqlx::query_as(
        "SELECT quiet_hours_start, quiet_hours_end, timezone FROM notification_settings WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    // In-app (bell/history).
    if !disabled("in_app") {
        let _ = sqlx::query(
            "INSERT INTO notifications (user_id, type, title, body, data) VALUES ($1,$2,$3,$4,$5)",
        )
        .bind(user_id)
        .bind(kind)
        .bind(title)
        .bind(body)
        .bind(&data)
        .execute(pool)
        .await;
    }

    // Email (Resend).
    if !disabled("email") {
        if let Some(resend) = state.resend.as_ref() {
            if let Ok(Some(email)) =
                sqlx::query_scalar::<_, String>("SELECT email FROM users WHERE id = $1")
                    .bind(user_id)
                    .fetch_optional(pool)
                    .await
            {
                let join = data.get("join_url").and_then(|v| v.as_str());
                let body_html = format!("<p>{}</p>", esc_html(body));
                let button = join.map(|u| EmailButton {
                    label: crate::notify_copy::join_label(lang),
                    url: u,
                });
                let html = render_html(&EmailLayout {
                    app_base_url: &state.config.app_base_url,
                    preheader: title,
                    heading: Some(title),
                    body_html: &body_html,
                    button,
                    tagline: tagline(lang),
                });
                let text = format!("{title}\n\n{body}\n\n{}", join.unwrap_or(""));
                let _ = resend
                    .send(&OutboundEmail {
                        to: vec![email],
                        cc: vec![],
                        subject: title.to_string(),
                        html,
                        text,
                    })
                    .await;
            }
        }
    }

    // Push (VAPID) — suppressed during quiet hours.
    if !disabled("push") && !in_quiet_hours(setting.as_ref()) {
        if let Some(push) = state.config.push.as_ref() {
            let subs: Vec<(Uuid, String, String, String)> = sqlx::query_as(
                "SELECT id, endpoint, p256dh, auth FROM user_push_subscriptions WHERE user_id = $1",
            )
            .bind(user_id)
            .fetch_all(pool)
            .await
            .unwrap_or_default();
            let payload = json!({ "title": title, "body": body, "data": data }).to_string();
            for (id, endpoint, p256dh, auth) in subs {
                match send_web_push(&state.http, push, &endpoint, &p256dh, &auth, &payload).await {
                    Ok(()) => {}
                    Err(PushSendError::Gone) => {
                        let _ = sqlx::query("DELETE FROM user_push_subscriptions WHERE id = $1")
                            .bind(id)
                            .execute(pool)
                            .await;
                    }
                    Err(e) => tracing::warn!("push send failed: {e:?}"),
                }
            }
        }
    }
}

fn in_quiet_hours(setting: Option<&(Option<i16>, Option<i16>, String)>) -> bool {
    let Some((Some(start), Some(end), tz)) = setting else {
        return false;
    };
    let tz: Tz = tz.parse().unwrap_or(chrono_tz::UTC);
    let hour = Utc::now().with_timezone(&tz).hour() as i16;
    if start <= end {
        hour >= *start && hour < *end
    } else {
        // Window wraps midnight (e.g. 22 → 7).
        hour >= *start || hour < *end
    }
}

#[derive(Debug)]
// Inner diagnostic data is surfaced via Debug logging only (`{e:?}`).
#[allow(dead_code)]
enum PushSendError {
    /// 404/410 — subscription is dead and should be deleted.
    Gone,
    Build(String),
    Http(String),
    Status(u16),
}

async fn send_web_push(
    http: &reqwest::Client,
    push: &PushConfig,
    endpoint: &str,
    p256dh: &str,
    auth: &str,
    payload: &str,
) -> Result<(), PushSendError> {
    let subscription =
        SubscriptionInfo::new(endpoint.to_string(), p256dh.to_string(), auth.to_string());
    let mut sig = VapidSignatureBuilder::from_base64(&push.vapid_private_key, &subscription)
        .map_err(|e| PushSendError::Build(e.to_string()))?;
    sig.add_claim("sub", push.vapid_subject.as_str());
    let signature = sig
        .build()
        .map_err(|e| PushSendError::Build(e.to_string()))?;

    let mut builder = WebPushMessageBuilder::new(&subscription);
    builder.set_payload(ContentEncoding::Aes128Gcm, payload.as_bytes());
    builder.set_vapid_signature(signature);
    let message = builder
        .build()
        .map_err(|e| PushSendError::Build(e.to_string()))?;

    let mut req = http
        .post(message.endpoint.to_string())
        .header("TTL", message.ttl.to_string());
    if let Some(p) = message.payload {
        req = req
            .header("Content-Encoding", p.content_encoding.to_str())
            .header("Content-Type", "application/octet-stream");
        for (k, v) in p.crypto_headers {
            req = req.header(k, v);
        }
        req = req.body(p.content);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| PushSendError::Http(e.to_string()))?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else if status.as_u16() == 404 || status.as_u16() == 410 {
        Err(PushSendError::Gone)
    } else {
        Err(PushSendError::Status(status.as_u16()))
    }
}

// --- Endpoints: push subscriptions --------------------------------------------

#[derive(Deserialize)]
pub struct SubscribeBody {
    endpoint: String,
    keys: SubscribeKeys,
    #[serde(default)]
    user_agent: Option<String>,
}
#[derive(Deserialize)]
pub struct SubscribeKeys {
    p256dh: String,
    auth: String,
}

/// `POST /api/push/subscribe`
pub async fn subscribe(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<SubscribeBody>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    sqlx::query(
        "INSERT INTO user_push_subscriptions (user_id, endpoint, p256dh, auth, user_agent)
         VALUES ($1,$2,$3,$4,$5)
         ON CONFLICT (user_id, endpoint) DO UPDATE SET
            p256dh = EXCLUDED.p256dh, auth = EXCLUDED.auth, user_agent = EXCLUDED.user_agent",
    )
    .bind(user.user_id)
    .bind(&body.endpoint)
    .bind(&body.keys.p256dh)
    .bind(&body.keys.auth)
    .bind(&body.user_agent)
    .execute(pool)
    .await
    .map_err(db_err)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
pub struct UnsubscribeBody {
    endpoint: String,
}

/// `DELETE /api/push/subscribe`
pub async fn unsubscribe(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<UnsubscribeBody>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    sqlx::query("DELETE FROM user_push_subscriptions WHERE user_id = $1 AND endpoint = $2")
        .bind(user.user_id)
        .bind(&body.endpoint)
        .execute(pool)
        .await
        .map_err(db_err)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `GET /api/push/vapid-public-key` — the key the browser needs to subscribe.
pub async fn vapid_public_key(State(state): State<AppState>) -> Response {
    match state.config.push.as_ref() {
        Some(p) => Json(json!({ "key": p.vapid_public_key })).into_response(),
        None => (StatusCode::SERVICE_UNAVAILABLE, "push not configured").into_response(),
    }
}

// --- Endpoints: preferences ----------------------------------------------------

#[derive(Serialize)]
struct PrefRow {
    r#type: String,
    channel: String,
    enabled: bool,
}

#[derive(Serialize)]
struct PrefsResponse {
    preferences: Vec<PrefRow>,
    quiet_hours_start: Option<i16>,
    quiet_hours_end: Option<i16>,
    timezone: String,
}

/// `GET /api/notifications/preferences` — effective matrix (type × channel) + quiet hours.
pub async fn get_preferences(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let overrides: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT type, channel, enabled FROM notification_preferences WHERE user_id = $1",
    )
    .bind(user.user_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    let lookup = |t: &str, c: &str| {
        overrides
            .iter()
            .find(|(ot, oc, _)| ot == t && oc == c)
            .map(|(_, _, e)| *e)
            .unwrap_or(true)
    };
    let mut preferences = Vec::new();
    for t in TYPES {
        for c in CHANNELS {
            preferences.push(PrefRow {
                r#type: t.to_string(),
                channel: c.to_string(),
                enabled: lookup(t, c),
            });
        }
    }
    let setting: Option<(Option<i16>, Option<i16>, String)> = sqlx::query_as(
        "SELECT quiet_hours_start, quiet_hours_end, timezone FROM notification_settings WHERE user_id = $1",
    )
    .bind(user.user_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    let (qs, qe, tz) = setting.unwrap_or((None, None, "UTC".to_string()));
    Ok(Json(PrefsResponse {
        preferences,
        quiet_hours_start: qs,
        quiet_hours_end: qe,
        timezone: tz,
    })
    .into_response())
}

#[derive(Deserialize)]
pub struct PrefsPatch {
    #[serde(default)]
    preferences: Vec<PrefPatchRow>,
    #[serde(default)]
    quiet_hours_start: Option<i16>,
    #[serde(default)]
    quiet_hours_end: Option<i16>,
    #[serde(default)]
    timezone: Option<String>,
}
#[derive(Deserialize)]
pub struct PrefPatchRow {
    r#type: String,
    channel: String,
    enabled: bool,
}

/// `PATCH /api/notifications/preferences`
pub async fn patch_preferences(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<PrefsPatch>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    for p in &body.preferences {
        if !TYPES.contains(&p.r#type.as_str()) || !CHANNELS.contains(&p.channel.as_str()) {
            continue;
        }
        sqlx::query(
            "INSERT INTO notification_preferences (user_id, type, channel, enabled)
             VALUES ($1,$2,$3,$4)
             ON CONFLICT (user_id, type, channel) DO UPDATE SET enabled = EXCLUDED.enabled",
        )
        .bind(user.user_id)
        .bind(&p.r#type)
        .bind(&p.channel)
        .bind(p.enabled)
        .execute(pool)
        .await
        .map_err(db_err)?;
    }
    if body.quiet_hours_start.is_some() || body.quiet_hours_end.is_some() || body.timezone.is_some()
    {
        sqlx::query(
            "INSERT INTO notification_settings (user_id, quiet_hours_start, quiet_hours_end, timezone, updated_at)
             VALUES ($1,$2,$3,COALESCE($4,'UTC'),now())
             ON CONFLICT (user_id) DO UPDATE SET
                quiet_hours_start = EXCLUDED.quiet_hours_start,
                quiet_hours_end = EXCLUDED.quiet_hours_end,
                timezone = COALESCE(EXCLUDED.timezone, notification_settings.timezone),
                updated_at = now()",
        )
        .bind(user.user_id)
        .bind(body.quiet_hours_start)
        .bind(body.quiet_hours_end)
        .bind(body.timezone.as_deref())
        .execute(pool)
        .await
        .map_err(db_err)?;
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

// --- Endpoints: in-app center --------------------------------------------------

#[derive(Serialize, sqlx::FromRow)]
struct NotificationRow {
    id: Uuid,
    r#type: String,
    title: String,
    body: String,
    data: Value,
    read_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

#[derive(Deserialize, Default)]
pub struct ListNotifQuery {
    unread: Option<bool>,
    limit: Option<i64>,
}

/// `GET /api/notifications?unread=&limit=`
pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<ListNotifQuery>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let rows: Vec<NotificationRow> = if q.unread.unwrap_or(false) {
        sqlx::query_as(
            "SELECT id, type, title, body, data, read_at, created_at FROM notifications
             WHERE user_id = $1 AND read_at IS NULL ORDER BY created_at DESC LIMIT $2",
        )
        .bind(user.user_id)
        .bind(limit)
        .fetch_all(pool)
        .await
    } else {
        sqlx::query_as(
            "SELECT id, type, title, body, data, read_at, created_at FROM notifications
             WHERE user_id = $1 ORDER BY created_at DESC LIMIT $2",
        )
        .bind(user.user_id)
        .bind(limit)
        .fetch_all(pool)
        .await
    }
    .map_err(db_err)?;
    let unread: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM notifications WHERE user_id = $1 AND read_at IS NULL",
    )
    .bind(user.user_id)
    .fetch_one(pool)
    .await
    .map_err(db_err)?;
    Ok(Json(json!({ "notifications": rows, "unread": unread })).into_response())
}

/// `POST /api/notifications/{id}/read`
pub async fn mark_read(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    sqlx::query("UPDATE notifications SET read_at = now() WHERE id = $1 AND user_id = $2 AND read_at IS NULL")
        .bind(id)
        .bind(user.user_id)
        .execute(pool)
        .await
        .map_err(db_err)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `POST /api/notifications/read-all`
pub async fn mark_all_read(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    sqlx::query("UPDATE notifications SET read_at = now() WHERE user_id = $1 AND read_at IS NULL")
        .bind(user.user_id)
        .execute(pool)
        .await
        .map_err(db_err)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// --- Reminder scheduler --------------------------------------------------------

#[derive(sqlx::FromRow)]
struct DueMeeting {
    id: Uuid,
    title: String,
    join_url: String,
    room_code: String,
    creator_user_id: Uuid,
    scheduled_at: DateTime<Utc>,
}

/// Background loop: every `interval`, claim meetings whose reminder lead-time has
/// arrived (atomic UPDATE … RETURNING, so it fires exactly once even with overlap)
/// and notify the creator + invitees with accounts.
pub async fn run_reminder_scheduler(state: AppState, interval: Duration) {
    let Some(pool) = state.pool.clone() else {
        return;
    };
    let mut tick = tokio::time::interval(interval);
    loop {
        tick.tick().await;
        let due: Vec<DueMeeting> = match sqlx::query_as(
            "UPDATE scheduled_meetings SET reminder_sent_at = now()
             WHERE id IN (
                 SELECT id FROM scheduled_meetings
                 WHERE status = 'scheduled' AND reminder_sent_at IS NULL
                   AND scheduled_at > now()
                   AND scheduled_at - (reminder_minutes_before * interval '1 minute') <= now()
                 FOR UPDATE SKIP LOCKED
             )
             RETURNING id, title, join_url, room_code, creator_user_id, scheduled_at",
        )
        .fetch_all(&pool)
        .await
        {
            Ok(rows) => rows,
            Err(e) => {
                tracing::error!("reminder scheduler query failed: {e}");
                continue;
            }
        };

        for m in due {
            // Recipients: the creator + every invitee that has an account.
            let mut recipients: Vec<Uuid> = vec![m.creator_user_id];
            if let Ok(ids) = sqlx::query_scalar::<_, Uuid>(
                "SELECT user_id FROM scheduled_meeting_invitees WHERE meeting_id = $1 AND user_id IS NOT NULL",
            )
            .bind(m.id)
            .fetch_all(&pool)
            .await
            {
                recipients.extend(ids);
            }
            recipients.sort();
            recipients.dedup();

            let data = json!({
                "meeting_id": m.id,
                "room_code": m.room_code,
                "join_url": m.join_url,
                "scheduled_at": m.scheduled_at,
            });
            for uid in recipients {
                // Each recipient gets the reminder in their own UI language.
                let lang = crate::notify_copy::user_locale(&pool, uid).await;
                let (title, body) =
                    crate::notify_copy::meeting_copy("meeting_reminder", &lang, &m.title);
                notify(
                    &state,
                    uid,
                    "meeting_reminder",
                    &lang,
                    &title,
                    &body,
                    data.clone(),
                )
                .await;
            }
        }
    }
}
