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
use crate::db;
use crate::email::OutboundEmail;
use crate::email_template::{render_html, tagline, EmailButton, EmailLayout};
use crate::middleware::AuthUser;
use crate::AppState;

/// Notification types. Phase 1 = meetings; Phase 2 = friends/presence (spec: friends).
pub const TYPES: [&str; 10] = [
    "meeting_invited",
    "meeting_reminder",
    "meeting_updated",
    "meeting_cancelled",
    "friend_request",
    "friend_accepted",
    "call_invite",
    "friend_active",
    // Webinar friend alerts (spec: webinar reminders): a public scheduled webinar is
    // about to start / an unscheduled public webinar just went live.
    "webinar_soon",
    "webinar_live",
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

/// Split unread notifications into the ones to keep and the stale `friend_active` alert
/// ids to retract. A "friend is in a public room" alert is only meaningful while that
/// friend is actually online — once they leave the room / close the app, the alert is a
/// phantom. Such rows are dropped from the response and their ids returned so the caller
/// can mark them read (they must never resurface). Pure over `is_online` so it unit-tests
/// without a DB or live rooms.
fn filter_live_friend_alerts(
    rows: Vec<NotificationRow>,
    is_online: impl Fn(Uuid) -> bool,
) -> (Vec<NotificationRow>, Vec<Uuid>) {
    let mut stale = Vec::new();
    let kept = rows
        .into_iter()
        .filter(|r| {
            if r.r#type != "friend_active" {
                return true;
            }
            let joiner_online = r
                .data
                .get("from_user_id")
                .and_then(Value::as_str)
                .and_then(|s| Uuid::parse_str(s).ok())
                .is_some_and(&is_online);
            if !joiner_online {
                stale.push(r.id);
            }
            joiner_online
        })
        .collect();
    (kept, stale)
}

