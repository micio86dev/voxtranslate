//! HTTP surface for translated telephone calls (spec 0111).
//!
//! Registered only when VoIP is enabled and a provider could be built, so turning the
//! feature off is a real kill switch: the routes are absent and a request 404s, rather
//! than reaching a handler that has to remember to check a flag.
//!
//! Authorization follows the Business API convention exactly — `require_role` on the org
//! in the path, never on an org id taken from a body. Non-admin members see only the
//! calls they placed (R28).

// Every handler returns `Result<Response, Response>` — the Business API convention, and
// what `business/mod.rs` allows for the same reason.
#![allow(clippy::result_large_err)]

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::FromRow;
use uuid::Uuid;

use crate::business::{db_err, not_found, require_pool, require_role, ADMIN, MEMBER};
use crate::middleware::AuthUser;
use crate::telephony::{WebhookHeaders, E164};
use crate::voip::service::{self, DialOptions, VoipError};
use crate::voip::{consent, contacts, numbers as number_mgmt, webhook};
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/business/organizations/{org_id}/voip/quote",
            post(quote),
        )
        .route(
            "/api/business/organizations/{org_id}/voip/calls",
            post(dial).get(history),
        )
        .route(
            "/api/business/organizations/{org_id}/voip/calls/{call_id}",
            get(detail),
        )
        .route(
            "/api/business/organizations/{org_id}/voip/calls/{call_id}/answer",
            post(answer_inbound),
        )
        .route(
            "/api/business/organizations/{org_id}/voip/calls/{call_id}/hangup",
            post(hangup),
        )
        .route(
            "/api/business/organizations/{org_id}/voip/calls/{call_id}/video-invite",
            post(video_invite),
        )
        .route(
            "/api/business/organizations/{org_id}/voip/settings",
            get(get_settings).put(put_settings),
        )
        .route(
            "/api/business/organizations/{org_id}/voip/numbers",
            get(numbers).post(number_mgmt::buy),
        )
        // The address book (spec 0114). `lookup` is registered BEFORE the `{contact_id}`
        // route, or axum reads "lookup" as an id and answers 400 for a path that exists.
        // Buying, verifying and keeping numbers (spec 0115). `search` is registered before
        // `{number_id}` for the same reason `lookup` is: otherwise axum reads the word as
        // an id.
        .route(
            "/api/business/organizations/{org_id}/voip/numbers/search",
            get(number_mgmt::search),
        )
        .route(
            "/api/business/organizations/{org_id}/voip/numbers/{number_id}",
            axum::routing::delete(number_mgmt::release),
        )
        .route(
            "/api/business/organizations/{org_id}/voip/numbers/{number_id}/routing",
            get(number_mgmt::get_routing).put(number_mgmt::put_routing),
        )
        .route(
            "/api/business/organizations/{org_id}/voip/numbers/{number_id}/verify",
            post(number_mgmt::verify),
        )
        .route(
            "/api/business/organizations/{org_id}/voip/numbers/{number_id}/verify/check",
            post(number_mgmt::verify_check),
        )
        .route(
            "/api/business/organizations/{org_id}/voip/contacts",
            get(contacts::list).post(contacts::create),
        )
        .route(
            "/api/business/organizations/{org_id}/voip/contacts/lookup",
            get(contacts::lookup),
        )
        .route(
            "/api/business/organizations/{org_id}/voip/contacts/{contact_id}",
            get(contacts::detail)
                .patch(contacts::update)
                .delete(contacts::remove),
        )
        // Unauthenticated by design: the provider cannot present a session. The Ed25519
        // signature IS the authentication, and it is verified before anything is read.
        .route("/api/voip/webhooks/{provider}", post(inbound_webhook))
        // Unauthenticated because the person redeeming it has no account and is not meant
        // to need one — a telephone recipient invited into a browser room. The signed,
        // short-lived ticket in the path is the whole of the authorisation.
        .route("/api/voip/video/{ticket}", get(redeem_video_invite))
        // Also unauthenticated by design, and for the same reason: the connection comes
        // from the carrier's media plane, which carries no session of ours. The ticket in
        // the path IS the authentication — signed by us, single-use, valid for sixty
        // seconds, and bound to one call, one leg and one peer.
        .route("/voip/media/{ticket}", get(media_socket))
}

/// A refusal carrying a **stable machine-readable code**, never prose.
///
/// `err_response` below already states the rule and the reason: a policy refusal's prose
/// would be untranslatable, so only the code crosses the boundary. The handlers in this
/// file used the shared `bad_request`/`forbidden` helpers instead, which emit raw English
/// as `text/plain` — untranslatable by definition, and unparseable by the dashboard, which
/// reads `{ error }` and therefore rendered every one of them as the generic message.
///
/// Codes added here must also gain copy in the dashboard's five locales; the client keeps
/// its list in `phone-dialer.ts`'s `KNOWN_REASONS`, so an unrecognised code degrades to
/// the generic string rather than printing itself at a customer.
pub(crate) fn refuse(status: StatusCode, code: &str) -> Response {
    (status, Json(json!({ "error": code }))).into_response()
}

/// `require_project` and project tenancy, checked identically wherever a call is priced
/// or placed.
///
/// Extracted because `quote` promised "the same gate as dial" and did not run this one:
/// an organisation with `require_project` got a cheerful price and then a refusal.
async fn check_project(
    pool: &crate::db::Pool,
    org_id: Uuid,
    require_project: bool,
    project_id: Option<Uuid>,
) -> Result<(), Response> {
    if require_project && project_id.is_none() {
        return Err(refuse(StatusCode::BAD_REQUEST, "project_required"));
    }
    if let Some(project_id) = project_id {
        let ok: Option<bool> =
            sqlx::query_scalar("SELECT true FROM projects WHERE id = $1 AND org_id = $2")
                .bind(project_id)
                .bind(org_id)
                .fetch_optional(pool)
                .await
                .map_err(db_err)?;
        if ok.is_none() {
            return Err(refuse(StatusCode::BAD_REQUEST, "project_not_in_org"));
        }
    }
    Ok(())
}

