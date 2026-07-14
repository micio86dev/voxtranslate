//! Webinar auto-translated chat (SPEC "Webinar Mode" Feature ⑤) — the SEND path.
//!
//! An optional per-webinar chat where EVERYONE can send: the host (a valid
//! `Authorization: Bearer` JWT whose user is a member of the webinar's org) and
//! unauthenticated viewers (the `guest_id` cookie). Everyone READS messages
//! auto-translated into their own subtitle language — delivery is realtime over
//! the presence WS ([`crate::webinar::presence::broadcast_chat`], which reaches
//! host + all viewers + the sender's echo).
//!
//! # REST: `POST /api/w/{code}/chat`
//!
//! Registered on the PUBLIC sub-router so it inherits the [`guest_session`]
//! middleware — `Extension<GuestId>` is the trustworthy guest identity. Auth is
//! OPTIONAL: the `Authorization: Bearer` header is read manually and tolerated
//! when absent (mirrors the inline read in `stt.rs`). A valid JWT whose user is a
//! MEMBER of the webinar's org → `sender_kind="host"`, `sender_id=user_id`; else
//! `sender_kind="guest"`, `sender_id=GuestId.0`. `display_name` is cosmetic — a
//! guest can't spoof identity because the server-side cookie UUID is the anchor.
//!
//! Messages are PERSISTED whenever the webinar has `chat_enabled` (the gate is
//! that flag itself, independent of `record_transcript`).
//!
//! [`guest_session`]: crate::webinar::guest::guest_session

// Handlers return `Result<Response, Response>` so `?` short-circuits with a status
// code; the `Response` Err variant is intentionally large (mirrors `routes`).
#![allow(clippy::result_large_err)]

use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::business::{bad_request, db_err, not_found, require_pool, require_role, MEMBER};
use crate::db::Pool;
use crate::moderation::Severity;
use crate::webinar::guest::GuestId;
use crate::webinar::presence::ChatEvent;
use crate::webinar::presence::WvAttachment;
use crate::webinar::routes::valid_lang;
use crate::webinar::{find_by_code, Webinar};
use crate::AppState;

/// Max chat message length (chars). Longer bodies are a clean 400, not a truncation.
const MAX_TEXT_CHARS: usize = 500;
/// Max cosmetic display-name length (chars). Longer is clamped, not rejected.
const MAX_NAME_CHARS: usize = 40;
/// Default display name when none is supplied (or it clamps to empty).
const DEFAULT_NAME: &str = "Guest";
/// Per-sender chat rate limit: `RATE_MAX` messages per `RATE_WINDOW`.
const RATE_MAX: u32 = 5;
const RATE_WINDOW: Duration = Duration::from_secs(10);
/// Per-WEBINAR global chat rate limit (all senders combined). Because a guest
/// with no cookie is minted a fresh `guest_id` per request, the per-sender key can
/// be evaded by cookie rotation — this webinar-wide cap (keyed WITHOUT the sender)
/// bounds paid Groq fan-out per webinar regardless of how identity is derived. Set
/// well above any real chat cadence, low enough to cap a scripted flood.
const WEBINAR_RATE_MAX: u32 = 30;
const WEBINAR_RATE_WINDOW: Duration = Duration::from_secs(10);

/// The request body for `POST /api/w/{code}/chat`.
#[derive(Deserialize)]
pub struct ChatBody {
    /// The message text (validated: non-empty trimmed, ≤ 500 chars).
    pub text: String,
    /// Cosmetic display name; defaults to `"Guest"`, clamped to ≤ 40 chars.
    #[serde(default)]
    pub display_name: Option<String>,
    /// The sender's UI language — stored as `original_lang` (a label only; the
    /// "auto" fan-out drives the real translation keys). Falls back to `"auto"`.
    #[serde(default)]
    pub lang: Option<String>,
    #[serde(default)]
    pub avatar_url: Option<String>,
    #[serde(default)]
    pub attachment: Option<WvAttachment>,
}

