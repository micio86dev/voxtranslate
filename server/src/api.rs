//! REST API handlers for billing + usage: package catalog, Stripe checkout,
//! Stripe webhook, credit history, and usage history.
//!
//! All money internals (cost, markup, rate, `stripe_price_id`) stay server-side
//! — only `balance`, package prices/credits, and deltas reach the client.

use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;

use chrono::{DateTime, Utc};

use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha1::Sha1;

use crate::ai::correction as ai_correction;
use crate::ai::correction::CorrectionMode;
use crate::ai::email_draft as ai_email;
use crate::ai::jobs as ai_jobs;
use crate::ai::quiz as ai_quiz;
use crate::ai::report as ai_report;
use crate::ai::sentiment as ai_sentiment;
use crate::billing::{usd, BillingError};
use crate::email::OutboundEmail;
use crate::glossary::{import_csv, normalize_entries, NewEntry, RoomGlossary};
use crate::middleware::AuthUser;
use crate::moderation::Severity;
use crate::protocol::ServerMessage;
use crate::stripe_handler;
use crate::transcripts::{BookmarkMutation, SessionAccess, TranscriptExport, TranscriptService};
use crate::AppState;

/// `GET /api/billing/packages` — the credit catalog (without `stripe_price_id`).
pub async fn billing_packages(State(state): State<AppState>) -> Response {
    match state.config.billing.as_ref() {
        Some(cfg) => Json(&cfg.pricing.packages).into_response(),
        None => service_unavailable(),
    }
}

/// `GET /api/engines` — the translation engines available in this deployment
/// (spec 0093), plus the UX rollout `flags` (spec 0102). The pre-join selector renders
/// from `engines` (always present — at least the default `standard` engine); each entry
/// carries the user-facing `rate_per_minute` (cost × markup), never the raw cost/markup.
/// `flags` is guest-safe here (unlike `/api/auth/config`, which 503s without billing) so
/// the language-first picker can be enabled for everyone.
pub async fn engines(State(state): State<AppState>) -> Response {
    Json(serde_json::json!({
        "engines": state.engines.infos(),
        "flags": { "language_first_ux": state.config.language_first_ux },
    }))
    .into_response()
}

// ---- Soniox "Enhanced" client-direct session (spec 0101) -------------------

#[derive(Deserialize, Default)]
pub struct SonioxSessionRequest {
    /// Also mint a TTS key for the optional "Spoken translation" leg. Default `false`
    /// → subtitles only, which stays within Soniox's small TTS concurrency cap.
    #[serde(default)]
    pub spoken: bool,
}

/// Build the Soniox temporary-key request body (spec 0101). `usage_type` is
/// `"transcribe_websocket"` (STT + translation) or `"tts_rt"` (spoken translation) —
/// note the STT value is Soniox's documented enum, not the `stt_rt` shorthand. Keys are
/// single-use (one WebSocket each) and short-lived; the browser mints a fresh one per
/// connection. Pure → unit-tested.
fn soniox_key_request(usage_type: &str, client_ref: &str) -> serde_json::Value {
    serde_json::json!({
        "usage_type": usage_type,
        "expires_in_seconds": 3600,           // max allowed (range 1–3600)
        "single_use": true,
        "max_session_duration_seconds": 7200, // ≤ 18000 allowed
        "client_reference_id": client_ref,    // our user id, for Soniox-side tracing
    })
}

/// Parse `{ api_key, expires_at }` from Soniox's 201 response. The `api_key` here is the
/// scoped, single-use TEMP key meant for the browser — never the raw `SONIOX_API_KEY`.
fn parse_soniox_key(body: &serde_json::Value) -> Result<(String, String), String> {
    let api_key = body
        .get("api_key")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("soniox response missing api_key")?
        .to_string();
    let expires_at = body
        .get("expires_at")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    Ok((api_key, expires_at))
}

/// Mint one Soniox temporary key (server→Soniox, Bearer = the region's raw key). Mirrors
/// the Cloudflare-TURN minting pattern: the raw key stays server-side, only the minted
/// temp key reaches the client.
async fn mint_soniox_key(
    http: &reqwest::Client,
    raw_key: &str,
    usage_type: &str,
    client_ref: &str,
) -> Result<(String, String), String> {
    let resp = http
        .post(crate::config::SONIOX_TEMP_KEY_URL)
        .bearer_auth(raw_key)
        .json(&soniox_key_request(usage_type, client_ref))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| format!("soniox temp-key request failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let detail = resp.text().await.unwrap_or_default();
        return Err(format!("soniox returned {status}: {detail}"));
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("soniox temp-key bad JSON: {e}"))?;
    parse_soniox_key(&body)
}

fn soniox_region_label(region: crate::config::SonioxRegion) -> &'static str {
    match region {
        crate::config::SonioxRegion::Us => "US",
        crate::config::SonioxRegion::Eu => "EU",
        crate::config::SonioxRegion::Jp => "JP",
    }
}

/// `POST /api/soniox/session` — mint scoped, single-use Soniox temp keys so the browser
/// can connect DIRECTLY to Soniox for the "Enhanced" tier (spec 0101). Auth-gated (so
/// guests, who are pinned to Standard, get 401), credit-gated, rate-limited, and routed
/// to a regional Soniox project via Cloudflare's `CF-IPCountry`. The raw `SONIOX_API_KEY`
/// never leaves the server — only the minted temp keys + public endpoints reach the
/// client. Returns `503` when the tier is not enabled (`SONIOX_ENHANCED` off).
pub async fn soniox_session(
    State(state): State<AppState>,
    user: AuthUser,
    headers: HeaderMap,
    Json(body): Json<SonioxSessionRequest>,
) -> Response {
    let (Some(soniox), Some(billing), Some(cfg)) = (
        state.config.soniox.as_ref(),
        state.billing.as_ref(),
        state.config.billing.as_ref(),
    ) else {
        return service_unavailable();
    };

    // Throttle (spec 0028): every Soniox WS (re)connect needs a fresh single-use key and
    // mesh churn is bursty, but cap scraping of our key-minting endpoint.
    if !state.rate_limiter.allow(
        &format!("soniox:{}", user.user_id),
        60,
        Duration::from_secs(60),
    ) {
        return (StatusCode::TOO_MANY_REQUESTS, "too many requests").into_response();
    }

    // Credit gate: same threshold as joining a call — don't mint a key the listener
    // can't afford to use. Advisory; the listener-pays meter is the real gate.
    match billing.can_join(user.user_id).await {
        Ok(true) => {}
        Ok(false) => {
            let available = billing
                .get_balance(user.user_id)
                .await
                .unwrap_or(Decimal::ZERO);
            return insufficient_credits(
                "soniox_enhanced",
                usd(cfg.pricing.min_balance_to_join),
                available,
            );
        }
        Err(e) => {
            tracing::error!("soniox can_join check failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "billing error").into_response();
        }
    }

    // Region routing (spec 0101): Cloudflare's edge sets `CF-IPCountry`. Only US is live;
    // EU/JP fall back to US until their regional projects + keys exist.
    let region = crate::config::soniox_region_for_country(
        headers.get("cf-ipcountry").and_then(|v| v.to_str().ok()),
    );
    let region_cfg = soniox.region(region);
    let client_ref = user.user_id.to_string();

    let stt = match mint_soniox_key(
        &state.http,
        &region_cfg.api_key,
        "transcribe_websocket",
        &client_ref,
    )
    .await
    {
        Ok((api_key, expires_at)) => serde_json::json!({
            "api_key": api_key,
            "expires_at": expires_at,
            "endpoint": region_cfg.stt_endpoint,
        }),
        Err(e) => {
            tracing::error!("soniox STT temp-key mint failed: {e}");
            return (StatusCode::BAD_GATEWAY, "soniox unavailable").into_response();
        }
    };

    let tts = if body.spoken {
        match mint_soniox_key(&state.http, &region_cfg.api_key, "tts_rt", &client_ref).await {
            Ok((api_key, expires_at)) => serde_json::json!({
                "api_key": api_key,
                "expires_at": expires_at,
                "endpoint": region_cfg.tts_endpoint,
            }),
            Err(e) => {
                tracing::error!("soniox TTS temp-key mint failed: {e}");
                return (StatusCode::BAD_GATEWAY, "soniox unavailable").into_response();
            }
        }
    } else {
        serde_json::Value::Null
    };

    Json(serde_json::json!({
        "stt": stt,
        "tts": tts,
        "region": soniox_region_label(region),
        "stt_model": soniox.stt_model,
    }))
    .into_response()
}

/// `GET /api/ice` — ICE servers for WebRTC peer connections (spec 0026). Always
/// returns public STUN; when a self-hosted coturn is configured (`TURN_*`) it also
/// returns time-limited TURN credentials via coturn's REST-API convention:
/// `username = "<unix-expiry>:vox"`, `credential = base64(HMAC-SHA1(secret, username))`.
/// The shared secret never leaves the server — only the derived credential does, and
/// it expires, so a leaked client config can't be abused for long.
pub async fn ice(State(state): State<AppState>, headers: HeaderMap) -> Response {
    // Per-IP throttle (spec 0028): /api/ice mints TURN credentials for anonymous
    // callers, so cap scraping (best-effort — coturn quotas bound the real damage).
    // Keyed by the trusted-proxy IP (issue #117 — last X-Forwarded-For hop).
    let ip = crate::observability::client_ip(&headers);
    if !state
        .rate_limiter
        .allow(&format!("ice:{ip}"), 30, Duration::from_secs(60))
    {
        return (StatusCode::TOO_MANY_REQUESTS, "too many requests").into_response();
    }
    let mut servers = vec![serde_json::json!({
        "urls": ["stun:stun.l.google.com:19302", "stun:stun1.l.google.com:19302"]
    })];
    if let Some(turn) = state.config.turn.as_ref() {
        let entry = match &turn.cred {
            // coturn REST: HMAC-sign a short expiry with the shared secret (spec 0026).
            crate::config::TurnCred::Secret { secret, ttl_secs } => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let username = format!("{}:vox", now + ttl_secs);
                let mut mac = Hmac::<Sha1>::new_from_slice(secret.as_bytes())
                    .expect("HMAC accepts a key of any length");
                mac.update(username.as_bytes());
                let credential =
                    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
                Some(serde_json::json!({
                    "urls": turn.urls,
                    "username": username,
                    "credential": credential,
                }))
            }
            // Managed relay: pass the static username/password straight through (spec 0059).
            crate::config::TurnCred::Static { username, password } => Some(serde_json::json!({
                "urls": turn.urls,
                "username": username,
                "credential": password,
            })),
            // Cloudflare Realtime TURN: mint a short-lived credential per request via
            // Cloudflare's API (spec 0077). Best-effort — on any failure we fall back
            // to STUN-only so a call still connects when direct P2P works.
            crate::config::TurnCred::Cloudflare {
                key_id,
                api_token,
                ttl_secs,
            } => cloudflare_ice_servers(&state.http, key_id, api_token, *ttl_secs).await,
        };
        if let Some(entry) = entry {
            servers.push(entry);
        }
    }
    Json(serde_json::json!({ "iceServers": servers })).into_response()
}

/// Cloudflare Realtime TURN credential-generation endpoint for a TURN key id (spec 0077).
fn cf_turn_credentials_url(key_id: &str) -> String {
    format!("https://rtc.live.cloudflare.com/v1/turn/keys/{key_id}/credentials/generate")
}

/// Pull the `iceServers` object out of Cloudflare's credential-generation response.
/// Returns `None` on an unexpected shape so `/api/ice` degrades to STUN-only.
fn parse_cf_ice_servers(body: &serde_json::Value) -> Option<serde_json::Value> {
    let ice = body.get("iceServers")?;
    // Only useful with both a server list and a credential to present.
    if ice.get("urls").is_some() && ice.get("credential").is_some() {
        Some(ice.clone())
    } else {
        None
    }
}

/// Ask Cloudflare to mint short-lived TURN credentials (anycast URLs + a time-limited
/// username/credential). Best-effort: any network / non-2xx / parse error logs and
/// yields `None`, so the caller returns STUN-only rather than failing the call. The
/// `api_token` stays server-side — only the minted username/credential reach the client.
async fn cloudflare_ice_servers(
    http: &reqwest::Client,
    key_id: &str,
    api_token: &str,
    ttl_secs: u64,
) -> Option<serde_json::Value> {
    let resp = http
        .post(cf_turn_credentials_url(key_id))
        .bearer_auth(api_token)
        .json(&serde_json::json!({ "ttl": ttl_secs }))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| tracing::warn!("cloudflare TURN request failed: {e}"))
        .ok()?;
    if !resp.status().is_success() {
        tracing::warn!("cloudflare TURN returned HTTP {}", resp.status());
        return None;
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| tracing::warn!("cloudflare TURN bad JSON: {e}"))
        .ok()?;
    parse_cf_ice_servers(&body)
}

/// Max characters in a user bug report (spec 0071).
const BUG_REPORT_MAX_LEN: usize = 2000;

#[derive(serde::Deserialize)]
pub struct BugReportRequest {
    pub message: String,
    #[serde(default)]
    pub page_url: Option<String>,
}