/// `GET /api/notifications?unread=&limit=`
pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<ListNotifQuery>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let unread_view = q.unread.unwrap_or(false);
    let mut rows: Vec<NotificationRow> = if unread_view {
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

    // Retract phantom "friend is in a public room" alerts whose subject has since left /
    // gone offline, so the live banner never fires for an absent user. Only the unread
    // (banner) view is filtered — history keeps the record. Best-effort: a failed retract
    // just leaves the row unread for the next poll to re-evaluate.
    if unread_view {
        let (kept, stale) = filter_live_friend_alerts(rows, |uid| state.rooms.is_user_online(uid));
        rows = kept;
        if !stale.is_empty() {
            if let Err(e) =
                sqlx::query("UPDATE notifications SET read_at = now() WHERE id = ANY($1)")
                    .bind(&stale)
                    .execute(pool)
                    .await
            {
                tracing::warn!("retract stale friend_active alerts failed: {e}");
            }
        }
    }

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

// --- Webinar friend alerts -----------------------------------------------------

/// Notify a webinar HOST's accepted friends that the webinar is starting soon
/// (`webinar_soon`) or has just gone live (`webinar_live`). Best-effort + per-recipient
/// localized. Loads the host's display name for the copy and the host's accepted
/// friends from `friendships`; the join URL is `{app_base_url}/w/{code}` so the email
/// CTA works. Dedup (`webinars.reminder_sent_at`) is the caller's responsibility — the
/// go-live hook claims it before spawning, the scheduler claims it in its RETURNING.
pub async fn notify_webinar_friends(
    state: AppState,
    webinar_id: Uuid,
    host_user_id: Uuid,
    webinar_code: String,
    kind: &'static str,
) {
    let Some(pool) = state.pool.as_ref() else {
        return;
    };

    // Go-live path: claim the dedup marker here so a re-`publish-started` (idempotent)
    // never double-fires. The scheduler already claimed it via its atomic RETURNING,
    // so this UPDATE is a no-op there (row_affected == 0) and we keep going.
    if kind == "webinar_live" {
        let claimed = sqlx::query(
            "UPDATE webinars SET reminder_sent_at = now()
             WHERE id = $1 AND reminder_sent_at IS NULL",
        )
        .bind(webinar_id)
        .execute(pool)
        .await;
        match claimed {
            Ok(r) if r.rows_affected() == 0 => return, // already notified
            Ok(_) => {}
            Err(e) => {
                tracing::error!("webinar go-live dedup claim failed: {e}");
                return;
            }
        }
    }

    let host_name: String = sqlx::query_scalar("SELECT name FROM users WHERE id = $1")
        .bind(host_user_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();

    let friends: Vec<Uuid> = match sqlx::query_scalar(
        "SELECT CASE WHEN user_low = $1 THEN user_high ELSE user_low END
           FROM friendships
          WHERE status = 'accepted' AND (user_low = $1 OR user_high = $1)",
    )
    .bind(host_user_id)
    .fetch_all(pool)
    .await
    {
        Ok(f) => f,
        Err(e) => {
            tracing::error!("webinar friend alert: load friends failed: {e}");
            return;
        }
    };

    let join_url = format!(
        "{}/w/{}",
        state.config.app_base_url.trim_end_matches('/'),
        webinar_code
    );
    let data = json!({
        "webinar_id": webinar_id,
        "code": webinar_code,
        "join_url": join_url,
    });
    for friend_id in friends {
        let lang = crate::notify_copy::user_locale(pool, friend_id).await;
        let (title, body) = crate::notify_copy::webinar_copy(kind, &lang, &host_name);
        notify(&state, friend_id, kind, &lang, &title, &body, data.clone()).await;
    }
}

// --- Webinar reminder scheduler ------------------------------------------------

#[derive(sqlx::FromRow)]
struct DueWebinar {
    id: Uuid,
    code: String,
    host_user_id: Option<Uuid>,
}

/// Select webinar IDs that are due for a reminder at `now`.
///
/// Exposed so integration tests can call this directly and assert on its output —
/// editing the production WHERE clause will immediately break the tests
/// rather than silently passing a re-implemented copy of the predicate.
///
/// Does NOT stamp `reminder_sent_at`; the scheduler loop does that atomically
/// via `UPDATE … RETURNING`.
pub async fn select_due_webinar_reminders(
    pool: &db::Pool,
    now: DateTime<Utc>,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT id FROM webinars
         WHERE visibility = 'public'
           AND status = 'scheduled'
           AND archived_at IS NULL
           AND reminder_sent_at IS NULL
           AND scheduled_start IS NOT NULL
           AND scheduled_start > $1
           AND scheduled_start - (reminder_minutes_before * interval '1 minute') <= $1
           AND notify_friends = true",
    )
    .bind(now)
    .fetch_all(pool)
    .await
}

/// Background loop mirroring [`run_reminder_scheduler`] for PUBLIC scheduled webinars.
/// Every `interval`, atomically claim webinars whose reminder lead-time has arrived
/// (UPDATE … RETURNING stamps `reminder_sent_at`, so each fires exactly once even with
/// overlapping ticks) and alert each host's accepted friends with a `webinar_soon`
/// notification. Unscheduled public webinars are handled by the go-live hook instead.
pub async fn run_webinar_reminder_scheduler(state: AppState, interval: Duration) {
    let Some(pool) = state.pool.clone() else {
        return;
    };
    let mut tick = tokio::time::interval(interval);
    loop {
        tick.tick().await;
        let now = Utc::now();
        let due: Vec<DueWebinar> = match sqlx::query_as(
            "UPDATE webinars SET reminder_sent_at = now()
             WHERE id IN (
                 SELECT id FROM webinars
                 WHERE visibility = 'public'
                   AND status = 'scheduled'
                   AND archived_at IS NULL
                   AND reminder_sent_at IS NULL
                   AND scheduled_start IS NOT NULL
                   AND scheduled_start > $1
                   AND scheduled_start - (reminder_minutes_before * interval '1 minute') <= $1
                   AND notify_friends = true
                 FOR UPDATE SKIP LOCKED
             )
             RETURNING id, code, host_user_id",
        )
        .bind(now)
        .fetch_all(&pool)
        .await
        {
            Ok(rows) => rows,
            Err(e) => {
                tracing::error!("webinar reminder scheduler query failed: {e}");
                continue;
            }
        };

        for w in due {
            let Some(host_id) = w.host_user_id else {
                continue;
            };
            notify_webinar_friends(state.clone(), w.id, host_id, w.code, "webinar_soon").await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: Uuid, kind: &str, data: Value) -> NotificationRow {
        NotificationRow {
            id,
            r#type: kind.to_string(),
            title: String::new(),
            body: String::new(),
            data,
            read_at: None,
            created_at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
        }
    }

    #[test]
    fn keeps_live_alerts_and_non_friend_rows_retracts_offline_ones() {
        let online = Uuid::from_u128(1);
        let offline = Uuid::from_u128(2);
        let other = Uuid::from_u128(10);
        let live = Uuid::from_u128(11);
        let stale = Uuid::from_u128(12);

        let rows = vec![
            // Non-friend_active rows are always kept, regardless of presence.
            row(
                other,
                "call_invite",
                json!({ "from_user_id": offline.to_string() }),
            ),
            // friend_active whose joiner is still online → kept.
            row(
                live,
                "friend_active",
                json!({ "from_user_id": online.to_string() }),
            ),
            // friend_active whose joiner is gone → retracted.
            row(
                stale,
                "friend_active",
                json!({ "from_user_id": offline.to_string() }),
            ),
        ];

        let (kept, retracted) = filter_live_friend_alerts(rows, |uid| uid == online);

        assert_eq!(
            kept.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![other, live]
        );
        assert_eq!(retracted, vec![stale]);
    }

    #[test]
    fn retracts_friend_alert_with_missing_or_unparseable_from_user_id() {
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let rows = vec![
            row(a, "friend_active", json!({})),
            row(b, "friend_active", json!({ "from_user_id": "not-a-uuid" })),
        ];
        // Even with "everyone online", a row we can't resolve to a user is treated as stale.
        let (kept, retracted) = filter_live_friend_alerts(rows, |_| true);
        assert!(kept.is_empty());
        assert_eq!(retracted, vec![a, b]);
    }
}