fn err_response(e: VoipError) -> Response {
    if matches!(e, VoipError::Refused(_)) {
        // Counted apart from `failed`: a rising refusal rate is usually a customer hitting
        // a limit they set, while a rising failure rate is usually us or the carrier.
        crate::metrics::record_voip_refused();
    }
    let status = match &e {
        VoipError::BadNumber(_) => StatusCode::BAD_REQUEST,
        VoipError::Refused(_) => StatusCode::PAYMENT_REQUIRED,
        VoipError::Misconfigured(_) => StatusCode::SERVICE_UNAVAILABLE,
        VoipError::Storage => StatusCode::INTERNAL_SERVER_ERROR,
    };
    // Only the stable machine-readable code crosses the boundary. A database error's text
    // is not an API payload, and a policy refusal's prose would be untranslatable.
    (status, Json(json!({ "error": e.code() }))).into_response()
}

/// The provider, or a 404 that matches "the feature is off".
fn provider(state: &AppState) -> Result<&dyn crate::telephony::TelephonyProvider, Response> {
    state
        .telephony
        .as_deref()
        .ok_or_else(|| not_found("voip is not enabled"))
}

fn cfg(state: &AppState) -> Result<&crate::config::VoipConfig, Response> {
    state
        .config
        .voip
        .as_ref()
        .ok_or_else(|| not_found("voip is not enabled"))
}

#[derive(Debug, Deserialize)]
pub struct QuoteBody {
    destination: String,
    #[serde(default)]
    engine_id: Option<String>,
    #[serde(default)]
    target_language: Option<String>,
    // A gate cannot check inputs it was not given. These two are what made `quote`'s
    // promise of "the same gate as dial" untrue: without them it could not run
    // `require_project` or resolve a caller id, so both refusals waited until the
    // customer had read a price and pressed Call.
    #[serde(default)]
    project_id: Option<Uuid>,
    #[serde(default)]
    caller_id: Option<String>,
    #[serde(default)]
    record: bool,
    #[serde(default)]
    transcribe: bool,
    #[serde(default)]
    ai_analysis: bool,
    #[serde(default)]
    estimated_minutes: Option<i32>,
}

/// `POST …/voip/quote` — price a call without placing it.
///
/// Runs the **same** gate as [`dial`], so the dialer cannot show a price for a call that
/// would then be refused. A quote that is cheerful about a call the policy forbids is
/// worse than no quote.
///
/// That means all three of them, not just the policy gate: `policy::check` via
/// `check_and_quote`, `check_project`, and caller-id resolution. The last two were
/// dial-only until spec 0112, which made this comment a claim the code did not keep.
pub async fn quote(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Json(body): Json<QuoteBody>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;
    let cfg = cfg(&state)?;
    let provider = provider(&state)?;

    let dest = E164::parse(&body.destination).map_err(|e| err_response(VoipError::BadNumber(e)))?;

    let world = service::load_world(pool, cfg, org_id, user.user_id, &dest)
        .await
        .map_err(db_err)?;

    let engine_id = body
        .engine_id
        .or_else(|| world.org.default_engine_id.clone())
        .unwrap_or_else(|| state.engines.default().metadata().id.clone());
    let engine = state.engines.resolve(Some(&engine_id));

    let opts = DialOptions {
        destination: body.destination.clone(),
        source_language: String::new(),
        target_language: body.target_language.clone().unwrap_or_default(),
        engine_id: engine.metadata().id.clone(),
        project_id: None,
        caller_id: None,
        record: body.record,
        transcribe: body.transcribe,
        ai_analysis: body.ai_analysis,
        estimated_minutes: body.estimated_minutes.unwrap_or(10),
    };
    let intent = service::capture_intent(cfg, &world.org, &opts);
    let minutes = service::hold_minutes(opts.estimated_minutes, &world.org, cfg);

    let subscription_active = crate::business::credits::org_subscription_active(pool, org_id)
        .await
        .map_err(db_err)?;

    let q = service::check_and_quote(
        cfg,
        &world,
        &dest,
        engine.metadata(),
        intent,
        subscription_active,
        cfg.beta_org_ids.iter().any(|id| id == &org_id.to_string()),
        // No tier can guarantee EU-only processing today; see docs/voip-data-flow.md.
        // Sourced here rather than hardcoded deeper so it flips in one place.
        false,
        provider.metadata().eu_telephony,
        minutes,
    )
    .map_err(err_response)?;

    // AFTER the policy gate, never before it. `policy::check` puts entitlement first so a
    // prober learns their organisation is not entitled before they learn anything else;
    // running these ahead of it would tell an unentitled caller which of our settings they
    // had got wrong. Proven by `an_org_without_a_live_subscription_still_cannot_dial`.
    check_project(pool, org_id, world.org.require_project, body.project_id).await?;

    // Only a caller id the requester actually NAMED. Presenting a number you cannot prove
    // you own is the refusal with a regulator behind it, and quoting it as fine and then
    // refusing the dial is exactly the mismatch this gate exists to remove.
    //
    // Deliberately NOT the "this org owns no number yet" case: that is a setup state, and
    // refusing to show a price to a paying customer who has not bought a number is hostile
    // — see `a_subscribed_org_can_quote_without_anyone_writing_a_settings_row`. `dial`
    // still refuses it, at the point where it actually matters.
    if body.caller_id.is_some() {
        resolve_caller_id(
            pool,
            org_id,
            body.caller_id.as_deref(),
            provider.metadata().default_caller_id.as_deref(),
        )
        .await?;
    }

    let balance: i32 =
        sqlx::query_scalar("SELECT credits_balance FROM organizations WHERE id = $1")
            .bind(org_id)
            .fetch_one(pool)
            .await
            .map_err(db_err)?;

    let disclosure = consent::plan(
        world.org.consent_policy,
        intent,
        true,
        &opts.target_language,
    );

    Ok(Json(json!({
        // The masked form, never the full number: this payload reaches a browser console
        // and, from there, any log the customer's own tooling keeps.
        "destination": dest.masked(),
        "country": dest.region(),
        "price_per_minute": q.price_per_minute,
        "currency": "USD",
        "reserve_credits": q.reserve_credits,
        "estimated_minutes": q.estimated_minutes,
        "balance_credits": balance,
        "engine_id": engine.metadata().id,
        "recording": intent.recording,
        "transcription": intent.transcription,
        "consent_policy": world.org.consent_policy.as_str(),
        "disclosure_language": disclosure.body.map(|_| disclosure.language.clone()),
    }))
    .into_response())
}

