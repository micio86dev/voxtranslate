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

use crate::ai::email_draft as ai_email;
use crate::ai::report as ai_report;
use crate::ai::sentiment as ai_sentiment;
use crate::billing::{usd, BillingError};
use crate::email::OutboundEmail;
use crate::glossary::{import_csv, normalize_entries, NewEntry, RoomGlossary};
use crate::middleware::AuthUser;
use crate::protocol::ServerMessage;
use crate::stripe_handler;
use crate::transcripts::{BookmarkMutation, SessionAccess, TranscriptService};
use crate::AppState;

/// `GET /api/billing/packages` — the credit catalog (without `stripe_price_id`).
pub async fn billing_packages(State(state): State<AppState>) -> Response {
    match state.config.billing.as_ref() {
        Some(cfg) => Json(&cfg.pricing.packages).into_response(),
        None => service_unavailable(),
    }
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

/// Shared 402 responder for credit-charged AI features. The pre-check is
/// advisory (the atomic `deduct_feature` is the real gate) but lets the client
/// show "need X, have Y" before any AI work runs.
pub fn insufficient_credits(feature: &str, required: Decimal, available: Decimal) -> Response {
    (
        StatusCode::PAYMENT_REQUIRED,
        Json(serde_json::json!({
            "error": "insufficient_credits",
            "required": required.to_f64().unwrap_or(0.0),
            "available": available.to_f64().unwrap_or(0.0),
            "feature": feature,
        })),
    )
        .into_response()
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

/// `GET /api/sessions/{id}/transcript.json` — download the transcript as
/// pretty-printed JSON. Participants only (404 unknown / 403 stranger).
pub async fn transcript_json(
    State(state): State<AppState>,
    user: AuthUser,
    Path(session_id): Path<Uuid>,
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

    let export = match svc.export(session_id).await {
        Ok(Some(doc)) => doc,
        // Purged between the access check and here (guest-only finalize race).
        Ok(None) => return (StatusCode::NOT_FOUND, "no such session").into_response(),
        Err(e) => {
            tracing::error!("transcript export failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };

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

    let export = match svc.export(session_id).await {
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

    let export = match svc.export(session_id).await {
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

/// `GET /api/rooms/{room}/glossary` — the room's glossary (empty when none).
/// No auth required: the room code is the access control (anyone who can type
/// the code is already in the room).
pub async fn glossary_get(State(state): State<AppState>, Path(room): Path<String>) -> Response {
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
    _user: AuthUser,
    Path(room): Path<String>,
) -> Response {
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

    let (markdown, model) = match ai_report::generate_report(
        &state.groq,
        ai,
        &export,
        format,
        &lang,
        guidelines.as_deref(),
    )
    .await
    {
        Ok(out) => out,
        Err(e) => {
            tracing::error!("report generation failed: {e}");
            return (
                StatusCode::BAD_GATEWAY,
                "report generation failed — you were not charged",
            )
                .into_response();
        }
    };

    let balance = match billing
        .deduct_feature(
            user.user_id,
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
            let available = billing
                .get_balance(user.user_id)
                .await
                .unwrap_or(Decimal::ZERO);
            return insufficient_credits("ai_report", cost, available);
        }
        Err(e) => {
            tracing::error!("ai_report deduction failed AFTER generation — delivering free: {e}");
            None
        }
    };

    match ai_report::save_report(
        pool,
        session_id,
        user.user_id,
        format,
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
            (StatusCode::CREATED, Json(v)).into_response()
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
            (StatusCode::CREATED, Json(v)).into_response()
        }
    }
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
        Ok(None) => (StatusCode::NOT_FOUND, "no report yet").into_response(),
        Err(e) => {
            tracing::error!("report load failed: {e}");
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

    let (result, model) = match ai_sentiment::analyze(&state.groq, ai, &export).await {
        Ok(out) => out,
        Err(e) => {
            tracing::error!("sentiment analysis failed: {e}");
            return (
                StatusCode::BAD_GATEWAY,
                "sentiment analysis failed — you were not charged",
            )
                .into_response();
        }
    };

    let balance = match billing
        .deduct_feature(
            user.user_id,
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
            let available = billing
                .get_balance(user.user_id)
                .await
                .unwrap_or(Decimal::ZERO);
            return insufficient_credits("ai_sentiment", cost, available);
        }
        // Any other failure is ours, not the user's: deliver free and log.
        Err(e) => {
            tracing::error!("ai_sentiment deduction failed AFTER analysis — delivering free: {e}");
            None
        }
    };

    match ai_sentiment::save_sentiment(pool, session_id, user.user_id, &result, &model, cost).await
    {
        Ok(Some(row)) => {
            let mut v = sentiment_json(&row, false);
            if let Some(b) = balance {
                v["balance"] = serde_json::json!(b.to_f64().unwrap_or(0.0));
            }
            (StatusCode::CREATED, Json(v)).into_response()
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
            (StatusCode::CREATED, Json(v)).into_response()
        }
    }
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
        Ok(None) => (StatusCode::NOT_FOUND, "no sentiment analysis yet").into_response(),
        Err(e) => {
            tracing::error!("sentiment load failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
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

    // Reuse the stored report's executive summary when available — better
    // grounding for free (no extra model call).
    let summary = if body.include_summary.unwrap_or(true) {
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
            return (
                StatusCode::BAD_GATEWAY,
                "email draft failed — you were not charged",
            )
                .into_response();
        }
    };

    let balance = match billing
        .deduct_feature(
            user.user_id,
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
            let available = billing
                .get_balance(user.user_id)
                .await
                .unwrap_or(Decimal::ZERO);
            return insufficient_credits("ai_email", cost, available);
        }
        // Any other failure is ours, not the user's: deliver free and log.
        Err(e) => {
            tracing::error!("ai_email deduction failed AFTER generation — delivering free: {e}");
            None
        }
    };

    match ai_email::save_email(
        pool,
        session_id,
        user.user_id,
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
            (StatusCode::CREATED, Json(v)).into_response()
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
            (StatusCode::CREATED, Json(v)).into_response()
        }
    }
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

    let email = OutboundEmail {
        to,
        cc,
        subject,
        html: body_html,
        text: body_text,
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
        Ok(None) => (StatusCode::NOT_FOUND, "no email draft yet").into_response(),
        Err(e) => {
            tracing::error!("email load failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
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