/// Validate the chat text: trim, reject empty, reject over the length cap. Pure,
/// so the rule is unit-tested without a live request (mirrors `valid_title`).
pub(crate) fn valid_chat_text(raw: &str) -> Result<&str, Response> {
    let t = raw.trim();
    if t.is_empty() {
        return Err(bad_request("message text is required"));
    }
    if t.chars().count() > MAX_TEXT_CHARS {
        return Err(bad_request("message too long (max 500 chars)"));
    }
    Ok(t)
}

/// Normalize the cosmetic display name: trim, clamp to the cap, and fall back to
/// `"Guest"` when absent or empty. Pure + total (never fails) — a display name is
/// cosmetic, so it's sanitized, not rejected.
pub(crate) fn valid_display_name(raw: Option<&str>) -> String {
    let trimmed = raw.map(str::trim).unwrap_or("");
    if trimmed.is_empty() {
        return DEFAULT_NAME.to_string();
    }
    clamp_chars(trimmed, MAX_NAME_CHARS)
}

/// Truncate `s` to at most `max` characters (char-boundary safe).
fn clamp_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// `POST /api/w/{code}/chat` — send one chat message (host or guest).
///
/// Steps (each a hard gate): resolve the webinar (404) → chat must be enabled
/// (403) → validate text (400) → resolve identity (host if a member JWT, else the
/// guest cookie) → rate-limit per sender (429) → moderate; a severe message is
/// rejected (422) and NOT persisted → fan out the translation ("auto" source) →
/// persist (always, since chat is enabled) → broadcast to everyone. Returns
/// `200 { id, created_at }`.
pub async fn post_chat(
    State(state): State<AppState>,
    Extension(guest): Extension<GuestId>,
    Path(code): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ChatBody>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;

    // (1) Resolve the webinar — unknown code is a 404.
    let w = find_by_code(pool, &code)
        .await
        .map_err(db_err)?
        .ok_or_else(|| not_found("webinar not found"))?;

    // (2) Chat must be enabled for this webinar.
    if !w.chat_enabled {
        return Err((
            StatusCode::FORBIDDEN,
            "chat is not enabled for this webinar",
        )
            .into_response());
    }

    // (3) Validate the message + normalize the cosmetic display name.
    let avatar_url = body.avatar_url.as_deref().and_then(|u| {
        if u.starts_with("https://") {
            Some(u.to_string())
        } else {
            None
        }
    });
    let text = if body.attachment.is_some() && body.text.trim().is_empty() {
        String::new()
    } else {
        valid_chat_text(&body.text)?.to_string()
    };
    let display_name = valid_display_name(body.display_name.as_deref());
    // The sender's UI language is only a label; the "auto" fan-out below drives
    // the actual per-viewer translation keys. It is client-supplied and gets
    // persisted + re-broadcast verbatim, so validate its shape (reuse `valid_lang`
    // → `[A-Za-z0-9-]{1,32}`) and fall back to "auto" on anything malformed rather
    // than storing an arbitrary string.
    let original_lang = body
        .lang
        .as_deref()
        .and_then(|l| valid_lang(l).ok())
        .unwrap_or("auto")
        .to_string();

    // (4) Identity: a valid JWT whose user is a MEMBER of the webinar's org is the
    // host; anyone else (no/invalid token, or a non-member) is the guest.
    let (sender_kind, sender_id) = resolve_sender(&state, pool, &w, &headers, guest).await;

    // (5) Rate-limit. Two gates:
    //   (a) a webinar-wide cap keyed WITHOUT the sender — the real cost backstop,
    //       un-bypassable by cookie rotation (a cookieless guest is minted a fresh
    //       id per request, so a per-sender key alone would never collide);
    //   (b) a per-sender cap as the normal-use UX throttle.
    let global_key = format!("chat:{}", w.id);
    if !state
        .rate_limiter
        .allow(&global_key, WEBINAR_RATE_MAX, WEBINAR_RATE_WINDOW)
    {
        return Err((StatusCode::TOO_MANY_REQUESTS, "chat is busy, slow down").into_response());
    }
    let rl_key = format!("chat:{}:{}", w.id, sender_id);
    if !state.rate_limiter.allow(&rl_key, RATE_MAX, RATE_WINDOW) {
        return Err((StatusCode::TOO_MANY_REQUESTS, "slow down").into_response());
    }

    // (6) Moderation: a severe message is refused (422) and NEVER persisted or
    // broadcast — same posture as the P2P chat path.
    if state.moderator.severity(&text) == Severity::Severe {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, "message blocked").into_response());
    }

    // (7) Translate: "auto" lets Groq detect the source and translate into every
    // live viewer language. Always include the webinar's source_language so the
    // host sees participant messages translated into their language regardless of
    // whether any viewer happens to share it.
    let translations = if text.is_empty() {
        std::collections::HashMap::new()
    } else {
        let mut targets = state.webinar_presence.target_languages(&code);
        if !targets.iter().any(|t| t == &w.source_language) {
            targets.push(w.source_language.clone());
        }
        state
            .translator
            .translate_fanout(&text, "auto", &targets, None)
            .await
    };

    // (8) Persist — always, since chat is enabled (the gate is `chat_enabled`).
    let (id, created_at) = insert_chat_message(
        pool,
        w.id,
        &sender_kind,
        sender_id,
        &display_name,
        &text,
        &original_lang,
        &translations,
        avatar_url.as_deref(),
        body.attachment.as_ref(),
    )
    .await
    .map_err(db_err)?;

    // (9) Broadcast to everyone (host + viewers + this sender's echo).
    state.webinar_presence.broadcast_chat(
        &code,
        &ChatEvent {
            id,
            sender_kind,
            display_name,
            original: text,
            lang: original_lang,
            translations,
            created_at: created_at.to_rfc3339(),
            avatar_url,
            attachment: body.attachment.clone(),
        },
    );

    Ok(Json(json!({ "id": id, "created_at": created_at })).into_response())
}

