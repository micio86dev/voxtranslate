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

use crate::business::{
    bad_request, db_err, forbidden, not_found, require_pool, require_role, ADMIN, MEMBER,
};
use crate::middleware::AuthUser;
use crate::telephony::{WebhookHeaders, E164};
use crate::voip::service::{self, DialOptions, VoipError};
use crate::voip::{consent, webhook};
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
            "/api/business/organizations/{org_id}/voip/calls/{call_id}/hangup",
            post(hangup),
        )
        .route(
            "/api/business/organizations/{org_id}/voip/settings",
            get(get_settings).put(put_settings),
        )
        // Unauthenticated by design: the provider cannot present a session. The Ed25519
        // signature IS the authentication, and it is verified before anything is read.
        .route("/api/voip/webhooks/{provider}", post(inbound_webhook))
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

    if world.org.require_project && body.project_id.is_none() {
        return Err(bad_request(
            "this organization requires a project on every call",
        ));
    }
    if let Some(project_id) = body.project_id {
        let ok: Option<bool> =
            sqlx::query_scalar("SELECT true FROM projects WHERE id = $1 AND org_id = $2")
                .bind(project_id)
                .bind(org_id)
                .fetch_optional(pool)
                .await
                .map_err(db_err)?;
        if ok.is_none() {
            return Err(bad_request("project does not belong to this organization"));
        }
    }

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
            return Err(forbidden(
                "caller id is not a verified outbound number for this organization",
            ))
        }
        (None, None) => provider_default
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| bad_request("no caller id is configured for this organization"))?,
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
    actual_provider_cost_usd: Option<Decimal>,
    gross_margin: Option<Decimal>,
    recording_status: String,
    transcription_status: String,
    consent_status: String,
    project_id: Option<Uuid>,
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
        "SELECT id, session_id, status, failure_reason, direction, recipient_e164,
                recipient_country, source_language, target_language, engine_id, started_at,
                ended_at, duration_seconds, credits_consumed, quoted_price_per_min,
                actual_provider_cost_usd, gross_margin, recording_status,
                transcription_status, consent_status, project_id
         FROM voip_calls WHERE id = $1 AND org_id = $2 AND ($3 OR user_id = $4)",
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
    // `actual_provider_cost_usd` is null until the provider rates the call, and is
    // surfaced as null rather than as zero — a zero would read as "free", which is a very
    // different claim from "not yet known". See docs/voip-telnyx-setup.md §6.
    Ok(Json(r).into_response())
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
    #[serde(default = "yes")]
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
        return Err(bad_request(
            "consent cannot be disabled while recording or transcription is enabled",
        ));
    }

    let countries_ok = |list: &[String]| {
        list.iter()
            .all(|c| c.len() == 2 && c.chars().all(|ch| ch.is_ascii_alphabetic()))
    };
    if !countries_ok(&body.allowed_countries) || !countries_ok(&body.blocked_countries) {
        return Err(bad_request("countries must be ISO 3166-1 alpha-2 codes"));
    }
    if let Some(home) = &body.home_country {
        if !countries_ok(std::slice::from_ref(home)) {
            return Err(bad_request(
                "home_country must be an ISO 3166-1 alpha-2 code",
            ));
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
        Ok(outcome) => {
            tracing::debug!(?outcome, "voip webhook");
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

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}