#[derive(Debug, Deserialize)]
pub struct DialBody {
    destination: String,
    source_language: String,
    target_language: String,
    #[serde(default)]
    engine_id: Option<String>,
    #[serde(default)]
    project_id: Option<Uuid>,
    #[serde(default)]
    caller_id: Option<String>,
    #[serde(default)]
    record: bool,
    #[serde(default)]
    transcribe: bool,
    #[serde(default)]
    ai_analysis: bool,
    #[serde(default)]
    estimated_minutes: Option<i32>,
}

/// `POST …/voip/calls` — place a call.
pub async fn dial(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Json(body): Json<DialBody>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;
    let cfg = cfg(&state)?;
    let provider = provider(&state)?;

    let dest = E164::parse(&body.destination).map_err(|e| err_response(VoipError::BadNumber(e)))?;

    let world = service::load_world(pool, cfg, org_id, user.user_id, &dest)
        .await
        .map_err(db_err)?;

    check_project(pool, org_id, world.org.require_project, body.project_id).await?;

    let engine_id = body
        .engine_id
        .clone()
        .or_else(|| world.org.default_engine_id.clone())
        .unwrap_or_else(|| state.engines.default().metadata().id.clone());
    let engine = state.engines.resolve(Some(&engine_id));

    // Caller id must be a number this org owns and that the provider has verified.
    // Anything else would be caller-id spoofing, which is illegal in most of our markets
    // — so it is resolved from the database, never taken from the request body as-is.
    let caller_id = resolve_caller_id(
        pool,
        org_id,
        body.caller_id.as_deref(),
        provider.metadata().default_caller_id.as_deref(),
    )
    .await?;

    let opts = DialOptions {
        destination: body.destination.clone(),
        source_language: body.source_language.clone(),
        target_language: body.target_language.clone(),
        engine_id: engine.metadata().id.clone(),
        project_id: body.project_id,
        caller_id: Some(caller_id.as_str().to_string()),
        record: body.record,
        transcribe: body.transcribe,
        ai_analysis: body.ai_analysis,
        estimated_minutes: body.estimated_minutes.unwrap_or(10),
    };
    let intent = service::capture_intent(cfg, &world.org, &opts);
    let minutes = service::hold_minutes(opts.estimated_minutes, &world.org, cfg);

    let subscription_active = crate::business::credits::org_subscription_active(pool, org_id)
        .await
        .map_err(db_err)?;

    let q = service::check_and_quote(
        cfg,
        &world,
        &dest,
        engine.metadata(),
        intent,
        subscription_active,
        cfg.beta_org_ids.iter().any(|id| id == &org_id.to_string()),
        false,
        provider.metadata().eu_telephony,
        minutes,
    )
    .map_err(err_response)?;

    let created = service::dial(
        pool,
        provider,
        cfg,
        org_id,
        user.user_id,
        &dest,
        &caller_id,
        &opts,
        &world,
        &q,
        intent,
        &dest.pseudonym(&cfg.pseudonym_key),
        &state.rooms,
        &state.voip_calls,
    )
    .await
    .map_err(err_response)?;

    crate::business::audit::log_audit_event(
        pool,
        org_id,
        user.user_id,
        "voip.call.started",
        "voip_call",
        created.call_id,
        // The pseudonym, never the number: audit rows are exported and read widely.
        json!({
            "country": dest.region(),
            "recipient": dest.pseudonym(&cfg.pseudonym_key),
            "engine_id": opts.engine_id,
            "recording": intent.recording,
            "transcription": intent.transcription,
            "reserved_credits": created.reserved_credits,
        }),
    );

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "call_id": created.call_id,
            "session_id": created.session_id,
            // The room to join. A phone call is a room with a telephone in it; the caller
            // is an ordinary browser peer and joins it the ordinary way.
            "room": created.room,
            "status": created.status.as_str(),
            "reserved_credits": created.reserved_credits,
            "price_per_minute": created.price_per_minute,
        })),
    )
        .into_response())
}

/// Resolve the presented caller id from numbers the org actually owns.
async fn resolve_caller_id(
    pool: &crate::db::Pool,
    org_id: Uuid,
    requested: Option<&str>,
    provider_default: Option<&str>,
) -> Result<E164, Response> {
    let row: Option<String> = match requested {
        Some(want) => sqlx::query_scalar(
            "SELECT e164 FROM voip_numbers
             WHERE org_id = $1 AND e164 = $2 AND outbound_enabled
               AND verification_status = 'verified'",
        )
        .bind(org_id)
        .bind(want)
        .fetch_optional(pool)
        .await
        .map_err(db_err)?,
        None => sqlx::query_scalar(
            "SELECT e164 FROM voip_numbers
             WHERE org_id = $1 AND outbound_enabled AND verification_status = 'verified'
             ORDER BY is_default DESC, created_at
             LIMIT 1",
        )
        .bind(org_id)
        .fetch_optional(pool)
        .await
        .map_err(db_err)?,
    };

    // Falling back to the deployment's own number is deliberate and narrow: it belongs to
    // the operator, so it cannot impersonate anyone. A number the CALLER named that we
    // cannot verify is refused outright — arbitrary caller-id spoofing is illegal in most
    // of our markets.
    //
    // The fallback comes from the PROVIDER's metadata rather than from a provider-specific
    // environment variable: `TELNYX_DEFAULT_CALLER_ID` is a Telnyx name, and provider names
    // stop at the `telephony::` boundary.
    let raw = match (row, requested) {
        (Some(n), _) => n,
        (None, Some(_)) => {
            return Err(refuse(StatusCode::FORBIDDEN, "caller_id_unverified"));
        }
        (None, None) => provider_default
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| refuse(StatusCode::BAD_REQUEST, "caller_id_missing"))?,
    };

    E164::parse(&raw).map_err(|e| err_response(VoipError::BadNumber(e)))
}