/// Hard cap on how many chat rows a history fetch may return.
const MAX_HISTORY: i64 = 100;
/// Default history size when `limit` is absent.
const DEFAULT_HISTORY: i64 = 50;

/// Query params for the chat history fetch.
#[derive(Deserialize)]
pub struct HistoryQuery {
    #[serde(default)]
    pub limit: Option<i64>,
}

/// Clamp a requested `limit` into `1..=MAX_HISTORY`, defaulting when absent.
/// Pure, so the cap/default/floor is unit-tested without a request.
pub(crate) fn clamp_limit(requested: Option<i64>) -> i64 {
    match requested {
        None => DEFAULT_HISTORY,
        Some(n) => n.clamp(1, MAX_HISTORY),
    }
}

/// One chat history row as stored (never serialized directly to the client — the
/// public view is projected in [`history_view`] so `sender_id` can't leak).
#[derive(sqlx::FromRow)]
struct ChatRow {
    id: Uuid,
    sender_kind: String,
    display_name: String,
    original_text: String,
    original_lang: String,
    translations: serde_json::Value,
    created_at: chrono::DateTime<chrono::Utc>,
    avatar_url: Option<String>,
    attachment_url: Option<String>,
    attachment_name: Option<String>,
    attachment_content_type: Option<String>,
    attachment_size: Option<i64>,
}

/// Project a stored row into the PUBLIC history shape — mirrors the live
/// [`ChatEvent`] frame and carries ONLY `{ id, sender_kind, display_name,
/// original, lang, translations, created_at }`. NEVER `sender_id` or any PII.
fn history_view(r: &ChatRow) -> serde_json::Value {
    let attachment = match (
        &r.attachment_url,
        &r.attachment_name,
        &r.attachment_content_type,
        r.attachment_size,
    ) {
        (Some(url), Some(name), Some(ct), Some(size)) => json!({
            "url": url,
            "name": name,
            "content_type": ct,
            "size": size,
        }),
        _ => json!(null),
    };
    json!({
        "id": r.id,
        "sender_kind": r.sender_kind,
        "display_name": r.display_name,
        "original": r.original_text,
        "lang": r.original_lang,
        "translations": r.translations,
        "created_at": r.created_at.to_rfc3339(),
        "avatar_url": r.avatar_url,
        "attachment": attachment,
    })
}