/// `POST /api/bug-report` — a user (guest or signed-in) reports a problem (spec 0071).
/// Stores the report (status `received`) and best-effort emails the admins
/// (`BUG_REPORT_TO`). No auth required so guests can report; a token, if present,
/// attributes it. Rate-limited per IP; the message is length-capped.
pub async fn bug_report(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<BugReportRequest>,
) -> Response {
    // Per-IP throttle (spec 0028/0064): cap report spam — keyed by the trusted hop.
    let ip = crate::observability::client_ip(&headers);
    if !state
        .rate_limiter
        .allow(&format!("bug:{ip}"), 5, Duration::from_secs(60))
    {
        return (StatusCode::TOO_MANY_REQUESTS, "too many requests").into_response();
    }

    let message = body.message.trim();
    if message.is_empty() {
        return (StatusCode::BAD_REQUEST, "message required").into_response();
    }
    if message.chars().count() > BUG_REPORT_MAX_LEN {
        return (StatusCode::BAD_REQUEST, "message too long").into_response();
    }

    // Persistence backs the backoffice triage flow, so a database is required.
    let Some(pool) = state.pool.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "reporting unavailable").into_response();
    };

    // Optional attribution: resolve a signed-in user from the bearer token if present
    // (guests report anonymously). No banned-check here — reporting a bug is harmless.
    let claims = state.config.billing.as_ref().and_then(|b| {
        headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .and_then(|tok| crate::auth::verify_jwt(&b.jwt_secret, tok).ok())
    });
    let user_id = claims
        .as_ref()
        .and_then(|c| uuid::Uuid::parse_str(&c.sub).ok());
    let email = user_id.and(claims.as_ref().map(|c| c.email.clone()));
    let page_url = body
        .page_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.chars().take(500).collect::<String>());

    let id = match crate::db::insert_bug_report(
        pool,
        message,
        user_id,
        email.as_deref(),
        page_url,
        user_agent.as_deref(),
    )
    .await
    {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("bug_report insert failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "could not save report").into_response();
        }
    };

    // Best-effort admin email — the report is already stored, so a send failure must
    // never fail the request. Only when Resend is configured.
    if let Some(resend) = state.resend.as_ref() {
        let who = match (user_id, email) {
            (Some(uid), Some(em)) => format!("{em} ({uid})"),
            _ => "guest".to_string(),
        };
        let page = page_url.unwrap_or("—");
        let body_text = format!("New bug report ({id})\n\nFrom: {who}\nPage: {page}\n\n{message}");
        let inner_html = format!(
            "<p style=\"margin:0 0 8px;color:#64748b;\">Report <code>{id}</code></p>\
             <p style=\"margin:0 0 12px;\">From: {}<br>Page: {}</p>\
             <pre style=\"white-space:pre-wrap;font-family:inherit;background:#f4f6fb;\
             padding:14px 16px;border-radius:8px;margin:0;\">{}</pre>",
            crate::admin::html_escape(&who),
            crate::admin::html_escape(page),
            crate::admin::html_escape(message),
        );
        let tagline = crate::email_template::tagline("en");
        let html = crate::email_template::render_html(&crate::email_template::EmailLayout {
            app_base_url: &state.config.app_base_url,
            preheader: "New bug report",
            heading: Some("New bug report"),
            body_html: &inner_html,
            button: None,
            tagline,
        });
        let text = crate::email_template::render_text(
            &body_text,
            None,
            &state.config.app_base_url,
            tagline,
        );
        let email_msg = OutboundEmail {
            to: vec![state.config.bug_report_to.clone()],
            cc: vec![],
            subject: "VoxTranslate — new bug report".to_string(),
            html,
            text,
        };
        if let Err(e) = resend.send(&email_msg).await {
            tracing::error!("bug_report email failed: {e}");
        }
    }

    Json(serde_json::json!({ "ok": true })).into_response()
}

#[derive(Deserialize)]
pub struct CheckoutRequest {
    pub package_id: String,
}

/// `POST /api/billing/checkout` — start a Stripe Checkout Session for a package.
/// Rate-limited per user. Returns `{ "url": "https://checkout.stripe.com/..." }`.
pub async fn billing_checkout(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<CheckoutRequest>,
) -> Response {
    let Some(cfg) = state.config.billing.as_ref() else {
        return service_unavailable();
    };

    // Throttle checkout creation per user (10 / minute).
    if !state.rate_limiter.allow(
        &format!("checkout:{}", user.user_id),
        10,
        Duration::from_secs(60),
    ) {
        return (StatusCode::TOO_MANY_REQUESTS, "too many requests").into_response();
    }

    let Some(pkg) = cfg
        .pricing
        .packages
        .iter()
        .find(|p| p.id == body.package_id)
    else {
        return (StatusCode::BAD_REQUEST, "unknown package").into_response();
    };
    if cfg.stripe_secret_key.trim().is_empty() {
        return (StatusCode::SERVICE_UNAVAILABLE, "payments not configured").into_response();
    }

    match stripe_handler::create_checkout_session(&state.http, cfg, pkg, &user.user_id).await {
        Ok(url) => Json(serde_json::json!({ "url": url })).into_response(),
        Err(e) => {
            tracing::error!("stripe checkout failed: {e}");
            (StatusCode::BAD_GATEWAY, "checkout failed").into_response()
        }
    }
}

/// `POST /api/billing/webhook` — verify the Stripe signature, then on
/// `checkout.session.completed` credit the user idempotently.
pub async fn billing_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (Some(cfg), Some(billing)) = (state.config.billing.as_ref(), state.billing.as_ref()) else {
        return service_unavailable();
    };

    let sig = headers
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !stripe_handler::verify_stripe_signature(&cfg.stripe_webhook_secret, &body, sig) {
        return (StatusCode::BAD_REQUEST, "invalid signature").into_response();
    }

    let event: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid payload").into_response(),
    };
    let event_id = event["id"].as_str().unwrap_or_default();
    let event_type = event["type"].as_str().unwrap_or_default();
    if event_id.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing event id").into_response();
    }

    if event_type == "checkout.session.completed" {
        let meta = &event["data"]["object"]["metadata"];
        let user_id = meta["user_id"]
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok());
        // Stripe metadata values are strings; accept a number too, defensively.
        let credits = meta["credits_usd"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .or_else(|| meta["credits_usd"].as_f64());
        let package = meta["package_id"].as_str().unwrap_or("credits");

        match (user_id, credits) {
            (Some(uid), Some(cr)) if cr > 0.0 => {
                match billing
                    .credit_from_stripe_event(
                        event_id,
                        event_type,
                        uid,
                        usd(cr),
                        &format!("Purchase: {package}"),
                    )
                    .await
                {
                    Ok(true) => tracing::info!(%uid, credits = cr, %event_id, "credited purchase"),
                    Ok(false) => tracing::info!(%event_id, "duplicate webhook ignored"),
                    Err(e) => {
                        tracing::error!("crediting failed: {e}");
                        return (StatusCode::INTERNAL_SERVER_ERROR, "credit failed")
                            .into_response();
                    }
                }
            }
            _ => tracing::warn!(%event_id, "checkout.session.completed missing metadata"),
        }
    }

    (StatusCode::OK, "ok").into_response()
}