#[derive(Debug, Default, Deserialize)]
pub struct HistoryQuery {
    project_id: Option<Uuid>,
    page: Option<i64>,
    limit: Option<i64>,
}

#[derive(Debug, Serialize, FromRow)]
struct CallRow {
    id: Uuid,
    status: String,
    failure_reason: Option<String>,
    direction: String,
    recipient_country: String,
    /// Display form only. The full number is deliberately NOT in the list payload — a
    /// history page is the most-exported view in the product.
    recipient_masked: String,
    source_language: String,
    target_language: String,
    engine_id: String,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    duration_seconds: Option<i32>,
    credits_consumed: i32,
    recording_status: String,
    transcription_status: String,
    consent_status: String,
    project_id: Option<Uuid>,
    project_name: Option<String>,
}

/// `GET …/voip/calls` — paginated history. Non-admins see only their own calls (R28).
pub async fn history(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Query(q): Query<HistoryQuery>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let role = require_role(pool, org_id, user.user_id, MEMBER).await?;
    let is_admin = matches!(role.as_str(), "admin" | "owner");

    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let page = q.page.unwrap_or(1).max(1);

    let rows: Vec<CallRow> = sqlx::query_as(
        "SELECT c.id, c.status, c.failure_reason, c.direction, c.recipient_country,
                '+' || substring(ltrim(c.recipient_e164, '+') from 1 for 2) || '••••' ||
                    right(c.recipient_e164, 4) AS recipient_masked,
                c.source_language, c.target_language, c.engine_id, c.started_at,
                c.ended_at, c.duration_seconds, c.credits_consumed, c.recording_status,
                c.transcription_status, c.consent_status, c.project_id,
                p.name AS project_name
         FROM voip_calls c
         LEFT JOIN projects p ON p.id = c.project_id
         WHERE c.org_id = $1
           AND ($2::uuid IS NULL OR c.project_id = $2)
           AND ($3 OR c.user_id = $4)
         ORDER BY c.started_at DESC
         LIMIT $5 OFFSET $6",
    )
    .bind(org_id)
    .bind(q.project_id)
    .bind(is_admin)
    .bind(user.user_id)
    .bind(limit)
    .bind((page - 1) * limit)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;

    Ok(Json(json!({ "calls": rows, "page": page, "limit": limit })).into_response())
}

#[derive(Debug, Serialize, FromRow)]
struct CallDetailRow {
    id: Uuid,
    session_id: Uuid,
    /// So a caller who reloaded the page can get back into a call that is still live.
    room: Option<String>,
    status: String,
    failure_reason: Option<String>,
    direction: String,
    /// The full number. Returned by this endpoint only.
    recipient_e164: String,
    recipient_country: String,
    source_language: String,
    target_language: String,
    engine_id: String,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    duration_seconds: Option<i32>,
    credits_consumed: i32,
    quoted_price_per_min: Option<Decimal>,
    /// `"pending"` until the provider rates the leg, then `"final"`.
    ///
    /// Deliberately a **status and not a number**. What the leg cost us
    /// (`actual_provider_cost_usd`) and the margin we made on it (`gross_margin`) are
    /// business-internal and stay on the server — the same rule `engine/metadata.rs`
    /// enforces for the engine catalogue with `engine_info_never_leaks_cost_or_markup`.
    /// What the customer agreed to (`quoted_price_per_min`) and what they paid
    /// (`credits_consumed`) are theirs, and both are still here. Spec 0112 R6.
    cost_status: String,
    recording_status: String,
    transcription_status: String,
    consent_status: String,
    project_id: Option<Uuid>,
    /// Who was called, when the address book knew (spec 0114). Null after the contact is
    /// deleted — the call outlives the convenience data that described it.
    contact_id: Option<Uuid>,
    contact_name: Option<String>,
}