/// `GET /api/w/{code}/chat?limit=N` — recent chat history (public, guest-visible).
///
/// Resolve the webinar (404) → when chat is disabled return an empty list (reading
/// is harmless, so it's `200 []`, not a 403) → select the most recent `limit`
/// rows (DESC), then reverse to chronological. The payload carries ONLY the public
/// projection — never `sender_id`.
pub async fn list_chat(
    State(state): State<AppState>,
    Path(code): Path<String>,
    axum::extract::Query(q): axum::extract::Query<HistoryQuery>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let w = find_by_code(pool, &code)
        .await
        .map_err(db_err)?
        .ok_or_else(|| not_found("webinar not found"))?;

    // Chat disabled → nothing to show; an empty list keeps the client simple.
    if !w.chat_enabled {
        return Ok(Json(json!([])).into_response());
    }

    let limit = clamp_limit(q.limit);
    // Most recent `limit` rows, newest first…
    let mut rows: Vec<ChatRow> = sqlx::query_as(
        "SELECT id, sender_kind, display_name, original_text, original_lang,
                translations, created_at,
                avatar_url, attachment_url, attachment_name, attachment_content_type, attachment_size
         FROM webinar_chat_messages
         WHERE webinar_id = $1
         ORDER BY created_at DESC
         LIMIT $2",
    )
    .bind(w.id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    // …reversed to chronological (oldest → newest) for rendering.
    rows.reverse();

    let view: Vec<serde_json::Value> = rows.iter().map(history_view).collect();
    Ok(Json(view).into_response())
}

/// Resolve `(sender_kind, sender_id)`. A valid `Authorization: Bearer` JWT whose
/// user is a MEMBER of the webinar's org → `("host", user_id)`; otherwise (no
/// header, malformed/invalid token, or a non-member) → `("guest", guest_id)`.
/// Auth is optional throughout — every failure quietly falls back to guest.
async fn resolve_sender(
    state: &AppState,
    pool: &Pool,
    w: &Webinar,
    headers: &HeaderMap,
    guest: GuestId,
) -> (String, Uuid) {
    if let Some(user_id) = host_user_id(state, pool, w, headers).await {
        return ("host".to_string(), user_id);
    }
    ("guest".to_string(), guest.0)
}

/// The host's user id IFF the request carries a valid `Bearer` JWT AND that user
/// is a member of the webinar's org. `None` on any failure (optional auth). The
/// inline Bearer read + `verify_jwt` mirrors `stt.rs`; `require_role`'s member
/// gate reuses the REST authz.
async fn host_user_id(
    state: &AppState,
    pool: &Pool,
    w: &Webinar,
    headers: &HeaderMap,
) -> Option<Uuid> {
    let billing = state.config.billing.as_ref()?;
    let token = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))?;
    let claims = crate::auth::verify_jwt(&billing.jwt_secret, token).ok()?;
    let user_id = Uuid::parse_str(&claims.sub).ok()?;
    // Member of the webinar's org? A non-member (or DB error) → not a host.
    match require_role(pool, w.org_id, user_id, MEMBER).await {
        Ok(_) => Some(user_id),
        Err(_) => None,
    }
}