/// `GET /api/billing/history` — the authenticated user's recent ledger entries.
pub async fn billing_history(State(state): State<AppState>, user: AuthUser) -> Response {
    let Some(billing) = state.billing.as_ref() else {
        return service_unavailable();
    };
    match billing.get_history(user.user_id, 50).await {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => {
            tracing::error!("history query failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

/// `GET /api/usage/sessions` — the authenticated user's recent usage sessions.
pub async fn usage_sessions(State(state): State<AppState>, user: AuthUser) -> Response {
    let Some(billing) = state.billing.as_ref() else {
        return service_unavailable();
    };
    match billing.get_sessions(user.user_id, 50).await {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => {
            tracing::error!("usage query failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

fn service_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "auth/billing not configured",
    )
        .into_response()
}

/// The standard `insufficient_credits` body. Shared by the synchronous 402
/// pre-check responder and the async job path (where a balance that dropped
/// between pre-check and `deduct_feature` fails the job with this as its
/// payload, so the poller can show the same "need X, have Y" prompt).
fn insufficient_body(feature: &str, required: Decimal, available: Decimal) -> serde_json::Value {
    serde_json::json!({
        "error": "insufficient_credits",
        "required": required.to_f64().unwrap_or(0.0),
        "available": available.to_f64().unwrap_or(0.0),
        "feature": feature,
    })
}

/// Shared 402 responder for credit-charged AI features. The pre-check is
/// advisory (the atomic `deduct_feature` is the real gate) but lets the client
/// show "need X, have Y" before any AI work runs.
pub fn insufficient_credits(feature: &str, required: Decimal, available: Decimal) -> Response {
    (
        StatusCode::PAYMENT_REQUIRED,
        Json(insufficient_body(feature, required, available)),
    )
        .into_response()
}

/// `202 Accepted` for a claimed async AI job: the client polls
/// `GET /api/sessions/{id}/ai-job/{job_id}` until the job is done or failed.
fn ai_job_accepted(job_id: Uuid) -> Response {
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "job_id": job_id, "status": "pending" })),
    )
        .into_response()
}

/// `GET /api/sessions/{id}/ai-job/{job_id}` — poll a background AI job (report,
/// correction, email draft). Any participant of the session may read it; the
/// lookup is session-scoped so a job id can't be probed across sessions.
/// Returns `{ status, error, result }`: `result` carries the body the
/// synchronous endpoint used to return (present on `done`; on an
/// `insufficient_credits` failure it carries the 402 body).
pub async fn ai_job_status(
    State(state): State<AppState>,
    user: AuthUser,
    Path((session_id, job_id)): Path<(Uuid, Uuid)>,
) -> Response {
    let (Some(svc), Some(pool)) = (state.transcripts.as_ref(), state.pool.as_ref()) else {
        return service_unavailable();
    };
    if let Err(resp) = session_gate(svc, session_id, user.user_id).await {
        return resp;
    }
    match ai_jobs::get(pool, job_id, session_id).await {
        Ok(Some(job)) => Json(serde_json::json!({
            "status": job.status,
            "error": job.error,
            "result": job.result_json,
        }))
        .into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no such job").into_response(),
        Err(e) => {
            tracing::error!("ai job load failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

/// `GET /api/billing/ai-pricing` — per-feature user rates for client cost
/// previews. These are the env-configured user-facing prices; raw cost/markup
/// internals are never exposed.
pub async fn ai_pricing(State(state): State<AppState>) -> Response {
    let Some(cfg) = state.config.billing.as_ref() else {
        return service_unavailable();
    };
    let ai = &cfg.ai;
    Json(serde_json::json!({
        "report": { "base": ai.report_base, "per_minute": ai.report_per_minute },
        "sentiment": {
            "base": ai.sentiment_base,
            "per_participant": ai.sentiment_per_participant,
            "per_minute": ai.sentiment_per_minute,
        },
        "email": { "draft": ai.email_draft },
        "suggestions": {
            "per_minute": ai.suggestions_per_minute,
            "interval_seconds": ai.suggestions_interval_secs,
        },
        "quiz": { "base": ai.quiz_base, "per_question": ai.quiz_per_question },
        "transcript_correction": {
            "base": ai.correction_base,
            "per_event": ai.correction_per_event,
        },
        "email_enabled": state.config.resend.is_some(),
    }))
    .into_response()
}

/// `GET /api/sessions` — call sessions the user took part in, newest first.
pub async fn sessions_list(State(state): State<AppState>, user: AuthUser) -> Response {
    let Some(svc) = state.transcripts.as_ref() else {
        return service_unavailable();
    };
    // Barrier so just-finished calls show their final event counts.
    svc.flush().await;
    match svc.list_sessions(user.user_id, 50).await {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => {
            tracing::error!("sessions query failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

/// `?corrected=1` flag on the download endpoints (spec 0068): render from the
/// cached AI-corrected text instead of the raw transcript.
fn wants_corrected(v: &Option<String>) -> bool {
    matches!(v.as_deref(), Some("1" | "true" | "yes"))
}

/// Overlay the cached AI correction for `(session, mode, lang)` onto `export`
/// before it is rendered. `lang` is normalized by the caller ("" for
/// `Original`). Returns `Err(response)` when billing is off (503) or no
/// correction has been generated yet (409 — the client POSTs first).
async fn overlay_correction(
    state: &AppState,
    session_id: Uuid,
    export: &mut crate::transcripts::TranscriptExport,
    mode: CorrectionMode,
    lang: &str,
) -> Result<(), Response> {
    let Some(pool) = state.pool.as_ref() else {
        return Err(service_unavailable());
    };
    match ai_correction::get_correction(pool, session_id, mode, lang).await {
        Ok(Some(row)) => {
            ai_correction::apply_correction(export, &ai_correction::row_lines(&row), lang);
            Ok(())
        }
        Ok(None) => Err((
            StatusCode::CONFLICT,
            "no correction generated for this export — request correction first",
        )
            .into_response()),
        Err(e) => {
            tracing::error!("correction load failed: {e}");
            Err((StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response())
        }
    }
}

#[derive(Deserialize, Default)]
pub struct TranscriptJsonQuery {
    /// `1`/`true` → render the cached AI-corrected text (spec 0068).
    pub corrected: Option<String>,
}

/// `GET /api/sessions/{id}/transcript.json` — download the transcript as
/// pretty-printed JSON. Participants only (404 unknown / 403 stranger).
pub async fn transcript_json(
    State(state): State<AppState>,
    user: AuthUser,
    Path(session_id): Path<Uuid>,
    Query(q): Query<TranscriptJsonQuery>,
) -> Response {
    let Some(svc) = state.transcripts.as_ref() else {
        return service_unavailable();
    };
    // Barrier kills the leave-then-download race and enables live mid-call export.
    svc.flush().await;

    match svc.access(session_id, user.user_id).await {
        Ok(SessionAccess::Ok) => {}
        Ok(SessionAccess::NotFound) => {
            return (StatusCode::NOT_FOUND, "no such session").into_response()
        }
        Ok(SessionAccess::Forbidden) => {
            return (StatusCode::FORBIDDEN, "not a participant").into_response()
        }
        Err(e) => {
            tracing::error!("transcript access check failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    }

    let mut export = match svc.export(session_id).await {
        Ok(Some(doc)) => doc,
        // Purged between the access check and here (guest-only finalize race).
        Ok(None) => return (StatusCode::NOT_FOUND, "no such session").into_response(),
        Err(e) => {
            tracing::error!("transcript export failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };

    // JSON carries every language, so a corrected JSON export polishes the
    // authoritative source text only (mode=original, lang-agnostic).
    if wants_corrected(&q.corrected) {
        if let Err(resp) = overlay_correction(
            &state,
            session_id,
            &mut export,
            CorrectionMode::Original,
            "",
        )
        .await
        {
            return resp;
        }
    }

    let body = match serde_json::to_string_pretty(&export) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("transcript serialization failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "export failed").into_response();
        }
    };
    let filename = transcript_filename(&export.session.room_name, session_id, "json");
    (
        [
            (header::CONTENT_TYPE, "application/json".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        body,
    )
        .into_response()
}

#[derive(Deserialize, Default)]
pub struct TranscriptPdfQuery {
    /// IANA timezone for displayed times (e.g. `Europe/Rome`); bogus → UTC.
    pub tz: Option<String>,
    /// Translation language to show per event; default = the requester's own
    /// participant language for that session, fallback `en`.
    pub lang: Option<String>,
    /// `1`/`true` → render the cached AI-corrected text (spec 0068).
    pub corrected: Option<String>,
}

/// `GET /api/sessions/{id}/transcript.pdf?tz=Europe/Rome&lang=it` — download
/// the transcript as a typst-rendered PDF. Same auth gates as the JSON export,
/// plus a per-user rate limit (PDF compilation is CPU-bound).
pub async fn transcript_pdf(
    State(state): State<AppState>,
    user: AuthUser,
    Path(session_id): Path<Uuid>,
    Query(q): Query<TranscriptPdfQuery>,
) -> Response {
    let Some(svc) = state.transcripts.as_ref() else {
        return service_unavailable();
    };

    // Throttle before any work — rendering costs real CPU (5 / minute).
    if !state
        .rate_limiter
        .allow(&format!("pdf:{}", user.user_id), 5, Duration::from_secs(60))
    {
        return (StatusCode::TOO_MANY_REQUESTS, "too many requests").into_response();
    }

    // Barrier kills the leave-then-download race and enables live mid-call export.
    svc.flush().await;

    match svc.access(session_id, user.user_id).await {
        Ok(SessionAccess::Ok) => {}
        Ok(SessionAccess::NotFound) => {
            return (StatusCode::NOT_FOUND, "no such session").into_response()
        }
        Ok(SessionAccess::Forbidden) => {
            return (StatusCode::FORBIDDEN, "not a participant").into_response()
        }
        Err(e) => {
            tracing::error!("transcript access check failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    }

    let mut export = match svc.export(session_id).await {
        Ok(Some(doc)) => doc,
        // Purged between the access check and here (guest-only finalize race).
        Ok(None) => return (StatusCode::NOT_FOUND, "no such session").into_response(),
        Err(e) => {
            tracing::error!("transcript export failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };

    let tz =
        q.tz.as_deref()
            .and_then(|s| s.parse::<chrono_tz::Tz>().ok())
            .unwrap_or(chrono_tz::UTC);
    let lang = match q.lang {
        Some(l) => l,
        None => svc
            .participant_lang(session_id, user.user_id)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| "en".to_string()),
    };

    // PDF shows original + the chosen translation, so a corrected PDF polishes
    // both (mode=both, lang = the shown translation).
    if wants_corrected(&q.corrected) {
        if let Err(resp) =
            overlay_correction(&state, session_id, &mut export, CorrectionMode::Both, &lang).await
        {
            return resp;
        }
    }

    let doc_json = match serde_json::to_string(&crate::pdf::build_pdf_doc(&export, tz, &lang)) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!("pdf doc serialization failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "export failed").into_response();
        }
    };
    // typst compilation is CPU-bound — keep it off the async runtime.
    let rendered =
        tokio::task::spawn_blocking(move || crate::pdf::render_transcript_pdf(&doc_json)).await;
    let pdf = match rendered {
        Ok(Ok(pdf)) => pdf,
        Ok(Err(e)) => {
            tracing::error!("transcript pdf render failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "pdf render failed").into_response();
        }
        Err(e) => {
            tracing::error!("transcript pdf task panicked: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "pdf render failed").into_response();
        }
    };

    let filename = transcript_filename(&export.session.room_name, session_id, "pdf");
    (
        [
            (header::CONTENT_TYPE, "application/pdf".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        pdf.bytes,
    )
        .into_response()
}

#[derive(Deserialize, Default)]
pub struct SubtitleQuery {
    /// `original` | `translated` (default) | `both`.
    pub lang: Option<String>,
    /// Translation language for `translated`/`both`; default = the requester's
    /// own participant language for that session, fallback `en`.
    pub target: Option<String>,
    /// `1`/`true` → render the cached AI-corrected text (spec 0068).
    pub corrected: Option<String>,
}

/// `GET /api/sessions/{id}/transcript.srt?lang=both&target=it` — SubRip
/// subtitles. Same auth gates as the JSON export.
pub async fn transcript_srt(
    State(state): State<AppState>,
    user: AuthUser,
    Path(session_id): Path<Uuid>,
    Query(q): Query<SubtitleQuery>,
) -> Response {
    subtitles_response(state, user, session_id, q, SubtitleFormat::Srt).await
}

/// `GET /api/sessions/{id}/transcript.vtt?lang=both&target=it` — WebVTT
/// subtitles with `<v Speaker>` voice tags. Same auth gates as the JSON export.
pub async fn transcript_vtt(
    State(state): State<AppState>,
    user: AuthUser,
    Path(session_id): Path<Uuid>,
    Query(q): Query<SubtitleQuery>,
) -> Response {
    subtitles_response(state, user, session_id, q, SubtitleFormat::Vtt).await
}

enum SubtitleFormat {
    Srt,
    Vtt,
}

async fn subtitles_response(
    state: AppState,
    user: AuthUser,
    session_id: Uuid,
    q: SubtitleQuery,
    format: SubtitleFormat,
) -> Response {
    let Some(svc) = state.transcripts.as_ref() else {
        return service_unavailable();
    };
    let mode = match q.lang.as_deref() {
        None => crate::subtitles::LangMode::Translated,
        Some(s) => match crate::subtitles::LangMode::parse(s) {
            Some(m) => m,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    "lang must be original, translated or both",
                )
                    .into_response()
            }
        },
    };

    // Barrier kills the leave-then-download race and enables live mid-call export.
    svc.flush().await;

    match svc.access(session_id, user.user_id).await {
        Ok(SessionAccess::Ok) => {}
        Ok(SessionAccess::NotFound) => {
            return (StatusCode::NOT_FOUND, "no such session").into_response()
        }
        Ok(SessionAccess::Forbidden) => {
            return (StatusCode::FORBIDDEN, "not a participant").into_response()
        }
        Err(e) => {
            tracing::error!("transcript access check failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    }

    let mut export = match svc.export(session_id).await {
        Ok(Some(doc)) => doc,
        // Purged between the access check and here (guest-only finalize race).
        Ok(None) => return (StatusCode::NOT_FOUND, "no such session").into_response(),
        Err(e) => {
            tracing::error!("transcript export failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };

    let target = match q.target {
        Some(t) => t,
        None => svc
            .participant_lang(session_id, user.user_id)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| "en".to_string()),
    };

    // Correct the same fields the subtitle mode shows. Original mode is
    // lang-agnostic (cache key lang=""); translated/both key on the target.
    if wants_corrected(&q.corrected) {
        let (cmode, clang) = match mode {
            crate::subtitles::LangMode::Original => (CorrectionMode::Original, ""),
            crate::subtitles::LangMode::Translated => (CorrectionMode::Translated, target.as_str()),
            crate::subtitles::LangMode::Both => (CorrectionMode::Both, target.as_str()),
        };
        if let Err(resp) = overlay_correction(&state, session_id, &mut export, cmode, clang).await {
            return resp;
        }
    }

    let cues =
        crate::subtitles::compute_cues(&export.events, export.session.started_at, mode, &target);
    let (body, content_type, ext) = match format {
        SubtitleFormat::Srt => (
            crate::subtitles::build_srt(&cues),
            "application/x-subrip",
            "srt",
        ),
        SubtitleFormat::Vtt => (crate::subtitles::build_vtt(&cues), "text/vtt", "vtt"),
    };
    let filename = transcript_filename(&export.session.room_name, session_id, ext);
    (
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        body,
    )
        .into_response()
}

#[derive(Deserialize, Default)]
pub struct BookmarkCreate {
    /// Moment to pin; default = server "now" (the in-call 🔖 button posts
    /// instantly, avoiding client clock skew, and labels afterwards).
    pub ts: Option<DateTime<Utc>>,
    pub label: Option<String>,
}

#[derive(Deserialize)]
pub struct BookmarkPatch {
    /// `null` / empty clears the label.
    pub label: Option<String>,
}

/// Trim a bookmark label: empty → `None`, > 200 chars → 400.
#[allow(clippy::result_large_err)] // the Err IS the handler's HTTP response
fn clean_label(label: Option<String>) -> Result<Option<String>, Response> {
    let Some(l) = label else { return Ok(None) };
    let l = l.trim();
    if l.chars().count() > 200 {
        return Err((StatusCode::BAD_REQUEST, "label too long").into_response());
    }
    Ok((!l.is_empty()).then(|| l.to_string()))
}

/// Shared 404/403 access gate for session-scoped endpoints.
async fn session_gate(
    svc: &TranscriptService,
    session_id: Uuid,
    user_id: Uuid,
) -> Result<(), Response> {
    match svc.access(session_id, user_id).await {
        Ok(SessionAccess::Ok) => Ok(()),
        Ok(SessionAccess::NotFound) => {
            Err((StatusCode::NOT_FOUND, "no such session").into_response())
        }
        Ok(SessionAccess::Forbidden) => {
            Err((StatusCode::FORBIDDEN, "not a participant").into_response())
        }
        Err(e) => {
            tracing::error!("transcript access check failed: {e}");
            Err((StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response())
        }
    }
}

/// `GET /api/sessions/{id}/bookmarks` — every participant's pins, chronological.
/// Participants only (404 unknown / 403 stranger). No flush needed: session and
/// participant rows insert synchronously, only events go through the channel.
pub async fn bookmarks_list(
    State(state): State<AppState>,
    user: AuthUser,
    Path(session_id): Path<Uuid>,
) -> Response {
    let Some(svc) = state.transcripts.as_ref() else {
        return service_unavailable();
    };
    if let Err(resp) = session_gate(svc, session_id, user.user_id).await {
        return resp;
    }
    match svc.list_bookmarks(session_id, user.user_id).await {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => {
            tracing::error!("bookmark list failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

/// `POST /api/sessions/{id}/bookmarks` — pin a moment (201 + the bookmark).
pub async fn bookmark_add(
    State(state): State<AppState>,
    user: AuthUser,
    Path(session_id): Path<Uuid>,
    Json(body): Json<BookmarkCreate>,
) -> Response {
    let Some(svc) = state.transcripts.as_ref() else {
        return service_unavailable();
    };
    let label = match clean_label(body.label) {
        Ok(l) => l,
        Err(resp) => return resp,
    };
    if let Err(resp) = session_gate(svc, session_id, user.user_id).await {
        return resp;
    }
    match svc
        .add_bookmark(session_id, user.user_id, body.ts, label.as_deref())
        .await
    {
        Ok(b) => (StatusCode::CREATED, Json(b)).into_response(),
        Err(e) => {
            tracing::error!("bookmark insert failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

/// `PATCH /api/sessions/{id}/bookmarks/{bid}` — relabel; owner only.
pub async fn bookmark_update(
    State(state): State<AppState>,
    user: AuthUser,
    Path((session_id, bookmark_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<BookmarkPatch>,
) -> Response {
    let Some(svc) = state.transcripts.as_ref() else {
        return service_unavailable();
    };
    let label = match clean_label(body.label) {
        Ok(l) => l,
        Err(resp) => return resp,
    };
    let outcome = svc
        .update_bookmark_label(session_id, bookmark_id, user.user_id, label.as_deref())
        .await;
    bookmark_mutation_response(outcome, "bookmark update")
}

/// `DELETE /api/sessions/{id}/bookmarks/{bid}` — owner only.
pub async fn bookmark_delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path((session_id, bookmark_id)): Path<(Uuid, Uuid)>,
) -> Response {
    let Some(svc) = state.transcripts.as_ref() else {
        return service_unavailable();
    };
    let outcome = svc
        .delete_bookmark(session_id, bookmark_id, user.user_id)
        .await;
    bookmark_mutation_response(outcome, "bookmark delete")
}

fn bookmark_mutation_response(
    outcome: Result<BookmarkMutation, sqlx::Error>,
    what: &str,
) -> Response {
    match outcome {
        Ok(BookmarkMutation::Ok) => StatusCode::NO_CONTENT.into_response(),
        Ok(BookmarkMutation::Forbidden) => {
            (StatusCode::FORBIDDEN, "not your bookmark").into_response()
        }
        Ok(BookmarkMutation::NotFound) => {
            (StatusCode::NOT_FOUND, "no such bookmark").into_response()
        }
        Err(e) => {
            tracing::error!("{what} failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

// ---- Room glossary (spec 0011) ---------------------------------------------

#[derive(Deserialize)]
pub struct GlossaryPayload {
    pub name: Option<String>,
    #[serde(default)]
    pub entries: Vec<NewEntry>,
}

#[derive(Deserialize)]
pub struct GlossaryImport {
    pub csv: String,
}

/// Validate the room code from the path (rooms are short user-chosen codes).
#[allow(clippy::result_large_err)] // the Err IS the handler's HTTP response
fn clean_room(room: &str) -> Result<String, Response> {
    let r = room.trim();
    if r.is_empty() || r.len() > 64 {
        return Err((StatusCode::BAD_REQUEST, "invalid room").into_response());
    }
    Ok(r.to_string())
}

/// Trim a glossary name: empty → `None`, > 100 chars → 400.
#[allow(clippy::result_large_err)] // the Err IS the handler's HTTP response
fn clean_glossary_name(name: Option<String>) -> Result<Option<String>, Response> {
    let Some(n) = name else { return Ok(None) };
    let n = n.trim();
    if n.chars().count() > 100 {
        return Err((StatusCode::BAD_REQUEST, "glossary name too long").into_response());
    }
    Ok((!n.is_empty()).then(|| n.to_string()))
}

/// `{ name, entries, max_entries }` — the shape the editor modal consumes.
fn glossary_response(g: &RoomGlossary, max_entries: usize) -> Response {
    Json(serde_json::json!({
        "name": g.name,
        "entries": g.entries,
        "max_entries": max_entries,
    }))
    .into_response()
}

/// Tell everyone currently in the room about the new glossary state, so the
/// in-call badge updates live and the next utterance uses the fresh terms.
fn broadcast_glossary(state: &AppState, room: &str, g: &RoomGlossary) {
    state.rooms.broadcast(
        room,
        &ServerMessage::GlossaryActive {
            name: g.name.clone(),
            entries: g.entries.len(),
        }
        .to_json(),
    );
}

/// Per-user request budget for the glossary editor endpoints, to cap room-code
/// enumeration/scraping (issue #117). The editor is auth-only, so all four endpoints
/// key the limit on the caller's user id.
const GLOSSARY_MAX_PER_MIN: u32 = 30;

/// Shared per-user throttle for the glossary editor endpoints. Returns `Some(429)` when
/// the caller is over budget.
fn glossary_throttle(state: &AppState, user_id: Uuid) -> Option<Response> {
    if !state.rate_limiter.allow(
        &format!("glossary:{user_id}"),
        GLOSSARY_MAX_PER_MIN,
        Duration::from_secs(60),
    ) {
        return Some((StatusCode::TOO_MANY_REQUESTS, "slow down").into_response());
    }
    None
}

/// `GET /api/rooms/{room}/glossary` — the room's glossary (empty when none).
/// **Auth required** (issue #117): the editor is auth-only, so reads must be too —
/// this removes the anonymous read of any room's glossary by code. The room code is
/// still the in-room capability (you can prepare a glossary from home before joining),
/// so this doesn't add a membership check; it just closes anonymous access + scraping.
pub async fn glossary_get(
    State(state): State<AppState>,
    user: AuthUser,
    Path(room): Path<String>,
) -> Response {
    if let Some(resp) = glossary_throttle(&state, user.user_id) {
        return resp;
    }
    let Some(svc) = state.glossary.as_ref() else {
        return service_unavailable();
    };
    let room = match clean_room(&room) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    match svc.get(&room).await {
        Ok(g) => glossary_response(&g, svc.max_entries()),
        Err(e) => {
            tracing::error!("glossary load failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

/// `POST /api/rooms/{room}/glossary` — replace the glossary (name + entries).
/// Any signed-in user may edit: whoever has the room code is in the meeting.
pub async fn glossary_save(
    State(state): State<AppState>,
    user: AuthUser,
    Path(room): Path<String>,
    Json(body): Json<GlossaryPayload>,
) -> Response {
    if let Some(resp) = glossary_throttle(&state, user.user_id) {
        return resp;
    }
    let Some(svc) = state.glossary.as_ref() else {
        return service_unavailable();
    };
    let room = match clean_room(&room) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let name = match clean_glossary_name(body.name) {
        Ok(n) => n,
        Err(resp) => return resp,
    };
    let entries = match normalize_entries(body.entries, svc.max_entries()) {
        Ok(e) => e,
        Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
    };
    match svc
        .save(&room, name.as_deref(), &entries, user.user_id)
        .await
    {
        Ok(g) => {
            broadcast_glossary(&state, &room, &g);
            glossary_response(&g, svc.max_entries())
        }
        Err(e) => {
            tracing::error!("glossary save failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

/// `DELETE /api/rooms/{room}/glossary` — drop it entirely (idempotent, 204).
pub async fn glossary_delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path(room): Path<String>,
) -> Response {
    if let Some(resp) = glossary_throttle(&state, user.user_id) {
        return resp;
    }
    let Some(svc) = state.glossary.as_ref() else {
        return service_unavailable();
    };
    let room = match clean_room(&room) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    match svc.delete(&room).await {
        Ok(()) => {
            broadcast_glossary(&state, &room, &RoomGlossary::default());
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            tracing::error!("glossary delete failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

/// `POST /api/rooms/{room}/glossary/import` — parse CSV server-side and merge
/// into the saved glossary (imported rows override same-key entries).
pub async fn glossary_import(
    State(state): State<AppState>,
    user: AuthUser,
    Path(room): Path<String>,
    Json(body): Json<GlossaryImport>,
) -> Response {
    if let Some(resp) = glossary_throttle(&state, user.user_id) {
        return resp;
    }
    let Some(svc) = state.glossary.as_ref() else {
        return service_unavailable();
    };
    let room = match clean_room(&room) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let imported = match import_csv(&body.csv) {
        Ok(rows) if rows.is_empty() => {
            return (StatusCode::BAD_REQUEST, "no entries in CSV").into_response()
        }
        Ok(rows) => rows,
        Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
    };
    let existing = match svc.get(&room).await {
        Ok(g) => g,
        Err(e) => {
            tracing::error!("glossary load failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };
    // Existing first, imported last → last-wins dedupe lets the import override.
    let mut merged: Vec<NewEntry> = existing
        .entries
        .iter()
        .map(|e| NewEntry {
            source_lang: e.source_lang.clone(),
            target_lang: e.target_lang.clone(),
            source_term: e.source_term.clone(),
            target_term: e.target_term.clone(),
        })
        .collect();
    merged.extend(imported);
    let entries = match normalize_entries(merged, svc.max_entries()) {
        Ok(e) => e,
        Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
    };
    let name = existing.name.clone();
    match svc
        .save(&room, name.as_deref(), &entries, user.user_id)
        .await
    {
        Ok(g) => {
            broadcast_glossary(&state, &room, &g);
            glossary_response(&g, svc.max_entries())
        }
        Err(e) => {
            tracing::error!("glossary import save failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

// ---- AI session report (spec 0014) ------------------------------------------

#[derive(Deserialize, Default)]
pub struct AiReportRequest {
    /// `structured` (default) | `freeform`.
    pub format: Option<String>,
    /// Report language; default = the requester's own participant language.
    pub lang: Option<String>,
    /// Free-text steering for the model, ≤ 2000 chars.
    pub guidelines: Option<String>,
}

/// A [`ReportRow`](ai_report::ReportRow) as the client sees it: `cost`
/// converted from Decimal (which rust_decimal serializes as a JSON *string*)
/// to a plain number, matching every other money field we expose.
fn report_json(row: &ai_report::ReportRow) -> serde_json::Value {
    let mut v = serde_json::to_value(row).unwrap_or_default();
    v["cost"] = serde_json::json!(row.cost.to_f64().unwrap_or(0.0));
    v
}

/// `POST /api/sessions/{id}/report` — generate an AI report (charged).
///
/// Failure-mode policy, user-favorable throughout:
/// * Groq fails → 502, nothing charged.
/// * Balance dropped below cost between pre-check and deduct → 402, report
///   withheld (delivering would make 402-then-regenerate a free path).
/// * Deduct fails for any *other* reason (DB hiccup) after generation →
///   deliver the report FREE and log loudly — never charge-or-lose paid AI
///   output over our own infra error.
/// * Persisting the row fails after a successful charge → still return the
///   markdown (the user paid for it); it just won't show up in GET later.
pub async fn report_generate(
    State(state): State<AppState>,
    user: AuthUser,
    Path(session_id): Path<Uuid>,
    Json(body): Json<AiReportRequest>,
) -> Response {
    // Throttle paid AI generation per user (spec 0028): credits gate cost but not
    // burst concurrency, and a stuck client could hammer Groq.
    if !state
        .rate_limiter
        .allow(&format!("ai:{}", user.user_id), 10, Duration::from_secs(60))
    {
        return (StatusCode::TOO_MANY_REQUESTS, "too many requests").into_response();
    }
    let (Some(svc), Some(billing), Some(pool), Some(cfg)) = (
        state.transcripts.as_ref(),
        state.billing.as_ref(),
        state.pool.as_ref(),
        state.config.billing.as_ref(),
    ) else {
        return service_unavailable();
    };
    let ai = &cfg.ai;

    let format = match body.format.as_deref() {
        None => "structured",
        Some(f @ ("structured" | "freeform")) => f,
        Some(_) => {
            return (
                StatusCode::BAD_REQUEST,
                "format must be structured or freeform",
            )
                .into_response()
        }
    };
    let guidelines = match body.guidelines.as_deref().map(str::trim) {
        Some(g) if g.chars().count() > 2000 => {
            return (
                StatusCode::BAD_REQUEST,
                "guidelines too long (max 2000 chars)",
            )
                .into_response()
        }
        Some(g) if !g.is_empty() => Some(g.to_string()),
        _ => None,
    };

    // Barrier so a report requested right after leaving sees the final events.
    svc.flush().await;
    if let Err(resp) = session_gate(svc, session_id, user.user_id).await {
        return resp;
    }

    // Report language: explicit param > requester's participant lang > en.
    let lang = match body.lang.as_deref().map(str::trim) {
        Some(l) if !l.is_empty() => {
            if l.len() > 8 || !l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                return (StatusCode::BAD_REQUEST, "invalid lang").into_response();
            }
            l.to_string()
        }
        _ => svc
            .participant_lang(session_id, user.user_id)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| "en".to_string()),
    };

    let export = match svc.export(session_id).await {
        Ok(Some(doc)) => doc,
        // Purged between the access check and here (guest-only finalize race).
        Ok(None) => return (StatusCode::NOT_FOUND, "no such session").into_response(),
        Err(e) => {
            tracing::error!("transcript export failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };
    if export.events.is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            "session has no transcript to report on",
        )
            .into_response();
    }

    let cost = ai_report::report_cost(ai, export.session.duration_seconds);

    // Advisory pre-check: fail fast before burning an expensive Groq call.
    // The atomic deduct below remains the real gate.
    match billing.get_balance(user.user_id).await {
        Ok(b) if b < cost => return insufficient_credits("ai_report", cost, b),
        Ok(_) => {}
        Err(e) => {
            tracing::error!("balance check failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    }

    // Generation fans out into many Groq calls; on a multi-hour transcript that
    // exceeds the edge proxy's request ceiling, so run it as a background job and
    // return a handle the client polls. Identical in-flight requests (a
    // double-click → same params_key) join the running job instead of charging
    // twice; a regenerate (terminal job) reclaims the slot.
    let params_key = format!(
        "{format}\u{1f}{lang}\u{1f}{}",
        guidelines.as_deref().unwrap_or("")
    );
    let job_id = match ai_jobs::claim(pool, session_id, user.user_id, "report", &params_key).await {
        Ok(ai_jobs::Claim::Owned(id)) => {
            let st = state.clone();
            let (uid, fmt, lng, gl) = (user.user_id, format.to_string(), lang, guidelines);
            tokio::spawn(ai_jobs::run(pool.clone(), id, async move {
                run_report_inner(st, session_id, uid, export, cost, fmt, lng, gl).await
            }));
            id
        }
        Ok(ai_jobs::Claim::AlreadyRunning(id)) => id,
        Err(e) => {
            tracing::error!("ai_report job claim failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };
    ai_job_accepted(job_id)
}

/// Background body of [`report_generate`]: generate → charge → persist, returning
/// the JSON the synchronous endpoint used to return (or a [`JobFailure`]). The
/// user-favorable failure policy is unchanged: Groq failure never charges;
/// `InsufficientFunds` at deduct withholds the report (and carries the 402 body);
/// our own deduct/insert errors deliver the report free.
#[allow(clippy::too_many_arguments)]
async fn run_report_inner(
    state: AppState,
    session_id: Uuid,
    user_id: Uuid,
    export: TranscriptExport,
    cost: Decimal,
    format: String,
    lang: String,
    guidelines: Option<String>,
) -> Result<serde_json::Value, ai_jobs::JobFailure> {
    let (Some(billing), Some(pool), Some(cfg)) = (
        state.billing.as_ref(),
        state.pool.as_ref(),
        state.config.billing.as_ref(),
    ) else {
        return Err(ai_jobs::JobFailure::new("unavailable"));
    };
    let ai = &cfg.ai;

    let (markdown, model) = match ai_report::generate_report(
        &state.groq,
        ai,
        &export,
        &format,
        &lang,
        guidelines.as_deref(),
    )
    .await
    {
        Ok(out) => out,
        Err(e) => {
            tracing::error!("report generation failed: {e}");
            return Err(ai_jobs::JobFailure::new("groq"));
        }
    };

    let balance = match billing
        .deduct_feature(
            user_id,
            Some(session_id),
            "ai_report",
            cost,
            &format!("AI report — room {}", export.session.room_name),
            serde_json::json!({ "format": format, "lang": lang, "model": model }),
        )
        .await
    {
        Ok(b) => Some(b),
        Err(BillingError::InsufficientFunds) => {
            let available = billing.get_balance(user_id).await.unwrap_or(Decimal::ZERO);
            return Err(ai_jobs::JobFailure::with_payload(
                "insufficient_credits",
                insufficient_body("ai_report", cost, available),
            ));
        }
        Err(e) => {
            tracing::error!("ai_report deduction failed AFTER generation — delivering free: {e}");
            None
        }
    };

    let v = match ai_report::save_report(
        pool,
        session_id,
        user_id,
        &format,
        &lang,
        guidelines.as_deref(),
        &markdown,
        &model,
        cost,
    )
    .await
    {
        Ok(row) => {
            let mut v = report_json(&row);
            if let Some(b) = balance {
                v["balance"] = serde_json::json!(b.to_f64().unwrap_or(0.0));
            }
            v
        }
        Err(e) => {
            // Charged but couldn't persist — deliver the markdown anyway.
            tracing::error!("report insert failed after charge: {e}");
            let mut v = serde_json::json!({
                "format": format,
                "lang": lang,
                "guidelines": guidelines,
                "markdown": markdown,
                "model": model,
                "cost": cost.to_f64().unwrap_or(0.0),
            });
            if let Some(b) = balance {
                v["balance"] = serde_json::json!(b.to_f64().unwrap_or(0.0));
            }
            v
        }
    };
    Ok(v)
}

/// `GET /api/sessions/{id}/report` — the latest stored report. Any participant
/// can read it (404 when none has been generated yet).
pub async fn report_latest(
    State(state): State<AppState>,
    user: AuthUser,
    Path(session_id): Path<Uuid>,
) -> Response {
    let (Some(svc), Some(pool)) = (state.transcripts.as_ref(), state.pool.as_ref()) else {
        return service_unavailable();
    };
    if let Err(resp) = session_gate(svc, session_id, user.user_id).await {
        return resp;
    }
    match ai_report::latest_report(pool, session_id).await {
        Ok(Some(row)) => Json(report_json(&row)).into_response(),
        // 200 + null (not 404): "no report yet" is a normal state the session-detail
        // page polls on every open, and a 404 just spams the browser console (#post-call).
        Ok(None) => Json(serde_json::Value::Null).into_response(),
        Err(e) => {
            tracing::error!("report load failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

// ---- Quiz history (spec 0098 / #221) ----------------------------------------

/// `GET /api/sessions/{id}/quizzes` — the quizzes run in the call + per-participant
/// scores. Any participant of the session may read.
pub async fn quizzes_list(
    State(state): State<AppState>,
    user: AuthUser,
    Path(session_id): Path<Uuid>,
) -> Response {
    let (Some(svc), Some(pool)) = (state.transcripts.as_ref(), state.pool.as_ref()) else {
        return service_unavailable();
    };
    if let Err(resp) = session_gate(svc, session_id, user.user_id).await {
        return resp;
    }
    match crate::quiz_history::session_quizzes(pool, session_id).await {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => {
            tracing::error!("list quizzes failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

/// `POST /api/sessions/{id}/quizzes` — persist a finished quiz + its results. Sent
/// once, by the host, when a quiz completes. The poster must be a participant.
pub async fn quiz_save(
    State(state): State<AppState>,
    user: AuthUser,
    Path(session_id): Path<Uuid>,
    Json(input): Json<crate::quiz_history::QuizInput>,
) -> Response {
    let (Some(svc), Some(pool)) = (state.transcripts.as_ref(), state.pool.as_ref()) else {
        return service_unavailable();
    };
    if let Err(resp) = session_gate(svc, session_id, user.user_id).await {
        return resp;
    }
    match crate::quiz_history::save_quiz(pool, session_id, Some(user.user_id), &input).await {
        Ok(id) => (StatusCode::CREATED, Json(serde_json::json!({ "id": id }))).into_response(),
        Err(e) => {
            tracing::error!("save quiz failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

// ---- Sentiment analysis (spec 0015) -----------------------------------------

fn sentiment_json(row: &ai_sentiment::SentimentRow, cached: bool) -> serde_json::Value {
    let mut v = serde_json::to_value(row).unwrap_or_default();
    // rust_decimal serializes as a string; the client wants a number.
    v["cost"] = serde_json::json!(row.cost.to_f64().unwrap_or(0.0));
    v["cached"] = serde_json::json!(cached);
    v
}

/// `POST /api/sessions/{id}/sentiment` — analyze once, then cache: the
/// UNIQUE(session_id) row is the contract, so any later POST (from any
/// participant) returns the stored result without charging anyone.
pub async fn sentiment_generate(
    State(state): State<AppState>,
    user: AuthUser,
    Path(session_id): Path<Uuid>,
) -> Response {
    if !state
        .rate_limiter
        .allow(&format!("ai:{}", user.user_id), 10, Duration::from_secs(60))
    {
        return (StatusCode::TOO_MANY_REQUESTS, "too many requests").into_response();
    }
    let (Some(svc), Some(billing), Some(pool), Some(cfg)) = (
        state.transcripts.as_ref(),
        state.billing.as_ref(),
        state.pool.as_ref(),
        state.config.billing.as_ref(),
    ) else {
        return service_unavailable();
    };
    let ai = &cfg.ai;

    // Barrier so an analysis requested right after leaving sees every event.
    svc.flush().await;
    if let Err(resp) = session_gate(svc, session_id, user.user_id).await {
        return resp;
    }

    // Cache hit: already analyzed — return it, charge nothing.
    match ai_sentiment::get_sentiment(pool, session_id).await {
        Ok(Some(row)) => return Json(sentiment_json(&row, true)).into_response(),
        Ok(None) => {}
        Err(e) => {
            tracing::error!("sentiment load failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    }

    let export = match svc.export(session_id).await {
        Ok(Some(doc)) => doc,
        // Purged between the access check and here (guest-only finalize race).
        Ok(None) => return (StatusCode::NOT_FOUND, "no such session").into_response(),
        Err(e) => {
            tracing::error!("transcript export failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };
    if export.events.is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            "session has no transcript to analyze",
        )
            .into_response();
    }

    let cost = ai_sentiment::sentiment_cost(
        ai,
        export.session.participants.len(),
        export.session.duration_seconds,
    );

    // Advisory pre-check: fail fast before burning the Groq calls.
    // The atomic deduct below remains the real gate.
    match billing.get_balance(user.user_id).await {
        Ok(b) if b < cost => return insufficient_credits("ai_sentiment", cost, b),
        Ok(_) => {}
        Err(e) => {
            tracing::error!("balance check failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    }

    // Analysis chunks the whole transcript and fans out into many Groq calls —
    // too slow for one request on a long call — so run it as a background job.
    // No params (per-session cache), so the slot key is empty; a double-click
    // joins the running job instead of charging twice.
    let job_id = match ai_jobs::claim(pool, session_id, user.user_id, "sentiment", "").await {
        Ok(ai_jobs::Claim::Owned(id)) => {
            let st = state.clone();
            let uid = user.user_id;
            tokio::spawn(ai_jobs::run(pool.clone(), id, async move {
                run_sentiment_inner(st, session_id, uid, export, cost).await
            }));
            id
        }
        Ok(ai_jobs::Claim::AlreadyRunning(id)) => id,
        Err(e) => {
            tracing::error!("ai_sentiment job claim failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };
    ai_job_accepted(job_id)
}

/// Background body of [`sentiment_generate`]: analyze → charge → persist,
/// returning the JSON the synchronous endpoint used to return (or a
/// [`JobFailure`]). Same user-favorable failure policy as the other AI jobs.
async fn run_sentiment_inner(
    state: AppState,
    session_id: Uuid,
    user_id: Uuid,
    export: TranscriptExport,
    cost: Decimal,
) -> Result<serde_json::Value, ai_jobs::JobFailure> {
    let (Some(billing), Some(pool), Some(cfg)) = (
        state.billing.as_ref(),
        state.pool.as_ref(),
        state.config.billing.as_ref(),
    ) else {
        return Err(ai_jobs::JobFailure::new("unavailable"));
    };
    let ai = &cfg.ai;

    let (result, model) = match ai_sentiment::analyze(&state.groq, ai, &export).await {
        Ok(out) => out,
        Err(e) => {
            tracing::error!("sentiment analysis failed: {e}");
            return Err(ai_jobs::JobFailure::new("groq"));
        }
    };

    let balance = match billing
        .deduct_feature(
            user_id,
            Some(session_id),
            "ai_sentiment",
            cost,
            &format!("Sentiment analysis — room {}", export.session.room_name),
            serde_json::json!({ "model": model }),
        )
        .await
    {
        Ok(b) => Some(b),
        // Charging after delivery would make 402-then-retry a free path, so
        // a genuine InsufficientFunds at the gate withholds the result.
        Err(BillingError::InsufficientFunds) => {
            let available = billing.get_balance(user_id).await.unwrap_or(Decimal::ZERO);
            return Err(ai_jobs::JobFailure::with_payload(
                "insufficient_credits",
                insufficient_body("ai_sentiment", cost, available),
            ));
        }
        // Any other failure is ours, not the user's: deliver free and log.
        Err(e) => {
            tracing::error!("ai_sentiment deduction failed AFTER analysis — delivering free: {e}");
            None
        }
    };

    let v = match ai_sentiment::save_sentiment(pool, session_id, user_id, &result, &model, cost)
        .await
    {
        Ok(Some(row)) => {
            let mut v = sentiment_json(&row, false);
            if let Some(b) = balance {
                v["balance"] = serde_json::json!(b.to_f64().unwrap_or(0.0));
            }
            v
        }
        // Lost the UNIQUE race or the insert failed — we already charged, so
        // deliver what we computed rather than a confusing error.
        other => {
            if let Err(e) = other {
                tracing::error!("sentiment insert failed after charge: {e}");
            }
            let mut v = serde_json::json!({
                "result": result,
                "model": model,
                "cost": cost.to_f64().unwrap_or(0.0),
                "cached": false,
            });
            if let Some(b) = balance {
                v["balance"] = serde_json::json!(b.to_f64().unwrap_or(0.0));
            }
            v
        }
    };
    Ok(v)
}

/// `GET /api/sessions/{id}/sentiment` — the cached analysis. Any participant
/// can read it (404 when nobody has run it yet).
pub async fn sentiment_latest(
    State(state): State<AppState>,
    user: AuthUser,
    Path(session_id): Path<Uuid>,
) -> Response {
    let (Some(svc), Some(pool)) = (state.transcripts.as_ref(), state.pool.as_ref()) else {
        return service_unavailable();
    };
    if let Err(resp) = session_gate(svc, session_id, user.user_id).await {
        return resp;
    }
    match ai_sentiment::get_sentiment(pool, session_id).await {
        Ok(Some(row)) => Json(sentiment_json(&row, true)).into_response(),
        // 200 + null (not 404): polled on every session-detail open; avoids console spam.
        Ok(None) => Json(serde_json::Value::Null).into_response(),
        Err(e) => {
            tracing::error!("sentiment load failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

#[derive(Deserialize, Default)]
pub struct CorrectionQuery {
    /// `original` (default) | `translated` | `both` — which text the export shows.
    pub mode: Option<String>,
    /// Target language for `translated`/`both`; ignored for `original`.
    pub lang: Option<String>,
}

/// Resolve `(mode, normalized lang)` from the query. `Original` forces lang to
/// "" (its cache key); `translated`/`both` resolve an explicit param, else the
/// requester's participant language, else `en`.
async fn resolve_correction_params(
    svc: &TranscriptService,
    session_id: Uuid,
    user_id: Uuid,
    q: &CorrectionQuery,
) -> Result<(CorrectionMode, String), Response> {
    let mode = match q.mode.as_deref() {
        None => CorrectionMode::Original,
        Some(m) => CorrectionMode::parse(m).ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                "mode must be original, translated or both",
            )
                .into_response()
        })?,
    };
    let lang = if mode == CorrectionMode::Original {
        String::new()
    } else {
        match q.lang.as_deref().map(str::trim) {
            Some(l) if !l.is_empty() => {
                if l.len() > 8 || !l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                    return Err((StatusCode::BAD_REQUEST, "invalid lang").into_response());
                }
                l.to_string()
            }
            _ => svc
                .participant_lang(session_id, user_id)
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| "en".to_string()),
        }
    };
    Ok((mode, lang))
}

/// `POST /api/sessions/{id}/correction?mode=&lang=` — ensure a cached AI
/// correction exists for this export shape, charging once (spec 0068).
///
/// Same user-favorable failure policy as the report/sentiment endpoints:
/// Groq fails → 502 uncharged; balance below cost → 402; a post-generation
/// deduct failure for any other reason delivers the correction free and logs.
/// The corrected text itself is served by the corrected download, not here.
pub async fn correction_generate(
    State(state): State<AppState>,
    user: AuthUser,
    Path(session_id): Path<Uuid>,
    Query(q): Query<CorrectionQuery>,
) -> Response {
    if !state
        .rate_limiter
        .allow(&format!("ai:{}", user.user_id), 10, Duration::from_secs(60))
    {
        return (StatusCode::TOO_MANY_REQUESTS, "too many requests").into_response();
    }
    let (Some(svc), Some(billing), Some(pool), Some(cfg)) = (
        state.transcripts.as_ref(),
        state.billing.as_ref(),
        state.pool.as_ref(),
        state.config.billing.as_ref(),
    ) else {
        return service_unavailable();
    };
    let ai = &cfg.ai;

    let (mode, lang) = match resolve_correction_params(svc, session_id, user.user_id, &q).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    // Barrier so a correction requested right after leaving sees every event.
    svc.flush().await;
    if let Err(resp) = session_gate(svc, session_id, user.user_id).await {
        return resp;
    }

    // Cache hit: already corrected this exact shape — return it, charge nothing.
    match ai_correction::get_correction(pool, session_id, mode, &lang).await {
        Ok(Some(row)) => return Json(correction_meta(&row, true, false, None)).into_response(),
        Ok(None) => {}
        Err(e) => {
            tracing::error!("correction load failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    }

    let export = match svc.export(session_id).await {
        Ok(Some(doc)) => doc,
        Ok(None) => return (StatusCode::NOT_FOUND, "no such session").into_response(),
        Err(e) => {
            tracing::error!("transcript export failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };
    if export.events.is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            "session has no transcript to correct",
        )
            .into_response();
    }

    let cost = ai_correction::correction_cost(ai, mode, export.events.len() as i64);

    // Advisory pre-check: fail fast before burning the Groq calls.
    match billing.get_balance(user.user_id).await {
        Ok(b) if b < cost => return insufficient_credits("ai_correction", cost, b),
        Ok(_) => {}
        Err(e) => {
            tracing::error!("balance check failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    }

    // Correcting a long transcript fans out into many Groq batches — far longer
    // than the edge proxy's request ceiling — so run it as a background job. A
    // double-click (same mode+lang) joins the running job rather than charging
    // twice; the corrected text itself is still served by the corrected download.
    let params_key = format!("{}\u{1f}{lang}", mode.as_str());
    let job_id =
        match ai_jobs::claim(pool, session_id, user.user_id, "correction", &params_key).await {
            Ok(ai_jobs::Claim::Owned(id)) => {
                let st = state.clone();
                let (uid, lng) = (user.user_id, lang);
                tokio::spawn(ai_jobs::run(pool.clone(), id, async move {
                    run_correction_inner(st, session_id, uid, export, cost, mode, lng).await
                }));
                id
            }
            Ok(ai_jobs::Claim::AlreadyRunning(id)) => id,
            Err(e) => {
                tracing::error!("ai_correction job claim failed: {e}");
                return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
            }
        };
    ai_job_accepted(job_id)
}

/// Background body of [`correction_generate`]: generate → charge → persist,
/// returning the metadata the synchronous endpoint used to return (or a
/// [`JobFailure`]). Same user-favorable failure policy as the report path.
async fn run_correction_inner(
    state: AppState,
    session_id: Uuid,
    user_id: Uuid,
    export: TranscriptExport,
    cost: Decimal,
    mode: CorrectionMode,
    lang: String,
) -> Result<serde_json::Value, ai_jobs::JobFailure> {
    let (Some(billing), Some(pool), Some(cfg)) = (
        state.billing.as_ref(),
        state.pool.as_ref(),
        state.config.billing.as_ref(),
    ) else {
        return Err(ai_jobs::JobFailure::new("unavailable"));
    };
    let ai = &cfg.ai;

    let (lines, model) =
        match ai_correction::generate_correction(&state.groq, ai, &export, mode, &lang).await {
            Ok(out) => out,
            Err(e) => {
                tracing::error!("correction generation failed: {e}");
                return Err(ai_jobs::JobFailure::new("groq"));
            }
        };

    let balance = match billing
        .deduct_feature(
            user_id,
            Some(session_id),
            "ai_correction",
            cost,
            &format!(
                "AI transcript correction — room {}",
                export.session.room_name
            ),
            serde_json::json!({ "mode": mode.as_str(), "lang": lang, "model": model }),
        )
        .await
    {
        Ok(b) => Some(b),
        Err(BillingError::InsufficientFunds) => {
            let available = billing.get_balance(user_id).await.unwrap_or(Decimal::ZERO);
            return Err(ai_jobs::JobFailure::with_payload(
                "insufficient_credits",
                insufficient_body("ai_correction", cost, available),
            ));
        }
        Err(e) => {
            tracing::error!(
                "ai_correction deduction failed AFTER generation — delivering free: {e}"
            );
            None
        }
    };

    // Persist for the corrected download + future free re-exports. Losing the
    // UNIQUE race means a concurrent request already cached it: we charged, so
    // report success rather than a confusing error.
    let charged = balance.is_some();
    let v = match ai_correction::save_correction(
        pool, session_id, user_id, mode, &lang, &lines, &model, cost,
    )
    .await
    {
        Ok(Some(row)) => correction_meta(&row, false, charged, balance),
        other => {
            if let Err(e) = other {
                tracing::error!("correction insert failed after charge: {e}");
            }
            let mut v = serde_json::json!({
                "cached": false,
                "charged": charged,
                "cost": cost.to_f64().unwrap_or(0.0),
                "mode": mode.as_str(),
                "lang": lang,
                "model": model,
                "event_count": lines.len(),
            });
            if let Some(b) = balance {
                v["balance"] = serde_json::json!(b.to_f64().unwrap_or(0.0));
            }
            v
        }
    };
    Ok(v)
}

/// Correction metadata for the client (never the corrected text — that comes
/// from the corrected download).
fn correction_meta(
    row: &ai_correction::CorrectionRow,
    cached: bool,
    charged: bool,
    balance: Option<Decimal>,
) -> serde_json::Value {
    let mut v = serde_json::json!({
        "cached": cached,
        "charged": charged,
        "cost": row.cost.to_f64().unwrap_or(0.0),
        "mode": row.mode,
        "lang": row.lang,
        "model": row.model,
    });
    if let Some(b) = balance {
        v["balance"] = serde_json::json!(b.to_f64().unwrap_or(0.0));
    }
    v
}

/// `GET /api/sessions/{id}/correction?mode=&lang=` — whether a correction is
/// already cached for this export shape (so the client can label it free).
pub async fn correction_status(
    State(state): State<AppState>,
    user: AuthUser,
    Path(session_id): Path<Uuid>,
    Query(q): Query<CorrectionQuery>,
) -> Response {
    let (Some(svc), Some(pool)) = (state.transcripts.as_ref(), state.pool.as_ref()) else {
        return service_unavailable();
    };
    let (mode, lang) = match resolve_correction_params(svc, session_id, user.user_id, &q).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    if let Err(resp) = session_gate(svc, session_id, user.user_id).await {
        return resp;
    }
    match ai_correction::get_correction(pool, session_id, mode, &lang).await {
        Ok(Some(row)) => Json(correction_meta(&row, true, false, None)).into_response(),
        Ok(None) => Json(serde_json::json!({
            "cached": false,
            "mode": mode.as_str(),
            "lang": lang,
        }))
        .into_response(),
        Err(e) => {
            tracing::error!("correction load failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

// ---- AI quiz on demand (spec 0067 / #124) -----------------------------------

#[derive(Deserialize)]
pub struct QuizGenerateRequest {
    pub prompt: String,
    #[serde(default)]
    pub count: Option<usize>,
    #[serde(default)]
    pub lang: Option<String>,
    /// Distinct languages currently in the room, so the quiz can be localized for
    /// every player (each question/option is returned keyed by language). Empty /
    /// absent → single-language quiz in `lang` (the previous behaviour).
    #[serde(default)]
    pub langs: Option<Vec<String>>,
}

/// Localize generated questions into the room's languages: each question's stem
/// and options are translated from `base` into every `target`, returned keyed by
/// language so each client renders its own. All fan-outs run concurrently (the
/// translator's own admission cap bounds the load). With no targets this just
/// wraps the base-language text — cheap and behaviour-preserving.
async fn localize_quiz(
    translator: &crate::translator::Translator,
    questions: &[ai_quiz::QuizQuestion],
    base: &str,
    targets: &[String],
) -> Vec<serde_json::Value> {
    use futures::future::join_all;
    let per_q = questions.iter().map(|q| async move {
        let stem_fut = translator.translate_fanout(&q.q, base, targets, None);
        let opt_futs = q
            .options
            .iter()
            .map(|o| translator.translate_fanout(o, base, targets, None));
        let (q_map, opt_maps) = tokio::join!(stem_fut, join_all(opt_futs));
        // Re-key options per language: for each language, its 4 options in order.
        let mut options: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for lang in std::iter::once(base.to_string()).chain(targets.iter().cloned()) {
            let opts: Vec<String> = opt_maps
                .iter()
                .map(|m| m.get(&lang).cloned().unwrap_or_default())
                .collect();
            options.insert(lang, opts);
        }
        serde_json::json!({ "answer": q.answer, "q": q_map, "options": options })
    });
    join_all(per_q).await
}

/// Normalize a client-supplied language code for the prompt: lowercase
/// alphanumerics/`-`, capped to 8 chars, defaulting to English. It is only ever
/// interpolated into the prompt text, never trusted otherwise.
fn sanitize_lang(lang: Option<&str>) -> String {
    let cleaned: String = lang
        .unwrap_or("en")
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(8)
        .collect();
    if cleaned.is_empty() {
        "en".to_string()
    } else {
        cleaned
    }
}

/// `POST /api/quiz/generate` — generate a custom multiple-choice quiz from a
/// prompt via Groq and charge credits. Same safe billing order as the other AI
/// features (advisory pre-check → generate → atomic deduct): a generation
/// failure is never charged, and a genuine 402 withholds the result. Stateless —
/// the pack plays client-side through the existing quiz engine (spec 0047).
pub async fn quiz_generate(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<QuizGenerateRequest>,
) -> Response {
    if !state
        .rate_limiter
        .allow(&format!("ai:{}", user.user_id), 10, Duration::from_secs(60))
    {
        return (StatusCode::TOO_MANY_REQUESTS, "too many requests").into_response();
    }
    let (Some(billing), Some(cfg)) = (state.billing.as_ref(), state.config.billing.as_ref()) else {
        return service_unavailable();
    };
    let ai = &cfg.ai;

    // Sanitize the prompt: trim, length-cap, moderate (severe → reject).
    let prompt = body.prompt.trim();
    if prompt.is_empty() {
        return (StatusCode::UNPROCESSABLE_ENTITY, "prompt is empty").into_response();
    }
    if prompt.chars().count() > ai_quiz::MAX_PROMPT_CHARS {
        return (StatusCode::UNPROCESSABLE_ENTITY, "prompt too long").into_response();
    }
    if state.moderator.severity(prompt) == Severity::Severe {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            "prompt blocked by moderation",
        )
            .into_response();
    }
    let count = ai_quiz::clamp_count(body.count);
    let lang = sanitize_lang(body.lang.as_deref());
    // Languages to localize into: the room's distinct languages, deduped, base
    // excluded, capped. The quiz is produced in 1 (base) + targets languages, and
    // the cost scales with that so a multilingual room is charged for the extra
    // translation work (single-language rooms are unchanged).
    let mut targets: Vec<String> = body
        .langs
        .clone()
        .unwrap_or_default()
        .iter()
        .map(|l| sanitize_lang(Some(l)))
        .filter(|l| l != &lang)
        .collect();
    targets.sort();
    targets.dedup();
    targets.truncate(8); // bound the translation fan-out per quiz
    let num_langs = 1 + targets.len();
    let cost = ai_quiz::quiz_cost(ai, count, num_langs);

    // Advisory pre-check before burning a Groq call; the atomic deduct is the gate.
    match billing.get_balance(user.user_id).await {
        Ok(b) if b < cost => return insufficient_credits("ai_quiz", cost, b),
        Ok(_) => {}
        Err(e) => {
            tracing::error!("balance check failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    }

    let (questions, model) = match ai_quiz::generate(&state.groq, ai, prompt, count, &lang).await {
        Ok(out) => out,
        Err(e) => {
            tracing::error!("quiz generation failed: {e}");
            return (
                StatusCode::BAD_GATEWAY,
                "quiz generation failed — you were not charged",
            )
                .into_response();
        }
    };

    let balance = match billing
        .deduct_feature(
            user.user_id,
            None,
            "ai_quiz",
            cost,
            &format!("AI quiz — {} questions", questions.len()),
            serde_json::json!({ "model": model, "count": questions.len() }),
        )
        .await
    {
        Ok(b) => Some(b),
        // Balance dropped between the pre-check and here: withhold, don't deliver free.
        Err(BillingError::InsufficientFunds) => {
            let available = billing
                .get_balance(user.user_id)
                .await
                .unwrap_or(Decimal::ZERO);
            return insufficient_credits("ai_quiz", cost, available);
        }
        // Our fault, not the user's: deliver free and log.
        Err(e) => {
            tracing::error!("ai_quiz deduction failed AFTER generation — delivering free: {e}");
            None
        }
    };

    // `targets` was computed up-front (it drives the cost); reuse it here.
    let localized = localize_quiz(&state.translator, &questions, &lang, &targets).await;

    let mut v = serde_json::json!({
        "questions": localized,
        "cost": cost.to_f64().unwrap_or(0.0),
    });
    if let Some(b) = balance {
        v["balance"] = serde_json::json!(b.to_f64().unwrap_or(0.0));
    }
    (StatusCode::CREATED, Json(v)).into_response()
}

// ---- Follow-up email (spec 0016) ---------------------------------------------

#[derive(Deserialize)]
pub struct EmailDraftRequest {
    pub recipients: Vec<ai_email::RecipientRef>,
    /// `professional` (default) | `friendly` | `concise` — whitelisted so it
    /// can sit in a prompt directive slot.
    pub tone: Option<String>,
    /// Free-text steering for the model, ≤ 2000 chars.
    pub guidelines: Option<String>,
    /// Email language; default = the requester's own participant language.
    pub lang: Option<String>,
    /// Lead with the stored report's executive summary (default true).
    pub include_summary: Option<bool>,
}

/// An [`EmailRow`](ai_email::EmailRow) as the client sees it. Never exposes
/// `body_html` (rebuilt from edited text at send time) or recipient `user_id`s
/// — and stored raw addresses only ever reach their owner (`latest_email` is
/// owner-scoped).
fn email_json(row: &ai_email::EmailRow, cost: Option<Decimal>) -> serde_json::Value {
    let mut v = serde_json::json!({
        "id": row.id,
        "status": row.status,
        "subject": row.subject,
        "body_text": row.body_text,
        "recipients": ai_email::sanitize_recipients(&row.recipients),
        "tone": row.tone,
        "guidelines": row.guidelines,
        "lang": row.lang,
        "resend_id": row.resend_id,
        "sent_at": row.sent_at,
        "created_at": row.created_at,
    });
    if let Some(c) = cost {
        v["cost"] = serde_json::json!(c.to_f64().unwrap_or(0.0));
    }
    v
}

/// `POST /api/sessions/{id}/email-draft` — generate a follow-up email draft
/// (charged flat `CREDITS_EMAIL_DRAFT`). Same user-favorable failure policy as
/// the report: Groq fail → 502 unchanged; InsufficientFunds at the gate → 402
/// withheld; our own deduct/insert errors never charge-or-lose paid output.
pub async fn email_draft_generate(
    State(state): State<AppState>,
    user: AuthUser,
    Path(session_id): Path<Uuid>,
    Json(body): Json<EmailDraftRequest>,
) -> Response {
    if !state
        .rate_limiter
        .allow(&format!("ai:{}", user.user_id), 10, Duration::from_secs(60))
    {
        return (StatusCode::TOO_MANY_REQUESTS, "too many requests").into_response();
    }
    let (Some(svc), Some(billing), Some(pool), Some(cfg)) = (
        state.transcripts.as_ref(),
        state.billing.as_ref(),
        state.pool.as_ref(),
        state.config.billing.as_ref(),
    ) else {
        return service_unavailable();
    };
    if state.resend.is_none() {
        return (StatusCode::SERVICE_UNAVAILABLE, "email not configured").into_response();
    }
    let ai = &cfg.ai;

    let tone = match body.tone.as_deref() {
        None => None,
        Some(t @ ("professional" | "friendly" | "concise")) => Some(t),
        Some(_) => {
            return (
                StatusCode::BAD_REQUEST,
                "tone must be professional, friendly or concise",
            )
                .into_response()
        }
    };
    let guidelines = match body.guidelines.as_deref().map(str::trim) {
        Some(g) if g.chars().count() > 2000 => {
            return (
                StatusCode::BAD_REQUEST,
                "guidelines too long (max 2000 chars)",
            )
                .into_response()
        }
        Some(g) if !g.is_empty() => Some(g.to_string()),
        _ => None,
    };

    // Barrier so a draft requested right after leaving sees the final events.
    svc.flush().await;
    if let Err(resp) = session_gate(svc, session_id, user.user_id).await {
        return resp;
    }

    // Email language: explicit param > requester's participant lang > en.
    let lang = match body.lang.as_deref().map(str::trim) {
        Some(l) if !l.is_empty() => {
            if l.len() > 8 || !l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                return (StatusCode::BAD_REQUEST, "invalid lang").into_response();
            }
            l.to_string()
        }
        _ => svc
            .participant_lang(session_id, user.user_id)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| "en".to_string()),
    };

    // Resolve recipient refs against the session roster (guests rejected,
    // dupes collapsed). user_ids land in the stored JSONB — never in responses.
    let participants = match ai_email::session_participants(pool, session_id).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("participant load failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };
    let recipients = match ai_email::resolve_recipients(&body.recipients, &participants) {
        Ok(r) => r,
        Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
    };

    let export = match svc.export(session_id).await {
        Ok(Some(doc)) => doc,
        // Purged between the access check and here (guest-only finalize race).
        Ok(None) => return (StatusCode::NOT_FOUND, "no such session").into_response(),
        Err(e) => {
            tracing::error!("transcript export failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };
    if export.events.is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            "session has no transcript to draft from",
        )
            .into_response();
    }

    let cost = ai_email::email_cost(ai);

    // Advisory pre-check: fail fast before burning an expensive Groq call.
    // The atomic deduct below remains the real gate.
    match billing.get_balance(user.user_id).await {
        Ok(b) if b < cost => return insufficient_credits("ai_email", cost, b),
        Ok(_) => {}
        Err(e) => {
            tracing::error!("balance check failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    }

    // Drafting condenses the whole transcript first — too slow for one request
    // on a long call — so run it as a background job. The params_key folds in the
    // recipients so a true double-click joins the running job (no double charge),
    // while a deliberate regenerate (different recipients/tone/...) starts fresh.
    let include_summary = body.include_summary.unwrap_or(true);
    let params_key = format!(
        "{}\u{1f}{lang}\u{1f}{}\u{1f}{include_summary}\u{1f}{}",
        tone.unwrap_or(""),
        guidelines.as_deref().unwrap_or(""),
        recipients,
    );
    let job_id =
        match ai_jobs::claim(pool, session_id, user.user_id, "email_draft", &params_key).await {
            Ok(ai_jobs::Claim::Owned(id)) => {
                let st = state.clone();
                let uid = user.user_id;
                let tone_owned = tone.map(str::to_string);
                tokio::spawn(ai_jobs::run(pool.clone(), id, async move {
                    run_email_inner(
                        st,
                        session_id,
                        uid,
                        export,
                        cost,
                        recipients,
                        tone_owned,
                        lang,
                        guidelines,
                        include_summary,
                    )
                    .await
                }));
                id
            }
            Ok(ai_jobs::Claim::AlreadyRunning(id)) => id,
            Err(e) => {
                tracing::error!("ai_email job claim failed: {e}");
                return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
            }
        };
    ai_job_accepted(job_id)
}

/// Background body of [`email_draft_generate`]: pull the report summary (if any),
/// generate → charge → persist, returning the JSON the synchronous endpoint used
/// to return (or a [`JobFailure`]). Same user-favorable failure policy as report.
#[allow(clippy::too_many_arguments)]
async fn run_email_inner(
    state: AppState,
    session_id: Uuid,
    user_id: Uuid,
    export: TranscriptExport,
    cost: Decimal,
    recipients: serde_json::Value,
    tone: Option<String>,
    lang: String,
    guidelines: Option<String>,
    include_summary: bool,
) -> Result<serde_json::Value, ai_jobs::JobFailure> {
    let (Some(billing), Some(pool), Some(cfg)) = (
        state.billing.as_ref(),
        state.pool.as_ref(),
        state.config.billing.as_ref(),
    ) else {
        return Err(ai_jobs::JobFailure::new("unavailable"));
    };
    let ai = &cfg.ai;
    let tone = tone.as_deref();

    // Reuse the stored report's executive summary when available — better
    // grounding for free (no extra model call).
    let summary = if include_summary {
        match ai_report::latest_report(pool, session_id).await {
            Ok(Some(r)) => ai_email::extract_exec_summary(&r.markdown),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("report lookup for email summary failed: {e}");
                None
            }
        }
    } else {
        None
    };

    let (draft, model) = match ai_email::generate_draft(
        &state.groq,
        ai,
        &export,
        summary.as_deref(),
        tone,
        &lang,
        guidelines.as_deref(),
    )
    .await
    {
        Ok(out) => out,
        Err(e) => {
            tracing::error!("email draft failed: {e}");
            return Err(ai_jobs::JobFailure::new("groq"));
        }
    };

    let balance = match billing
        .deduct_feature(
            user_id,
            Some(session_id),
            "ai_email",
            cost,
            &format!("Follow-up email draft — room {}", export.session.room_name),
            serde_json::json!({ "model": model, "tone": tone }),
        )
        .await
    {
        Ok(b) => Some(b),
        // Charging after delivery would make 402-then-retry a free path, so
        // a genuine InsufficientFunds at the gate withholds the result.
        Err(BillingError::InsufficientFunds) => {
            let available = billing.get_balance(user_id).await.unwrap_or(Decimal::ZERO);
            return Err(ai_jobs::JobFailure::with_payload(
                "insufficient_credits",
                insufficient_body("ai_email", cost, available),
            ));
        }
        // Any other failure is ours, not the user's: deliver free and log.
        Err(e) => {
            tracing::error!("ai_email deduction failed AFTER generation — delivering free: {e}");
            None
        }
    };

    let v = match ai_email::save_email(
        pool,
        session_id,
        user_id,
        &draft,
        &recipients,
        tone,
        guidelines.as_deref(),
        &lang,
    )
    .await
    {
        Ok(row) => {
            let mut v = email_json(&row, Some(cost));
            if let Some(b) = balance {
                v["balance"] = serde_json::json!(b.to_f64().unwrap_or(0.0));
            }
            v
        }
        Err(e) => {
            // Charged but couldn't persist — deliver the draft anyway (it just
            // can't be sent: no id). The user keeps the text they paid for.
            tracing::error!("email insert failed after charge: {e}");
            let mut v = serde_json::json!({
                "status": "draft",
                "subject": draft.subject,
                "body_text": draft.body_text,
                "recipients": ai_email::sanitize_recipients(&recipients),
                "tone": tone,
                "guidelines": guidelines,
                "lang": lang,
                "cost": cost.to_f64().unwrap_or(0.0),
            });
            if let Some(b) = balance {
                v["balance"] = serde_json::json!(b.to_f64().unwrap_or(0.0));
            }
            v
        }
    };
    Ok(v)
}

#[derive(Deserialize)]
pub struct EmailSendRequest {
    pub email_id: Uuid,
    /// Pre-send edits; omitted fields keep the stored draft values.
    pub subject: Option<String>,
    pub body_text: Option<String>,
}

/// `POST /api/sessions/{id}/email-send` — send a draft through Resend. Free
/// (the charge was the draft). Owner-only; participant user_ids resolve to
/// addresses HERE, server-side only — they never appear in any response.
pub async fn email_send(
    State(state): State<AppState>,
    user: AuthUser,
    Path(session_id): Path<Uuid>,
    Json(body): Json<EmailSendRequest>,
) -> Response {
    // Tight per-user cap on outbound email (spec 0028) — protects the Resend
    // domain reputation from a user spamming follow-up sends.
    if !state.rate_limiter.allow(
        &format!("email:{}", user.user_id),
        5,
        Duration::from_secs(60),
    ) {
        return (StatusCode::TOO_MANY_REQUESTS, "too many requests").into_response();
    }
    let (Some(svc), Some(pool)) = (state.transcripts.as_ref(), state.pool.as_ref()) else {
        return service_unavailable();
    };
    let Some(resend) = state.resend.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "email not configured").into_response();
    };
    if let Err(resp) = session_gate(svc, session_id, user.user_id).await {
        return resp;
    }

    let row = match ai_email::get_email(pool, session_id, body.email_id).await {
        Ok(Some(row)) => row,
        Ok(None) => return (StatusCode::NOT_FOUND, "no such draft").into_response(),
        Err(e) => {
            tracing::error!("email load failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };
    if row.user_id != user.user_id {
        return (StatusCode::FORBIDDEN, "not your draft").into_response();
    }
    if row.status == "sent" {
        return (StatusCode::CONFLICT, "already sent").into_response();
    }

    // Apply pre-send edits; body_html is rebuilt from the edited text so the
    // two parts can't drift apart.
    let subject = match body.subject.as_deref().map(str::trim) {
        None => row.subject.clone(),
        Some("") => return (StatusCode::BAD_REQUEST, "subject cannot be empty").into_response(),
        Some(s) if s.chars().count() > 200 => {
            return (StatusCode::BAD_REQUEST, "subject too long (max 200 chars)").into_response()
        }
        Some(s) => s.to_string(),
    };
    let (body_text, body_html) = match body.body_text.as_deref().map(str::trim) {
        None => (row.body_text.clone(), row.body_html.clone()),
        Some("") => return (StatusCode::BAD_REQUEST, "body cannot be empty").into_response(),
        Some(t) if t.chars().count() > 20_000 => {
            return (StatusCode::BAD_REQUEST, "body too long (max 20000 chars)").into_response()
        }
        Some(t) => (t.to_string(), ai_email::text_to_html(t)),
    };
    // Persist edits BEFORE the send attempt: a failed send keeps the edited
    // draft, not the stale one.
    if body.subject.is_some() || body.body_text.is_some() {
        if let Err(e) = ai_email::update_draft(pool, row.id, &subject, &body_html, &body_text).await
        {
            tracing::error!("email edit persist failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    }

    // Resolve stored recipient refs to addresses — server-side only.
    let mut to_ids: Vec<Uuid> = Vec::new();
    let mut cc_ids: Vec<Uuid> = Vec::new();
    let mut to: Vec<String> = Vec::new();
    let mut cc: Vec<String> = Vec::new();
    for r in row.recipients.as_array().map(Vec::as_slice).unwrap_or(&[]) {
        let is_cc = r["cc"].as_bool().unwrap_or(false);
        if r["kind"] == "participant" {
            if let Some(uid) = r["user_id"].as_str().and_then(|s| Uuid::parse_str(s).ok()) {
                if is_cc {
                    cc_ids.push(uid);
                } else {
                    to_ids.push(uid);
                }
            }
        } else if let Some(e) = r["email"].as_str() {
            if is_cc {
                cc.push(e.to_string());
            } else {
                to.push(e.to_string());
            }
        }
    }
    let all_ids: Vec<Uuid> = to_ids.iter().chain(&cc_ids).copied().collect();
    if !all_ids.is_empty() {
        let rows: Vec<(Uuid, String)> =
            match sqlx::query_as("SELECT id, email FROM users WHERE id = ANY($1)")
                .bind(&all_ids)
                .fetch_all(pool)
                .await
            {
                Ok(rows) => rows,
                Err(e) => {
                    tracing::error!("recipient email lookup failed: {e}");
                    return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
                }
            };
        let lookup: std::collections::HashMap<Uuid, String> = rows.into_iter().collect();
        for uid in &to_ids {
            match lookup.get(uid) {
                Some(e) => to.push(e.clone()),
                None => tracing::warn!("email recipient {uid} no longer exists — skipped"),
            }
        }
        for uid in &cc_ids {
            match lookup.get(uid) {
                Some(e) => cc.push(e.clone()),
                None => tracing::warn!("email cc recipient {uid} no longer exists — skipped"),
            }
        }
    }
    if to.is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            "no deliverable recipients",
        )
            .into_response();
    }

    // Wrap the editable inner HTML/text in the shared branded shell (spec 0082)
    // only at send time — the stored draft keeps the clean inner body so re-edits
    // and previews never double-wrap.
    let lang = row.lang.as_deref().unwrap_or("en");
    let tagline = crate::email_template::tagline(lang);
    let html = crate::email_template::render_html(&crate::email_template::EmailLayout {
        app_base_url: &state.config.app_base_url,
        preheader: &subject,
        heading: None,
        body_html: &body_html,
        button: None,
        tagline,
    });
    let text =
        crate::email_template::render_text(&body_text, None, &state.config.app_base_url, tagline);
    let email = OutboundEmail {
        to,
        cc,
        subject,
        html,
        text,
    };
    match resend.send(&email).await {
        Ok(resend_id) => {
            // The mail is out either way — a failed status flip must not turn
            // a delivered email into a user-facing error (or a re-send).
            if let Err(e) = ai_email::mark_sent(pool, row.id, &resend_id).await {
                tracing::error!("mark_sent failed for {} AFTER delivery: {e}", row.id);
            }
            Json(serde_json::json!({
                "id": row.id,
                "status": "sent",
                "resend_id": resend_id,
                "sent_at": Utc::now(),
            }))
            .into_response()
        }
        Err(e) => {
            tracing::error!("resend send failed: {e}");
            (
                StatusCode::BAD_GATEWAY,
                "email send failed — the draft was kept",
            )
                .into_response()
        }
    }
}

/// `GET /api/sessions/{id}/email` — the requester's own latest draft/sent
/// email. Owner-scoped: drafts can hold raw addresses the requester typed.
pub async fn email_latest(
    State(state): State<AppState>,
    user: AuthUser,
    Path(session_id): Path<Uuid>,
) -> Response {
    let (Some(svc), Some(pool)) = (state.transcripts.as_ref(), state.pool.as_ref()) else {
        return service_unavailable();
    };
    if state.resend.is_none() {
        return (StatusCode::SERVICE_UNAVAILABLE, "email not configured").into_response();
    }
    if let Err(resp) = session_gate(svc, session_id, user.user_id).await {
        return resp;
    }
    match ai_email::latest_email(pool, session_id, user.user_id).await {
        Ok(Some(row)) => Json(email_json(&row, None)).into_response(),
        // 200 + null (not 404): polled on every session-detail open; avoids console spam.
        Ok(None) => Json(serde_json::Value::Null).into_response(),
        Err(e) => {
            tracing::error!("email load failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct InviteRequest {
    /// Recipient addresses the sender typed — people they already know. We never
    /// surface a directory, so addresses can only come from here.
    pub emails: Vec<String>,
    /// The sender's UI language, used to localise the email copy. Optional.
    #[serde(default)]
    pub lang: Option<String>,
}

/// `POST /api/rooms/{room}/invite` — email a one-tap join link to people the
/// sender knows. Auth-gated and rate-limited (anti-spam). One email per
/// recipient, so no recipient sees the others; the reply is a bare count and
/// never reveals whether an address maps to a registered account.
pub async fn invite_send(
    State(state): State<AppState>,
    user: AuthUser,
    Path(room): Path<String>,
    Json(body): Json<InviteRequest>,
) -> Response {
    // Bound how fast one account can fan out invites — protects the Resend
    // sending domain's reputation from being used as a spam relay.
    if !state.rate_limiter.allow(
        &format!("invite:{}", user.user_id),
        6,
        Duration::from_secs(60),
    ) {
        return (StatusCode::TOO_MANY_REQUESTS, "too many requests").into_response();
    }
    let Some(resend) = state.resend.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "email not configured").into_response();
    };

    let Some(room) = crate::invite::sanitize_room(&room) else {
        return (StatusCode::BAD_REQUEST, "invalid room code").into_response();
    };
    let recipients = match crate::invite::prepare_recipients(&body.emails) {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };

    // The link always points at OUR canonical origin + the sanitised room — a
    // client never supplies the URL we put our brand behind.
    let join_url = format!("{}/?room={room}", state.config.app_base_url);
    let lang = body.lang.as_deref().unwrap_or("en");
    let inviter: String = user.name.chars().take(60).collect();

    let invite = crate::invite::build_invite_email(lang, &inviter, &join_url);
    let html = crate::email_template::render_html(&crate::email_template::EmailLayout {
        app_base_url: &state.config.app_base_url,
        preheader: &invite.preheader,
        heading: Some(&invite.heading),
        body_html: &invite.body_html,
        button: None, // the button is embedded in body_html, ahead of the link
        tagline: &invite.tagline,
    });
    let text = crate::email_template::render_text(
        &invite.body_text,
        None,
        &state.config.app_base_url,
        &invite.tagline,
    );

    let mut sent = 0usize;
    let mut failed = 0usize;
    for addr in &recipients {
        let msg = OutboundEmail {
            to: vec![addr.clone()],
            cc: vec![],
            subject: invite.subject.clone(),
            html: html.clone(),
            text: text.clone(),
        };
        match resend.send(&msg).await {
            Ok(_) => sent += 1,
            Err(e) => {
                failed += 1;
                tracing::warn!("invite email to a recipient failed: {e}");
            }
        }
    }

    if sent == 0 {
        return (StatusCode::BAD_GATEWAY, "invite send failed").into_response();
    }
    Json(serde_json::json!({ "sent": sent, "failed": failed })).into_response()
}

/// `voxtranslate-{room_slug}-{id8}.{ext}` — the room slug is filtered to
/// `[A-Za-z0-9_-]` so user-chosen room names can't inject header syntax.
fn transcript_filename(room: &str, session_id: Uuid, ext: &str) -> String {
    let slug: String = room
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(40)
        .collect();
    let slug = if slug.is_empty() { "room".into() } else { slug };
    let id = session_id.to_string();
    let id8 = &id[..8];
    format!("voxtranslate-{slug}-{id8}.{ext}")
}

/// The ToS/Privacy version a consent is recorded against.
pub const CURRENT_TOS_VERSION: &str = "2026-06-10";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cf_turn_url_targets_the_key() {
        assert_eq!(
            cf_turn_credentials_url("abc123"),
            "https://rtc.live.cloudflare.com/v1/turn/keys/abc123/credentials/generate"
        );
    }

    #[test]
    fn soniox_key_request_uses_documented_usage_types_and_single_use() {
        // STT uses Soniox's documented enum value, NOT the `stt_rt` shorthand.
        let stt = soniox_key_request("transcribe_websocket", "user-1");
        assert_eq!(stt["usage_type"], "transcribe_websocket");
        assert_eq!(stt["single_use"], true);
        assert_eq!(stt["expires_in_seconds"], 3600);
        assert_eq!(stt["max_session_duration_seconds"], 7200);
        assert_eq!(stt["client_reference_id"], "user-1");
        let tts = soniox_key_request("tts_rt", "user-1");
        assert_eq!(tts["usage_type"], "tts_rt");
    }

    #[test]
    fn parse_soniox_key_reads_temp_key_or_errors() {
        let ok = serde_json::json!({
            "api_key": "temp:WYJ67RBEFUWQXXPKYPD2UGXKWB",
            "expires_at": "2025-02-22T22:47:37.150Z"
        });
        let (key, exp) = parse_soniox_key(&ok).unwrap();
        assert_eq!(key, "temp:WYJ67RBEFUWQXXPKYPD2UGXKWB");
        assert_eq!(exp, "2025-02-22T22:47:37.150Z");
        // Missing / empty key is an error (never hand the client a blank credential).
        assert!(parse_soniox_key(&serde_json::json!({ "expires_at": "x" })).is_err());
        assert!(parse_soniox_key(&serde_json::json!({ "api_key": "" })).is_err());
    }

    #[test]
    fn soniox_region_labels() {
        use crate::config::SonioxRegion;
        assert_eq!(soniox_region_label(SonioxRegion::Us), "US");
        assert_eq!(soniox_region_label(SonioxRegion::Eu), "EU");
        assert_eq!(soniox_region_label(SonioxRegion::Jp), "JP");
    }

    #[test]
    fn parse_cf_ice_extracts_iceservers_else_none() {
        let ok = serde_json::json!({
            "iceServers": {
                "urls": ["turn:turn.cloudflare.com:3478?transport=udp"],
                "username": "u",
                "credential": "c"
            }
        });
        let got = parse_cf_ice_servers(&ok).expect("well-formed response yields iceServers");
        assert_eq!(got["username"], "u");
        assert_eq!(got["credential"], "c");
        // Missing credential / wrong shape ⇒ None (caller falls back to STUN-only).
        assert!(
            parse_cf_ice_servers(&serde_json::json!({ "iceServers": { "urls": [] } })).is_none()
        );
        assert!(parse_cf_ice_servers(&serde_json::json!({ "error": "bad token" })).is_none());
    }

    #[test]
    fn transcript_filename_sanitizes_room_names() {
        let sid = Uuid::parse_str("a1b2c3d4-0000-0000-0000-000000000000").unwrap();
        assert_eq!(
            transcript_filename("my-room_1", sid, "json"),
            "voxtranslate-my-room_1-a1b2c3d4.json"
        );
        // Header-injection / quote-breaking characters are stripped.
        assert_eq!(
            transcript_filename("evil\"; rm -rf /\r\nX: y", sid, "pdf"),
            "voxtranslate-evilrm-rfXy-a1b2c3d4.pdf"
        );
        // Nothing survivable -> generic slug; long names truncated to 40 chars.
        assert_eq!(
            transcript_filename("🎉🎉🎉", sid, "json"),
            "voxtranslate-room-a1b2c3d4.json"
        );
        let long = "x".repeat(80);
        assert_eq!(
            transcript_filename(&long, sid, "json"),
            format!("voxtranslate-{}-a1b2c3d4.json", "x".repeat(40))
        );
    }
}

#[derive(Deserialize)]
pub struct ReportRequest {
    pub room: String,
    #[serde(default)]
    pub reported_peer_id: Option<String>,
    #[serde(default)]
    pub reported_name: Option<String>,
    pub reason: String,
    #[serde(default)]
    pub transcript_excerpt: Option<String>,
}

/// `POST /api/report` — file an abuse report against a peer.
pub async fn report(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<ReportRequest>,
) -> Response {
    let Some(safety) = state.safety.as_ref() else {
        return service_unavailable();
    };
    if body.reason.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "missing reason").into_response();
    }
    // Truncate the excerpt defensively.
    let excerpt = body
        .transcript_excerpt
        .as_deref()
        .map(|s| s.chars().take(500).collect::<String>());
    match safety
        .record_report(
            user.user_id,
            &body.room,
            body.reported_peer_id.as_deref(),
            body.reported_name.as_deref(),
            &body.reason,
            excerpt.as_deref(),
        )
        .await
    {
        Ok(()) => (StatusCode::CREATED, "reported").into_response(),
        Err(e) => {
            tracing::error!("report failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "report failed").into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct ConsentRequest {
    pub age_confirmed: bool,
}

/// `POST /api/user/consent` — record the user is 18+ and accepts the ToS/Privacy.
pub async fn submit_consent(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<ConsentRequest>,
) -> Response {
    let Some(safety) = state.safety.as_ref() else {
        return service_unavailable();
    };
    if !body.age_confirmed {
        return (StatusCode::FORBIDDEN, "must be 18+ to use this service").into_response();
    }
    match safety.set_consent(user.user_id, CURRENT_TOS_VERSION).await {
        Ok(()) => Json(serde_json::json!({ "consent_given": true })).into_response(),
        Err(e) => {
            tracing::error!("consent failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "consent failed").into_response()
        }
    }
}

/// `GET /api/user/data` — export everything we hold on the user (GDPR).
pub async fn export_data(State(state): State<AppState>, user: AuthUser) -> Response {
    let Some(safety) = state.safety.as_ref() else {
        return service_unavailable();
    };
    match safety.export_user_data(user.user_id).await {
        Ok(data) => Json(data).into_response(),
        Err(e) => {
            tracing::error!("export failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "export failed").into_response()
        }
    }
}

/// `DELETE /api/user` — erase the account and all linked data (GDPR).
pub async fn delete_account(State(state): State<AppState>, user: AuthUser) -> Response {
    let Some(safety) = state.safety.as_ref() else {
        return service_unavailable();
    };
    match safety.delete_user(user.user_id).await {
        Ok(()) => (StatusCode::OK, "deleted").into_response(),
        Err(e) => {
            tracing::error!("delete failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "delete failed").into_response()
        }
    }
}