/// `GET …/voip/calls/{id}` — one call, with the money detail.
pub async fn detail(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, call_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let role = require_role(pool, org_id, user.user_id, MEMBER).await?;
    let is_admin = matches!(role.as_str(), "admin" | "owner");

    // `org_id` in the WHERE clause is the tenancy boundary: a call id from another org
    // returns 404, not the row.
    let row: Option<CallDetailRow> = sqlx::query_as(
        // `room` only while the call is live: it is a join handle, and handing one out
        // for a call that is over invites someone into a room nobody is in.
        "SELECT c.id, c.session_id,
                CASE WHEN c.status NOT IN ('completed', 'failed') THEN s.room END AS room,
                c.status, c.failure_reason, c.direction, c.recipient_e164,
                c.recipient_country, c.source_language, c.target_language, c.engine_id,
                c.started_at, c.ended_at, c.duration_seconds, c.credits_consumed,
                c.quoted_price_per_min,
                CASE WHEN c.actual_provider_cost_usd IS NULL THEN 'pending' ELSE 'final' END
                    AS cost_status,
                c.recording_status, c.transcription_status, c.consent_status, c.project_id,
                c.contact_id, ct.name AS contact_name
         FROM voip_calls c
         JOIN call_sessions s ON s.id = c.session_id
         LEFT JOIN voip_contacts ct ON ct.id = c.contact_id
         WHERE c.id = $1 AND c.org_id = $2 AND ($3 OR c.user_id = $4)",
    )
    .bind(call_id)
    .bind(org_id)
    .bind(is_admin)
    .bind(user.user_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;

    let Some(r) = row else {
        return Err(not_found("call not found"));
    };

    // The full number IS returned here, and only here: the detail view is the record the
    // customer paid for, and a sales rep has to be able to see who they called. The list
    // view and every log carry the masked or pseudonymous form instead.
    //
    // Our provider cost and our margin are NOT returned, to anyone, at any role — see
    // `CallDetailRow::cost_status`. Reconciliation is reported as `pending` rather than as
    // a zero, because a zero reads as "this call was free", which is a very different
    // claim from "the provider has not rated it yet". See docs/voip-telnyx-setup.md §7.
    Ok(Json(r).into_response())
}

#[derive(Debug, Serialize, FromRow)]
struct NumberRow {
    id: Uuid,
    e164: String,
    country: String,
    label: Option<String>,
    is_default: bool,
    inbound_enabled: bool,
    outbound_enabled: bool,
    verification_status: String,
    /// The lifecycle (spec 0115). `pending_regulatory` is its own state because saying
    /// "active" while a regulator is the blocker is a lie with a fine attached.
    status: String,
    status_reason: Option<String>,
    regulatory_requirement: Option<String>,
    /// What the customer pays each month — the price recorded when the number was bought,
    /// never today's. Our own cost and the markup stay on the server.
    customer_monthly_usd: Option<Decimal>,
    next_renewal_at: Option<DateTime<Utc>>,
}

/// `GET …/voip/numbers` — the organisation's own telephone numbers (spec 0112 R3).
///
/// Read-only. Searching, buying, verifying and releasing numbers are spec 0115; this
/// exists because without it the dialer's caller-id select had exactly one hardcoded
/// option and could not name a number the organisation actually owns.
///
/// Every row is returned, including the ones that may not be presented as caller id yet,
/// each carrying the state that says so. Filtering them out here would hide a number stuck
/// in `pending` from the admin who needs to chase it. Which rows are *usable* is decided
/// by `resolve_caller_id` on the way out, and mirrored in the client as a pure function —
/// this endpoint reports, it does not adjudicate.
///
/// The numbers are returned in full. R23's masking rule is about the **recipient's**
/// number in logs and lists; these belong to the organisation asking for them, and a
/// caller id you cannot read is not a caller id you can choose.
pub async fn numbers(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;

    let rows: Vec<NumberRow> = sqlx::query_as(
        "SELECT id, e164, country, label, is_default, inbound_enabled, outbound_enabled,
                verification_status, status, status_reason, regulatory_requirement,
                customer_monthly_usd, next_renewal_at
         FROM voip_numbers
         WHERE org_id = $1
         ORDER BY is_default DESC, created_at",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;

    Ok(Json(json!({ "numbers": rows })).into_response())
}

/// `POST …/voip/calls/{id}/answer` — I am taking this call (spec 0116 R3).
///
/// Records WHO picked up, which is what makes a missed call a fact rather than an
/// inference: `answered_by IS NULL` past the ring deadline is the definition the sweep
/// uses. First to claim it wins — the `answered_by IS NULL` in the WHERE is the race
/// guard, so two people pressing at once cannot both be the answerer.
pub async fn answer_inbound(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, call_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;

    let room: Option<Option<String>> = sqlx::query_scalar(
        "UPDATE voip_calls c
            SET answered_by = $3, ring_deadline_at = NULL, user_id = $3, updated_at = now()
          FROM call_sessions s
          WHERE c.id = $1 AND c.org_id = $2 AND c.session_id = s.id
            AND c.direction = 'inbound' AND c.answered_by IS NULL
          RETURNING s.room",
    )
    .bind(call_id)
    .bind(org_id)
    .bind(user.user_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;

    match room {
        // Somebody else already took it, or it is over. Not an error — two colleagues
        // reaching for the same ringing call is the normal case, not a fault.
        None => Err(refuse(StatusCode::CONFLICT, "already_answered")),
        Some(room) => Ok(Json(json!({ "room": room })).into_response()),
    }
}

/// `POST …/voip/calls/{id}/hangup`.
pub async fn hangup(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, call_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let role = require_role(pool, org_id, user.user_id, MEMBER).await?;
    let is_admin = matches!(role.as_str(), "admin" | "owner");
    let provider = provider(&state)?;

    let legs: Option<Vec<String>> = sqlx::query_scalar(
        "SELECT provider_leg_ids FROM voip_calls
         WHERE id = $1 AND org_id = $2 AND ($3 OR user_id = $4)",
    )
    .bind(call_id)
    .bind(org_id)
    .bind(is_admin)
    .bind(user.user_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;

    let Some(legs) = legs else {
        return Err(not_found("call not found"));
    };

    for leg in legs {
        // Best effort per leg: the far party may already have hung up one of them, and
        // that race must not stop us tearing down the other.
        let _ = provider.hangup(&crate::telephony::LegId::new(leg)).await;
    }

    // The authoritative state change comes from the hangup webhook. Marking it here would
    // race the provider and could settle a call that is still connected.
    Ok(Json(json!({ "requested": true })).into_response())
}

/// `GET …/voip/settings` (member) and `PUT` (admin).
pub async fn get_settings(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;
    let s = service::load_org_settings(pool, org_id)
        .await
        .map_err(db_err)?;
    Ok(Json(json!({
        "enabled": s.policy.enabled,
        "home_country": s.home_country,
        "allowed_countries": s.policy.allowed_countries,
        "blocked_countries": s.policy.blocked_countries,
        "allow_international": s.policy.allow_international,
        "consent_policy": s.consent_policy.as_str(),
        "consent_refused_action": s.consent_refused_action.as_str(),
        "recording_enabled": s.recording_enabled,
        "transcription_enabled": s.transcription_enabled,
        "ai_analysis_enabled": s.ai_analysis_enabled,
        "max_call_minutes": s.max_call_minutes,
        "max_concurrent_per_user": s.policy.max_concurrent_per_user,
        "max_concurrent_per_org": s.policy.max_concurrent_per_org,
        "default_engine_id": s.default_engine_id,
        "require_project": s.require_project,
    }))
    .into_response())
}

#[derive(Debug, Deserialize)]
pub struct SettingsBody {
    enabled: bool,
    #[serde(default)]
    home_country: Option<String>,
    #[serde(default)]
    allowed_countries: Vec<String>,
    #[serde(default)]
    blocked_countries: Vec<String>,
    #[serde(default = "yes")]
    allow_international: bool,
    #[serde(default)]
    consent_policy: Option<String>,
    #[serde(default)]
    consent_refused_action: Option<String>,
    #[serde(default)]
    recording_enabled: bool,
    // NOT `default = "yes"`. An admin who omits this field has said nothing about
    // transcription, and `OrgSettings::default_for_new_org` is explicit that capture stays
    // off until somebody asks for it. Migration 059 brings the column default into line.
    #[serde(default)]
    transcription_enabled: bool,
    #[serde(default)]
    ai_analysis_enabled: bool,
    #[serde(default)]
    max_call_minutes: Option<i32>,
    #[serde(default)]
    max_concurrent_per_user: Option<i32>,
    #[serde(default)]
    max_concurrent_per_org: Option<i32>,
    #[serde(default)]
    default_engine_id: Option<String>,
    #[serde(default)]
    require_project: bool,
}

fn yes() -> bool {
    true
}

pub async fn put_settings(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Json(body): Json<SettingsBody>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;

    // Refuse the incoherent combination at the boundary rather than letting the call-time
    // escalation quietly override an admin's setting on every call.
    let policy =
        consent::ConsentPolicy::parse(body.consent_policy.as_deref().unwrap_or("press_key"));
    if policy == consent::ConsentPolicy::Disabled
        && (body.recording_enabled || body.transcription_enabled)
    {
        return Err(refuse(
            StatusCode::BAD_REQUEST,
            "consent_required_for_capture",
        ));
    }

    let countries_ok = |list: &[String]| {
        list.iter()
            .all(|c| c.len() == 2 && c.chars().all(|ch| ch.is_ascii_alphabetic()))
    };
    if !countries_ok(&body.allowed_countries) || !countries_ok(&body.blocked_countries) {
        return Err(refuse(StatusCode::BAD_REQUEST, "invalid_country_code"));
    }
    if let Some(home) = &body.home_country {
        if !countries_ok(std::slice::from_ref(home)) {
            return Err(refuse(StatusCode::BAD_REQUEST, "invalid_country_code"));
        }
    }

    let upper: Vec<String> = body
        .allowed_countries
        .iter()
        .map(|c| c.to_uppercase())
        .collect();
    let blocked: Vec<String> = body
        .blocked_countries
        .iter()
        .map(|c| c.to_uppercase())
        .collect();

    sqlx::query(
        "INSERT INTO voip_org_settings
            (org_id, enabled, home_country, allowed_countries, blocked_countries,
             allow_international, consent_policy, consent_refused_action, recording_enabled,
             transcription_enabled, ai_analysis_enabled, max_call_minutes,
             max_concurrent_per_user, max_concurrent_per_org, default_engine_id,
             require_project)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,
                 COALESCE($12, 60), COALESCE($13, 2), COALESCE($14, 10), $15, $16)
         ON CONFLICT (org_id) DO UPDATE SET
            enabled = EXCLUDED.enabled,
            home_country = EXCLUDED.home_country,
            allowed_countries = EXCLUDED.allowed_countries,
            blocked_countries = EXCLUDED.blocked_countries,
            allow_international = EXCLUDED.allow_international,
            consent_policy = EXCLUDED.consent_policy,
            consent_refused_action = EXCLUDED.consent_refused_action,
            recording_enabled = EXCLUDED.recording_enabled,
            transcription_enabled = EXCLUDED.transcription_enabled,
            ai_analysis_enabled = EXCLUDED.ai_analysis_enabled,
            max_call_minutes = EXCLUDED.max_call_minutes,
            max_concurrent_per_user = EXCLUDED.max_concurrent_per_user,
            max_concurrent_per_org = EXCLUDED.max_concurrent_per_org,
            default_engine_id = EXCLUDED.default_engine_id,
            require_project = EXCLUDED.require_project,
            updated_at = now()",
    )
    .bind(org_id)
    .bind(body.enabled)
    .bind(body.home_country.as_ref().map(|c| c.to_uppercase()))
    .bind(&upper)
    .bind(&blocked)
    .bind(body.allow_international)
    .bind(policy.as_str())
    .bind(
        consent::RefusedAction::parse(
            body.consent_refused_action
                .as_deref()
                .unwrap_or("continue_unrecorded"),
        )
        .as_str(),
    )
    .bind(body.recording_enabled)
    .bind(body.transcription_enabled)
    .bind(body.ai_analysis_enabled)
    .bind(body.max_call_minutes)
    .bind(body.max_concurrent_per_user)
    .bind(body.max_concurrent_per_org)
    .bind(body.default_engine_id.as_deref())
    .bind(body.require_project)
    .execute(pool)
    .await
    .map_err(db_err)?;

    crate::business::audit::log_audit_event(
        pool,
        org_id,
        user.user_id,
        "voip.settings.updated",
        "voip_org_settings",
        org_id,
        json!({
            "enabled": body.enabled,
            "recording_enabled": body.recording_enabled,
            "transcription_enabled": body.transcription_enabled,
            "consent_policy": policy.as_str(),
        }),
    );

    get_settings(State(state), user, Path(org_id)).await
}

/// `POST /api/voip/webhooks/{provider}` — the provider's callback.
///
/// Unauthenticated in the session sense and authenticated cryptographically: the Ed25519
/// signature is checked before the body is parsed. Always answers 200 once verified, even
/// for an event it does nothing with — a provider that is told "error" retries forever.
pub async fn inbound_webhook(
    State(state): State<AppState>,
    Path(provider_id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let provider = provider(&state)?;

    if provider.metadata().id != provider_id {
        return Err(not_found("unknown provider"));
    }

    let h = WebhookHeaders {
        signature: header(&headers, "telnyx-signature-ed25519")
            .or_else(|| header(&headers, "x-signature")),
        timestamp: header(&headers, "telnyx-timestamp").or_else(|| header(&headers, "x-timestamp")),
    };

    match webhook::ingest(pool, provider, &h, &body).await {
        Ok((outcome, event)) => {
            tracing::debug!(?outcome, "voip webhook");

            // Somebody is calling US (spec 0116). There is no call row yet, so `apply`
            // above answered `Unknown` — admission is what creates one. Done here rather
            // than inside `apply` because it needs the room map, the engine registry and
            // the provider, none of which a database-only function has.
            if let crate::telephony::ProviderEventKind::Incoming { from, to } = &event.kind {
                match crate::voip::inbound::admit(&state, pool, provider, &event.leg_id, from, to)
                    .await
                {
                    Ok(call_id) => tracing::info!(call = %call_id, "inbound call admitted"),
                    Err(reason) => {
                        // Hung up, and nothing written. A stranger who dialled a wrong
                        // number is owed a normal busy tone, not a row in somebody's
                        // history — and not an explanation of our schema either.
                        tracing::info!(reason = reason.as_str(), "inbound call refused");
                        let _ = provider.hangup(&event.leg_id).await;
                    }
                }
            }
            // The far end just picked up. Everything else about the call already worked
            // without this line — it rang, it billed, it settled, it appeared in history —
            // and it did all of that in silence. This is the audio path.
            if let webhook::Ingest::Applied {
                call_id,
                after: crate::voip::state::CallState::Answered,
                ..
            } = outcome
            {
                // **Disclosure first, media second.** Not a preference — an ordering the
                // audio path depends on.
                //
                // The engine session is handed a transcript service only if the row
                // already says transcription is permitted (`session::run_leg`), and it
                // cannot be given one afterwards. So a call whose gate is still open must
                // not open its engine session yet: it would be created un-permitted, and
                // the recipient pressing 1 a second later could not change that — a
                // granted call would produce no transcript at all.
                //
                // The cost is a few seconds of silence for the caller at the start of a
                // gated call. During those seconds the recipient is listening to the
                // announcement rather than talking, so there is nothing to translate.
                let delivery = match crate::voip::disclosure::announce_on_answer(
                    pool,
                    provider,
                    call_id,
                    &event.leg_id,
                )
                .await
                {
                    Ok(d) => d,
                    Err(e) => {
                        // The call is up and nothing was captured, which is the safe side
                        // of this failure. Logged rather than returned: a 5xx would make
                        // the provider redeliver the answer event, and the second delivery
                        // is a duplicate that changes nothing.
                        crate::metrics::record_voip_disclosure_failure();
                        tracing::error!(%call_id, error = %e, "consent announcement step failed");
                        crate::voip::disclosure::Delivery::NotAnnounced {
                            reason: "disclosure_write_failed",
                        }
                    }
                };

                // Everything except an open gate arms now. An open gate arms when the
                // digit lands, or when the sweep gives up waiting for it.
                if !matches!(
                    delivery,
                    crate::voip::disclosure::Delivery::AwaitingConsent { .. }
                ) {
                    if let Ok(vcfg) = cfg(&state) {
                        crate::voip::session::arm_media(
                            &state,
                            vcfg,
                            provider,
                            call_id,
                            &event.leg_id,
                        )
                        .await;
                    }
                }
            }
            // A call that is over releases its phone leg. The common case is not a crash:
            // it is a recipient who was busy, rejected the call, or never picked up — all
            // of which end the call before any media socket exists to claim the leg.
            if let webhook::Ingest::Applied { call_id, after, .. } = &outcome {
                if after.is_terminal() {
                    crate::voip::session::reclaim(&state, *call_id);
                }
            }

            // A keypad digit while a consent gate is open is the recipient answering.
            if let (
                webhook::Ingest::Recorded { call_id, .. },
                crate::telephony::ProviderEventKind::Dtmf { digit },
            ) = (&outcome, &event.kind)
            {
                match crate::voip::disclosure::on_dtmf(
                    pool,
                    provider,
                    *call_id,
                    &event.leg_id,
                    *digit,
                )
                .await
                {
                    // The gate is settled, so the engine session can finally be opened —
                    // knowing whether it may keep words. `EndCall` is the one outcome that
                    // arms nothing: the recipient refused and org policy says the call ends.
                    Ok(Some(crate::voip::consent::AfterConsent::StartCapture))
                    | Ok(Some(crate::voip::consent::AfterConsent::ContinueWithoutCapture)) => {
                        if let Ok(vcfg) = cfg(&state) {
                            crate::voip::session::arm_media(
                                &state,
                                vcfg,
                                provider,
                                *call_id,
                                &event.leg_id,
                            )
                            .await;
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::error!(%call_id, error = %e, "could not resolve the consent gate")
                    }
                }
            }
            Ok(StatusCode::OK.into_response())
        }
        Err(e) if e.is_retryable() => {
            // We could not process a webhook that may well be valid. 503 so the provider
            // redelivers — a 401 here would tell it "this will never verify" and the event
            // would be lost, leaving the call non-terminal with credits held.
            tracing::warn!(error = e.code(), "voip webhook could not be processed");
            Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": e.code() })),
            )
                .into_response())
        }
        // A REFUSED webhook is a 401, not a 5xx: the provider must not retry something
        // that will never verify, and a 5xx would make it try for hours.
        Err(e) => {
            // A sustained non-zero rate here means someone who cannot sign is posting to
            // the endpoint, which is worth an alert on its own.
            crate::metrics::record_voip_webhook_rejected();
            Err((StatusCode::UNAUTHORIZED, Json(json!({ "error": e.code() }))).into_response())
        }
    }
}

/// `POST …/voip/calls/{id}/video-invite` — offer the recipient a browser room.
///
/// Returns a link the caller passes on however they like. There is no channel from here to
/// a telephone: they are already talking to the person, so reading it out or sending it
/// through whatever they already use is the delivery mechanism. An SMS integration would
/// be a second provider surface and a per-message charge, and is not required for this to
/// work.
///
/// **Nothing here touches the call.** No carrier command, no media change, no billing
/// row — an upgrade that fails leaves two people on the telephone exactly as they were,
/// which is what D9 requires.
pub async fn video_invite(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, call_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let cfg = cfg(&state)?;
    let role = require_role(pool, org_id, user.user_id, MEMBER).await?;
    let is_admin = matches!(role.as_str(), "admin" | "owner");

    if !cfg.video_enabled {
        // Off is off: 404, the same answer the whole feature gives when it is disabled,
        // rather than a refusal that confirms the call exists.
        return Err(not_found("video is not enabled"));
    }

    // Tenancy in the WHERE clause, and the room only while the call is live: inviting
    // someone into a room whose call is over puts them alone in an empty conversation.
    let room: Option<Option<String>> = sqlx::query_scalar(
        "SELECT CASE WHEN c.status NOT IN ('completed', 'failed') THEN s.room END
         FROM voip_calls c
         JOIN call_sessions s ON s.id = c.session_id
         WHERE c.id = $1 AND c.org_id = $2 AND ($3 OR c.user_id = $4)",
    )
    .bind(call_id)
    .bind(org_id)
    .bind(is_admin)
    .bind(user.user_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;

    let Some(Some(room)) = room else {
        // One 404 for "no such call", "not yours" and "already over". A different answer
        // for each would tell an unauthorised caller which of the three it was.
        return Err(not_found("call not found"));
    };

    let (ticket, expires) =
        crate::voip::video::issue(&cfg.video_invite_key, call_id, &room, chrono::Utc::now());
    let api_base = crate::voip::video::api_base_from_ws(&cfg.media_ws_base);
    let url = crate::voip::video::invite_url(&api_base, &ticket);

    // Audited: someone was invited into a conversation, and who offered is a fact worth
    // keeping. The URL is NOT recorded — it is a live capability, and an audit log is read
    // by more people than the call was.
    crate::business::audit::log_audit_event(
        pool,
        org_id,
        user.user_id,
        "voip.video_invite",
        "voip_call",
        call_id,
        json!({ "expires_at": expires }),
    );

    Ok(Json(json!({ "url": url, "expires_at": expires })).into_response())
}

/// `GET /api/voip/video/{ticket}` — redeem an invitation and go to the room.
///
/// A redirect rather than a JSON body, because the person opening it is a human with a
/// link, not a client with a parser.
pub async fn redeem_video_invite(
    State(state): State<AppState>,
    Path(ticket): Path<String>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let cfg = cfg(&state)?;

    let invite = crate::voip::video::verify(
        &cfg.video_invite_key,
        &ticket,
        chrono::Utc::now().timestamp(),
    )
    .map_err(|e| {
        tracing::warn!(reason = e.code(), "video invite refused");
        not_found("this invitation is no longer valid")
    })?;

    // The signature proves the room was ours to give. Whether it is still worth giving is
    // a separate question, and only the database can answer it.
    let live: Option<bool> = sqlx::query_scalar(
        "SELECT status NOT IN ('completed', 'failed') FROM voip_calls WHERE id = $1",
    )
    .bind(invite.call_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;

    if live != Some(true) {
        return Err(not_found("this invitation is no longer valid"));
    }

    let to = crate::voip::video::join_url(&state.config.app_base_url, &invite.room);
    Ok((StatusCode::SEE_OTHER, [(axum::http::header::LOCATION, to)]).into_response())
}

/// Serve one phone leg's media socket.
///
/// Everything that authorises this connection is in the ticket, which is why the handler
/// can be this short: redeem it, claim the leg it names, and hand the socket to the bridge.
/// A ticket that is forged, expired, or already spent gets a 404 rather than a 401 — the
/// endpoint should not confirm to an unauthenticated caller that a call exists at all.
pub async fn media_socket(
    State(state): State<AppState>,
    Path(ticket): Path<String>,
    ws: axum::extract::WebSocketUpgrade,
) -> Response {
    let Some(cfg) = state.config.voip.as_ref() else {
        return not_found("voip is not configured");
    };

    let redeemed = state.voip_tickets.redeem(
        &cfg.media_ticket_key,
        &ticket,
        chrono::Utc::now().timestamp(),
    );
    let Ok(t) = redeemed else {
        crate::metrics::record_voip_webhook_rejected();
        tracing::warn!("media socket presented an unusable ticket");
        return not_found("no such media session");
    };

    let Some(leg) = state.voip_calls.take(t.call_id) else {
        // Redeemed but unclaimable: the call failed between the answer and the connection,
        // or a socket already has it. Either way there is nothing to bridge.
        tracing::warn!(call_id = %t.call_id, "media socket for a call with no parked leg");
        return not_found("no such media session");
    };

    // The ticket names a room and a peer as well as a call, and `token.rs` documents that
    // binding as a security property. Checking it is what makes the claim true rather than
    // aspirational: today one call has one leg, but modes 2 and 3 in the spec put two legs
    // on one call, and a ticket minted for one of them must not be able to claim the other.
    if t.room != leg.room || t.peer_id != leg.peer_id {
        tracing::error!(
            call_id = %t.call_id,
            "media ticket does not match the parked leg it claimed"
        );
        crate::voip::session::reclaim(&state, t.call_id);
        return not_found("no such media session");
    }

    ws.on_upgrade(move |socket| async move {
        let call_id = leg.call_id;
        if let Err(e) = crate::voip::session::run_leg(
            &state,
            leg,
            crate::telephony::MediaCodec::L16,
            crate::voip::session::TextSocket(socket),
        )
        .await
        {
            tracing::warn!(%call_id, error = ?e, "phone media bridge ended with an error");
        }
    })
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}