/// Insert one chat message row and return its generated id + timestamp. Runtime
/// `sqlx::query_as` + `.bind()` only (never string interpolation).
#[allow(clippy::too_many_arguments)]
async fn insert_chat_message(
    pool: &Pool,
    webinar_id: Uuid,
    sender_kind: &str,
    sender_id: Uuid,
    display_name: &str,
    original_text: &str,
    original_lang: &str,
    translations: &std::collections::HashMap<String, String>,
    avatar_url: Option<&str>,
    attachment: Option<&WvAttachment>,
) -> Result<(Uuid, chrono::DateTime<chrono::Utc>), sqlx::Error> {
    let payload = serde_json::to_value(translations).unwrap_or_else(|_| json!({}));
    let (att_url, att_name, att_ct, att_size) = match attachment {
        Some(a) => (
            Some(a.url.as_str()),
            Some(a.name.as_str()),
            Some(a.content_type.as_str()),
            Some(a.size),
        ),
        None => (None, None, None, None),
    };
    sqlx::query_as(
        "INSERT INTO webinar_chat_messages
            (webinar_id, sender_kind, sender_id, display_name, original_text,
             original_lang, translations, avatar_url,
             attachment_url, attachment_name, attachment_content_type, attachment_size)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
         RETURNING id, created_at",
    )
    .bind(webinar_id)
    .bind(sender_kind)
    .bind(sender_id)
    .bind(display_name)
    .bind(original_text)
    .bind(original_lang)
    .bind(payload)
    .bind(avatar_url)
    .bind(att_url)
    .bind(att_name)
    .bind(att_ct)
    .bind(att_size)
    .fetch_one(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(r: &Response) -> StatusCode {
        r.status()
    }

    #[test]
    fn valid_chat_text_trims_and_rejects_empty() {
        assert_eq!(valid_chat_text("  hello  ").unwrap(), "hello");
        // Empty / whitespace-only → 400.
        assert_eq!(
            status(&valid_chat_text("").unwrap_err()),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status(&valid_chat_text("   ").unwrap_err()),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn valid_chat_text_enforces_length_cap() {
        // Exactly at the cap is accepted; one over is a 400.
        let ok = "a".repeat(MAX_TEXT_CHARS);
        assert_eq!(
            valid_chat_text(&ok).unwrap().chars().count(),
            MAX_TEXT_CHARS
        );
        let too_long = "a".repeat(MAX_TEXT_CHARS + 1);
        assert_eq!(
            status(&valid_chat_text(&too_long).unwrap_err()),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn clamp_limit_defaults_caps_and_floors() {
        // Absent → the default.
        assert_eq!(clamp_limit(None), DEFAULT_HISTORY);
        // A normal value passes through.
        assert_eq!(clamp_limit(Some(10)), 10);
        // Over the hard cap → clamped to MAX_HISTORY.
        assert_eq!(clamp_limit(Some(9999)), MAX_HISTORY);
        // Zero / negative → floored to 1 (never a non-positive LIMIT).
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(-5)), 1);
    }

    #[test]
    fn history_view_carries_no_sender_id_or_pii() {
        let row = ChatRow {
            id: Uuid::new_v4(),
            sender_kind: "guest".to_string(),
            display_name: "Guest".to_string(),
            original_text: "ciao".to_string(),
            original_lang: "it".to_string(),
            translations: json!({ "en": "hi" }),
            created_at: chrono::Utc::now(),
            avatar_url: None,
            attachment_url: None,
            attachment_name: None,
            attachment_content_type: None,
            attachment_size: None,
        };
        let v = history_view(&row);
        // Public projection includes the render fields…
        for key in [
            "id",
            "sender_kind",
            "display_name",
            "original",
            "lang",
            "translations",
            "created_at",
            "avatar_url",
            "attachment",
        ] {
            assert!(v.get(key).is_some(), "history view has `{key}`");
        }
        // …and never the trustworthy identity.
        let s = v.to_string();
        for leaked in ["sender_id", "guest_id", "user_id", "host_user_id", "email"] {
            assert!(!s.contains(leaked), "history view leaked `{leaked}`");
        }
    }

    #[test]
    fn display_name_defaults_and_clamps() {
        // Absent or empty → the default.
        assert_eq!(valid_display_name(None), DEFAULT_NAME);
        assert_eq!(valid_display_name(Some("   ")), DEFAULT_NAME);
        // A normal name is trimmed and kept.
        assert_eq!(valid_display_name(Some("  Alice  ")), "Alice");
        // Over the cap is clamped, not rejected.
        let long = "n".repeat(MAX_NAME_CHARS + 10);
        assert_eq!(
            valid_display_name(Some(&long)).chars().count(),
            MAX_NAME_CHARS
        );
    }
}
