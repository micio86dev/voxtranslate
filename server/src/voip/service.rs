//! Placing a translated telephone call (spec 0111, §4 sequence).
//!
//! The order of operations is the design, and it is chosen so that nothing irreversible
//! happens before everything reversible has been checked:
//!
//! 1. Normalise the destination — a typo must not reach a carrier.
//! 2. Load the world (policy, live counts, today's spend, the rate) in one place.
//! 3. Run [`policy::check`] — every refusal happens here, **before** the provider exists.
//! 4. Price it.
//! 5. Create the rows.
//! 6. **Reserve the credits**, atomically. This is the last point at which "no" is free.
//! 7. Only now, dial.
//!
//! Step 6 before step 7 is what makes concurrent dials safe. Reversing them would let two
//! calls start against one balance and discover the problem after the carrier had already
//! connected them.

use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use sqlx::FromRow;
use uuid::Uuid;

use crate::config::VoipConfig;
use crate::db::Pool;
use crate::engine::EngineMetadata;
use crate::telephony::{DialRequest, E164Error, TelephonyProvider, E164};
use crate::voip::consent::{self, CaptureIntent, ConsentPolicy, RefusedAction};
use crate::voip::policy::{self, DialContext, GlobalPolicy, OrgPolicy, RolloutStage};
use crate::voip::pricing::{self, MarginPolicy, ProviderCost, Quote, Rate};
use crate::voip::reservation::{self, ReserveOutcome};
use crate::voip::session;
use crate::voip::state::{CallState, FailureReason};

/// Why a request was refused, in a form a route handler can turn into a status code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoipError {
    /// The destination is not a usable telephone number.
    BadNumber(E164Error),
    /// A policy, credit or provider refusal. Carries the stable reason the dashboard
    /// localises.
    Refused(FailureReason),
    /// Misconfiguration that should stop the deployment being used, not just this call.
    Misconfigured(&'static str),
    /// Storage failed. Deliberately opaque — a database error must not become an API
    /// payload.
    Storage,
}

impl VoipError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadNumber(e) => e.code(),
            Self::Refused(r) => r.as_str(),
            Self::Misconfigured(_) => "voip_misconfigured",
            Self::Storage => "storage_error",
        }
    }
}

impl From<sqlx::Error> for VoipError {
    fn from(_: sqlx::Error) -> Self {
        Self::Storage
    }
}

/// What the dialer is told before the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuoteView {
    pub destination: String,
    pub destination_masked: String,
    pub country: String,
    /// USD per minute the customer will be charged.
    pub price_per_minute: Decimal,
    pub currency: String,
    /// Credits that will be held when the call starts.
    pub reserve_credits: i32,
    /// The org pool right now.
    pub balance_credits: i32,
    pub engine_id: String,
    pub recording: bool,
    pub transcription: bool,
    pub consent_policy: &'static str,
    /// Whether the announcement will be played, and in which language.
    pub disclosure_language: Option<String>,
}

/// A call that has been created and handed to the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallCreated {
    pub call_id: Uuid,
    pub session_id: Uuid,
    /// The room the telephone was joined into.
    ///
    /// Returned so the caller's browser can join it. Without this the call is a telephone
    /// talking to an empty room: the engine translates a speaker into "the room's other
    /// languages", and a room with only the phone in it has none — so nothing is
    /// translated, in either direction, and the call is silent for both parties.
    pub room: String,
    pub status: CallState,
    pub reserved_credits: i32,
    pub price_per_minute: Decimal,
}

/// What the caller asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialOptions {
    pub destination: String,
    pub source_language: String,
    pub target_language: String,
    pub engine_id: String,
    pub project_id: Option<Uuid>,
    pub caller_id: Option<String>,
    pub record: bool,
    pub transcribe: bool,
    pub ai_analysis: bool,
    /// Minutes to size the credit hold against. Clamped to the org's maximum duration —
    /// holding more than the call can possibly cost is just money the org cannot spend
    /// elsewhere.
    pub estimated_minutes: i32,
}

/// The per-org settings row, as stored.
#[derive(Debug, Clone, FromRow)]
struct OrgSettingsRow {
    enabled: bool,
    allowed_countries: Vec<String>,
    blocked_countries: Vec<String>,
    allow_international: bool,
    home_country: Option<String>,
    consent_policy: String,
    consent_refused_action: String,
    recording_enabled: bool,
    transcription_enabled: bool,
    ai_analysis_enabled: bool,
    max_call_minutes: i32,
    max_concurrent_per_user: i32,
    max_concurrent_per_org: i32,
    monthly_spend_limit_credits: Option<i32>,
    default_engine_id: Option<String>,
    require_project: bool,
}

/// Resolved org configuration.
#[derive(Debug, Clone)]
pub struct OrgSettings {
    pub policy: OrgPolicy,
    /// ISO 3166-1 alpha-2, or `None` when the org has not set one.
    pub home_country: Option<String>,
    pub consent_policy: ConsentPolicy,
    pub consent_refused_action: RefusedAction,
    pub recording_enabled: bool,
    pub transcription_enabled: bool,
    pub ai_analysis_enabled: bool,
    pub max_call_minutes: i32,
    pub default_engine_id: Option<String>,
    pub require_project: bool,
}

impl OrgSettings {
    /// An organization with no settings row has VoIP **off**. Enabling it is always an
    /// explicit administrative act, which is what makes the audit trail meaningful.
    fn disabled() -> Self {
        Self {
            policy: OrgPolicy::default(),
            home_country: None,
            consent_policy: ConsentPolicy::PressKey,
            consent_refused_action: RefusedAction::ContinueUnrecorded,
            recording_enabled: false,
            transcription_enabled: false,
            ai_analysis_enabled: false,
            max_call_minutes: 60,
            default_engine_id: None,
            require_project: false,
        }
    }
}

/// Load an org's VoIP settings. Absent ⇒ disabled.
pub async fn load_org_settings(pool: &Pool, org_id: Uuid) -> Result<OrgSettings, sqlx::Error> {
    let row: Option<OrgSettingsRow> = sqlx::query_as(
        "SELECT enabled, allowed_countries, blocked_countries, allow_international,
                home_country, consent_policy, consent_refused_action, recording_enabled,
                transcription_enabled, ai_analysis_enabled, max_call_minutes,
                max_concurrent_per_user, max_concurrent_per_org,
                monthly_spend_limit_credits, default_engine_id, require_project
         FROM voip_org_settings WHERE org_id = $1",
    )
    .bind(org_id)
    .fetch_optional(pool)
    .await?;

    Ok(match row {
        None => OrgSettings::disabled(),
        Some(r) => OrgSettings {
            policy: OrgPolicy {
                enabled: r.enabled,
                allowed_countries: r.allowed_countries,
                blocked_countries: r.blocked_countries,
                allow_international: r.allow_international,
                max_concurrent_per_user: r.max_concurrent_per_user,
                max_concurrent_per_org: r.max_concurrent_per_org,
                monthly_spend_limit_credits: r.monthly_spend_limit_credits,
            },
            home_country: r.home_country,
            consent_policy: ConsentPolicy::parse(&r.consent_policy),
            consent_refused_action: RefusedAction::parse(&r.consent_refused_action),
            recording_enabled: r.recording_enabled,
            transcription_enabled: r.transcription_enabled,
            ai_analysis_enabled: r.ai_analysis_enabled,
            max_call_minutes: r.max_call_minutes,
            default_engine_id: r.default_engine_id,
            require_project: r.require_project,
        },
    })
}

/// Build the deployment-wide half of the policy from configuration.
pub fn global_policy(cfg: &VoipConfig) -> GlobalPolicy {
    GlobalPolicy {
        rollout_stage: RolloutStage::parse(&cfg.rollout_stage),
        require_eu_processing: cfg.require_eu_processing,
        max_destination_rate: pricing::usd_from_config(cfg.max_destination_rate),
        daily_provider_spend_limit: pricing::usd_from_config(cfg.daily_provider_spend_limit),
        max_concurrent_global: cfg.max_concurrent_global,
        allow_international: cfg.allow_international,
        allowed_countries: cfg.allowed_countries.clone(),
        blocked_countries: cfg.blocked_countries.clone(),
        china_enabled: cfg.china_enabled,
        china_require_validated_route: cfg.china_require_validated_route,
    }
}

/// Advisory-lock key for call admission.
///
/// One constant key, so the count-then-insert that enforces the concurrency caps is
/// serialised across the whole deployment. That sounds heavy and is not: dials are a few
/// per second at most, and the lock is held for one COUNT and one INSERT. A per-org key
/// would be cheaper and would leave `max_concurrent_global` racy, which is exactly the
/// hole R6 says must not exist.
const ADMISSION_LOCK: i64 = 0x0111_0000_0001;

/// Live call counts: `(user, org, global)`.
///
/// One statement, so the three numbers describe the same instant. Three separate queries
/// could each be true and jointly wrong.
///
/// Reading them is still only a **snapshot**: it is what the dialer's quote is priced
/// against, and it is deliberately unlocked there because a quote makes no commitment.
/// The enforcement copy lives inside [`dial`]'s transaction, under [`ADMISSION_LOCK`].
pub async fn live_counts(
    pool: &Pool,
    org_id: Uuid,
    user_id: Uuid,
) -> Result<(i32, i32, i32), sqlx::Error> {
    let row: (i64, i64, i64) = sqlx::query_as(LIVE_COUNTS_SQL)
        .bind(org_id)
        .bind(user_id)
        .fetch_one(pool)
        .await?;
    Ok((row.0 as i32, row.1 as i32, row.2 as i32))
}

const LIVE_COUNTS_SQL: &str = "SELECT
            count(*) FILTER (WHERE user_id = $2)::bigint,
            count(*) FILTER (WHERE org_id = $1)::bigint,
            count(*)::bigint
         FROM voip_calls
         WHERE status IN ('created','dialing','ringing','answered','bridged','ending')";

/// Provider cost incurred today, USD. Falls back to the estimate for calls the provider
/// has not rated yet, so an unreconciled backlog cannot hide a spend spike.
pub async fn daily_spend(pool: &Pool) -> Result<Decimal, sqlx::Error> {
    let total: Option<Decimal> = sqlx::query_scalar(
        "SELECT COALESCE(SUM(COALESCE(actual_provider_cost_usd, estimated_cost_usd, 0)), 0)
         FROM voip_calls WHERE started_at >= date_trunc('day', now())",
    )
    .fetch_one(pool)
    .await?;
    Ok(total.unwrap_or(Decimal::ZERO))
}

/// Longest-prefix rate lookup, freshness-checked. `None` refuses the call (R5).
pub async fn lookup_rate(
    pool: &Pool,
    provider: &str,
    dest: &E164,
    max_age_secs: i64,
) -> Result<Option<Rate>, sqlx::Error> {
    let row: Option<(String, Decimal, String, chrono::DateTime<Utc>)> = sqlx::query_as(
        "SELECT prefix, cost_per_minute, description, fetched_at
         FROM voip_rates
         WHERE provider = $1 AND $2 LIKE prefix || '%'
         ORDER BY length(prefix) DESC
         LIMIT 1",
    )
    .bind(provider)
    .bind(dest.digits())
    .fetch_optional(pool)
    .await?;

    Ok(
        row.and_then(|(prefix, cost_per_minute, description, fetched_at)| {
            // Stale is the same as missing. Deliberate: dialing on a price we are not sure of
            // means discovering it on the invoice.
            if Utc::now().signed_duration_since(fetched_at) > Duration::seconds(max_age_secs.max(1))
            {
                return None;
            }
            Some(Rate {
                prefix,
                cost_per_minute,
                description,
                fetched_at,
            })
        }),
    )
}

/// Whether a recorded, in-country, unexpired route validation covers this destination.
pub async fn route_validated(
    pool: &Pool,
    provider: &str,
    dest: &E164,
) -> Result<bool, sqlx::Error> {
    let found: Option<bool> = sqlx::query_scalar(
        "SELECT true FROM voip_route_validations
         WHERE provider = $1 AND country = $2 AND valid
           AND tested_from_country = country
           AND (expires_at IS NULL OR expires_at > now())
         LIMIT 1",
    )
    .bind(provider)
    .bind(dest.region())
    .fetch_optional(pool)
    .await?;
    Ok(found.unwrap_or(false))
}

/// The full per-minute provider cost for one call.
///
/// Translation is counted **twice** (R13): both directions run for the whole call, the
/// same shape as Talk to Anyone and for the same reason — closing the idle direction would
/// lose the first clause of every turn.
///
/// Recording and its storage are only charged when recording is actually on, and they come
/// from configuration rather than from a literal. An earlier version wrote
/// `if recording { usd_from_config(0.0) } else { ZERO }` — both branches were zero, so a
/// recorded call was priced exactly like an unrecorded one and the margin floor was proven
/// against a cost that was too low. The floor is only as good as the cost fed to it.
pub fn provider_cost(
    cfg: &VoipConfig,
    rate: &Rate,
    engine: &EngineMetadata,
    recording: bool,
) -> ProviderCost {
    ProviderCost {
        telephony: rate.cost_per_minute,
        translation: pricing::usd_from_config(engine.cost_per_minute) * Decimal::TWO,
        media_streaming: pricing::usd_from_config(cfg.media_streaming_cost_per_minute),
        recording: if recording {
            pricing::usd_from_config(cfg.recording_cost_per_minute)
        } else {
            Decimal::ZERO
        },
        storage: if recording {
            pricing::usd_from_config(cfg.storage_cost_per_minute)
        } else {
            Decimal::ZERO
        },
        ancillary: Decimal::ZERO,
    }
}

/// Everything the decision needs, loaded once.
pub struct DialWorld {
    pub org: OrgSettings,
    pub global: GlobalPolicy,
    pub rate: Option<Rate>,
    pub route_validated: bool,
    pub live_user: i32,
    pub live_org: i32,
    pub live_global: i32,
    pub daily_spend: Decimal,
}

/// Load the snapshot the policy check runs against.
pub async fn load_world(
    pool: &Pool,
    cfg: &VoipConfig,
    org_id: Uuid,
    user_id: Uuid,
    dest: &E164,
) -> Result<DialWorld, sqlx::Error> {
    let (live_user, live_org, live_global) = live_counts(pool, org_id, user_id).await?;
    let org = load_org_settings(pool, org_id).await?;
    Ok(DialWorld {
        org,
        global: global_policy(cfg),
        rate: lookup_rate(pool, &cfg.provider, dest, cfg.rate_max_age_secs).await?,
        route_validated: route_validated(pool, &cfg.provider, dest).await?,
        live_user,
        live_org,
        live_global,
        daily_spend: daily_spend(pool).await?,
    })
}

/// Build the policy context from a loaded world. Split out so a caller can reuse one
/// `DialWorld` for both the quote and the dial without loading it twice.
#[allow(clippy::too_many_arguments)]
pub fn dial_context<'a>(
    dest: &'a E164,
    world: &'a DialWorld,
    subscription_active: bool,
    org_in_rollout_list: bool,
    tier_supports_eu_only: bool,
    provider_eu_telephony: bool,
) -> DialContext<'a> {
    DialContext {
        destination: dest,
        org: &world.org.policy,
        global: &world.global,
        subscription_active,
        org_in_rollout_list,
        tier_supports_eu_only,
        provider_eu_telephony,
        rate: world.rate.as_ref(),
        route_validated: world.route_validated,
        live_calls_user: world.live_user,
        live_calls_org: world.live_org,
        live_calls_global: world.live_global,
        daily_spend_usd: world.daily_spend,
        org_country: world.org.home_country.as_deref(),
    }
}

/// Resolve what will actually be captured, intersecting what the caller asked for with
/// what the org and the deployment permit.
///
/// Intersection, never union: a user cannot turn on recording an org has disabled, and an
/// org cannot turn on what the deployment has switched off.
pub fn capture_intent(cfg: &VoipConfig, org: &OrgSettings, opts: &DialOptions) -> CaptureIntent {
    CaptureIntent {
        recording: opts.record && org.recording_enabled && cfg.recording_enabled,
        transcription: opts.transcribe && org.transcription_enabled && cfg.transcription_enabled,
        ai_analysis: opts.ai_analysis && org.ai_analysis_enabled,
    }
    .normalised()
}

/// Price a call without placing it.
#[allow(clippy::too_many_arguments)]
pub fn build_quote(
    cfg: &VoipConfig,
    world: &DialWorld,
    engine: &EngineMetadata,
    intent: CaptureIntent,
    estimated_minutes: i32,
) -> Result<Quote, VoipError> {
    let rate = world
        .rate
        .as_ref()
        .ok_or(VoipError::Refused(FailureReason::RateUnavailable))?;
    let margin = MarginPolicy::new(
        pricing::usd_from_config(cfg.min_gross_margin),
        pricing::usd_from_config(cfg.cost_safety_buffer),
    )
    .map_err(|_| VoipError::Misconfigured("VOIP_MIN_GROSS_MARGIN / VOIP_COST_SAFETY_BUFFER"))?;

    let cost = provider_cost(cfg, rate, engine, intent.recording);
    pricing::quote(&margin, cost, estimated_minutes)
        .map_err(|_| VoipError::Misconfigured("VOIP pricing configuration"))
}

/// Create the call rows, hold the credits, and dial. See the module docs for why the
/// order is what it is.
#[allow(clippy::too_many_arguments)]
pub async fn dial(
    pool: &Pool,
    provider: &dyn TelephonyProvider,
    cfg: &VoipConfig,
    org_id: Uuid,
    user_id: Uuid,
    dest: &E164,
    caller_id: &E164,
    opts: &DialOptions,
    world: &DialWorld,
    quote: &Quote,
    intent: CaptureIntent,
    pseudonym: &str,
    rooms: &crate::rooms::RoomManager,
    live: &session::LiveCalls,
) -> Result<CallCreated, VoipError> {
    // The room exists BEFORE the call rows, and its session id becomes the call's. See
    // `session::PhonePeer::session_id` — the browser caller joins this same room by the
    // ordinary path and files its transcript under the room's id, so a call session with
    // an id of its own would split the conversation in half.
    let room = format!("ph-{}", Uuid::new_v4().simple());
    let peer = session::create_phone_peer(rooms, &room, &opts.engine_id, &opts.target_language)
        // A brand-new room name cannot be full; `Err` here means the room map is exhausted,
        // which is a capacity condition and reads as one.
        .map_err(|_| VoipError::Refused(FailureReason::ConcurrencyLimit))?;
    let session_id = peer.session_id;
    // Armed until the call is actually dialing. Every failure below — including the ones
    // that leave through `?` — takes the telephone back out of the room.
    let guard = session::PeerGuard::new(rooms, &peer);

    // Admission and creation happen in ONE transaction, under an advisory lock.
    //
    // The counts in `world` were read without a lock — fine for pricing a quote, useless
    // for enforcing a cap. Two dials that both saw "one below the limit" would both pass
    // `policy::check` and both create a call. `reservation.rs` says it in its own words:
    // any check-then-act is racy no matter how carefully it is written. So the cap is
    // re-checked here, against counts read inside the lock, immediately before the INSERT
    // that would break it.
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(ADMISSION_LOCK)
        .execute(&mut *tx)
        .await?;

    let counts: (i64, i64, i64) = sqlx::query_as(LIVE_COUNTS_SQL)
        .bind(org_id)
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await?;
    if counts.0 as i32 >= world.org.policy.max_concurrent_per_user
        || counts.1 as i32 >= world.org.policy.max_concurrent_per_org
        || counts.2 as i32 >= world.global.max_concurrent_global
    {
        return Err(VoipError::Refused(FailureReason::ConcurrencyLimit));
    }

    sqlx::query("INSERT INTO call_sessions (id, room, org_id, project_id, kind) VALUES ($1, $2, $3, $4, 'phone')")
        .bind(session_id)
        .bind(&room)
        .bind(org_id)
        .bind(opts.project_id)
        .execute(&mut *tx)
        .await?;

    let disclosure = consent::plan(
        world.org.consent_policy,
        intent,
        // AI disclosure follows the announcement rules: it is spoken when something is
        // captured anyway, and on its own only where required.
        true,
        &opts.target_language,
    );

    let call_id: Uuid = sqlx::query_scalar(
        "INSERT INTO voip_calls
            (session_id, org_id, user_id, project_id, provider, provider_region, direction,
             caller_e164, recipient_e164, recipient_pseudonym, recipient_country,
             source_language, target_language, engine_id, eu_processing_required,
             status, consent_policy, consent_status, estimated_cost_usd,
             quoted_price_per_min, recording_status, transcription_status)
         VALUES ($1, $2, $3, $4, $5, $6, 'outbound', $7, $8, $9, $10, $11, $12, $13, $14,
                 'created', $15, $16, $17, $18, $19, $20)
         RETURNING id",
    )
    .bind(session_id)
    .bind(org_id)
    .bind(user_id)
    .bind(opts.project_id)
    .bind(&cfg.provider)
    .bind(&cfg.default_region)
    .bind(caller_id.as_str())
    .bind(dest.as_str())
    .bind(pseudonym)
    .bind(dest.region())
    .bind(&opts.source_language)
    .bind(&opts.target_language)
    .bind(&opts.engine_id)
    .bind(cfg.require_eu_processing)
    .bind(world.org.consent_policy.as_str())
    .bind(disclosure.initial_status.as_str())
    .bind(quote.cost.total() * Decimal::from(quote.estimated_minutes))
    .bind(quote.price_per_minute)
    .bind(if intent.recording { "pending" } else { "none" })
    .bind(if intent.transcription { "live" } else { "none" })
    .fetch_one(&mut *tx)
    .await?;

    // Committing releases the admission lock. The row now exists and counts towards the
    // caps, so the next dial sees it — which is the whole point of doing it here.
    tx.commit().await?;

    // The last point at which "no" is free.
    match reservation::reserve(
        pool,
        org_id,
        call_id,
        session_id,
        Some(user_id),
        quote.reserve_credits,
    )
    .await?
    {
        ReserveOutcome::Held(_) => {}
        ReserveOutcome::Insufficient { .. } => {
            crate::metrics::record_voip_reservation_failure();
            fail(pool, call_id, FailureReason::InsufficientCredits).await?;
            return Err(VoipError::Refused(FailureReason::InsufficientCredits));
        }
    }

    // Parked before the dial, not after: on a fast answer the provider's `call.answered`
    // webhook can land while `dial` is still returning, and that webhook is what issues the
    // media ticket. A leg that is not yet parked would have nothing to issue it against.
    live.park_peer(
        peer,
        call_id,
        org_id,
        Some(user_id),
        &opts.engine_id,
        &opts.target_language,
    );

    // `client_state` is the call id: our own correlation id, echoed back on every webhook,
    // and it survives a leg being replaced in a way the provider's leg id does not.
    let req = DialRequest {
        to: dest.clone(),
        from: caller_id.clone(),
        client_state: call_id.to_string(),
        timeout_secs: 45,
        region: cfg.default_region.clone(),
    };

    match provider.dial(req).await {
        Ok(leg) => {
            sqlx::query(
                "UPDATE voip_calls
                 SET status = 'dialing',
                     provider_leg_ids = array_append(provider_leg_ids, $2),
                     updated_at = now()
                 WHERE id = $1",
            )
            .bind(call_id)
            .bind(leg.id.as_str())
            .execute(pool)
            .await?;

            crate::metrics::record_voip_started();
            guard.disarm();
            Ok(CallCreated {
                call_id,
                session_id,
                room: room.clone(),
                status: CallState::Dialing,
                reserved_credits: quote.reserve_credits,
                price_per_minute: quote.price_per_minute,
            })
        }
        Err(e) => {
            // The provider refused. Give the credits back immediately rather than leaving
            // them held until a sweep runs — the customer can see their balance.
            crate::metrics::record_voip_provider_error();
            let reason = e.as_failure_reason();
            fail(pool, call_id, reason).await?;
            reservation::release(pool, call_id, session_id).await?;
            // The leg was parked a moment ago and no socket will ever claim it; the guard
            // takes the peer out of the room as this returns.
            live.discard(call_id);
            Err(VoipError::Refused(reason))
        }
    }
}

async fn fail(pool: &Pool, call_id: Uuid, reason: FailureReason) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE voip_calls
         SET status = 'failed', failure_reason = COALESCE(failure_reason, $2),
             ended_at = COALESCE(ended_at, now()), updated_at = now()
         WHERE id = $1",
    )
    .bind(call_id)
    .bind(reason.as_str())
    .execute(pool)
    .await?;
    Ok(())
}

/// Run the whole pre-dial gate and produce the customer-facing quote.
#[allow(clippy::too_many_arguments)]
pub fn check_and_quote(
    cfg: &VoipConfig,
    world: &DialWorld,
    dest: &E164,
    engine: &EngineMetadata,
    intent: CaptureIntent,
    subscription_active: bool,
    org_in_rollout_list: bool,
    tier_supports_eu_only: bool,
    provider_eu_telephony: bool,
    estimated_minutes: i32,
) -> Result<Quote, VoipError> {
    let ctx = dial_context(
        dest,
        world,
        subscription_active,
        org_in_rollout_list,
        tier_supports_eu_only,
        provider_eu_telephony,
    );
    policy::check(&ctx).map_err(VoipError::Refused)?;
    build_quote(cfg, world, engine, intent, estimated_minutes)
}

/// Clamp the reservation horizon to something the call can actually reach.
///
/// Holding for longer than the org's maximum duration is money the org cannot spend
/// elsewhere, for a call that will be terminated before it gets there.
pub fn hold_minutes(requested: i32, org: &OrgSettings, cfg: &VoipConfig) -> i32 {
    requested
        .max(1)
        .min(org.max_call_minutes.max(1))
        .min(cfg.max_call_minutes.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{EngineCapabilities, EngineMetadata};

    fn engine(cost: f64) -> EngineMetadata {
        EngineMetadata {
            id: "standard".into(),
            display_name: "Standard".into(),
            tier: "standard".into(),
            description: String::new(),
            cost_per_minute: cost,
            markup: 0.25,
            input_languages: vec![],
            output_languages: vec![],
            capabilities: EngineCapabilities {
                translated_audio: true,
                cost_scales_per_language: true,
                client_direct: false,
                max_room_size: 4,
            },
        }
    }

    fn rate(cost: &str) -> Rate {
        Rate {
            prefix: "86".into(),
            cost_per_minute: cost.parse().unwrap(),
            description: "China".into(),
            fetched_at: Utc::now(),
        }
    }

    fn cfg() -> VoipConfig {
        VoipConfig::test_default()
    }

    #[test]
    fn translation_is_costed_in_both_directions() {
        // R13. One direction would look profitable and bill half the truth.
        let c = provider_cost(&cfg(), &rate("0.025"), &engine(0.0036), false);
        assert_eq!(c.telephony, "0.025".parse::<Decimal>().unwrap());
        assert_eq!(c.translation, "0.0072".parse::<Decimal>().unwrap());
        assert_eq!(c.total(), "0.0322".parse::<Decimal>().unwrap());
    }

    #[test]
    fn the_engine_raw_cost_is_used_not_its_marked_up_rate() {
        // Using the customer-facing rate as our cost would compound the markup and price
        // the call far above what the margin floor actually requires.
        let e = engine(0.0036);
        let c = provider_cost(&cfg(), &rate("0"), &e, false);
        assert_eq!(c.translation, "0.0072".parse::<Decimal>().unwrap());
        assert_ne!(
            c.translation,
            pricing::usd_from_config(e.user_rate_per_minute()) * Decimal::TWO
        );
    }

    #[test]
    fn a_recorded_call_costs_more_than_an_unrecorded_one() {
        // The regression two independent reviews found: the recording branch used a literal
        // 0.0, so both arms were zero and a recorded call was priced as if recording were
        // free — the margin floor then held against a cost that was not the real one.
        let mut c = cfg();
        c.recording_cost_per_minute = 0.0025;
        c.storage_cost_per_minute = 0.0005;
        c.media_streaming_cost_per_minute = 0.001;

        let off = provider_cost(&c, &rate("0.020"), &engine(0.0036), false);
        let on = provider_cost(&c, &rate("0.020"), &engine(0.0036), true);

        assert!(
            on.total() > off.total(),
            "{} vs {}",
            on.total(),
            off.total()
        );
        assert_eq!(on.recording, "0.0025".parse::<Decimal>().unwrap());
        assert_eq!(on.storage, "0.0005".parse::<Decimal>().unwrap());
        // Media streaming is charged whether or not the call is recorded.
        assert_eq!(off.media_streaming, "0.001".parse::<Decimal>().unwrap());
        assert_eq!(off.recording, Decimal::ZERO);
        assert_eq!(off.storage, Decimal::ZERO);

        // …and the difference reaches the customer's price, which is the whole point.
        let margin = MarginPolicy::new(
            pricing::usd_from_config(c.min_gross_margin),
            pricing::usd_from_config(c.cost_safety_buffer),
        )
        .unwrap();
        assert!(
            margin.price_per_minute(on.total()).unwrap()
                > margin.price_per_minute(off.total()).unwrap()
        );
    }

    #[test]
    fn a_zero_recording_cost_still_prices_recording_as_free_and_that_is_configuration() {
        // Left at the default, a recorded call is priced identically. That is a
        // COMMERCIAL statement, not a bug — and `Config::from_env` warns loudly at boot
        // when recording is enabled with the cost still at zero.
        let c = cfg();
        assert_eq!(c.recording_cost_per_minute, 0.0);
        let off = provider_cost(&c, &rate("0.020"), &engine(0.0036), false);
        let on = provider_cost(&c, &rate("0.020"), &engine(0.0036), true);
        assert_eq!(on.total(), off.total());
    }

    #[test]
    fn an_org_with_no_settings_row_has_voip_switched_off() {
        // Enabling is always an explicit administrative act; that is what makes the audit
        // trail worth reading.
        let s = OrgSettings::disabled();
        assert!(!s.policy.enabled);
        assert!(!s.recording_enabled);
        assert!(!s.transcription_enabled);
        // …and the conservative consent policy, so a misconfiguration cannot mean
        // "capture silently".
        assert_eq!(s.consent_policy, ConsentPolicy::PressKey);
    }

    #[test]
    fn capture_intent_is_an_intersection_never_a_union() {
        let mut c = cfg();
        let mut org = OrgSettings::disabled();
        let opts = DialOptions {
            destination: "+8613800138000".into(),
            source_language: "it".into(),
            target_language: "zh".into(),
            engine_id: "standard".into(),
            project_id: None,
            caller_id: None,
            record: true,
            transcribe: true,
            ai_analysis: true,
            estimated_minutes: 10,
        };

        // The org has everything off: asking for it changes nothing.
        let i = capture_intent(&c, &org, &opts);
        assert!(!i.recording && !i.transcription && !i.ai_analysis);

        // The org turns it on, the deployment has recording off: still no recording.
        org.recording_enabled = true;
        org.transcription_enabled = true;
        org.ai_analysis_enabled = true;
        c.recording_enabled = false;
        let i = capture_intent(&c, &org, &opts);
        assert!(!i.recording, "the deployment switch wins");
        assert!(i.transcription);
        assert!(i.ai_analysis);

        // Everything on, and the user asked: now it happens.
        c.recording_enabled = true;
        let i = capture_intent(&c, &org, &opts);
        assert!(i.recording && i.transcription && i.ai_analysis);

        // The user declining beats everything permitting.
        let quiet = DialOptions {
            record: false,
            transcribe: false,
            ai_analysis: false,
            ..opts.clone()
        };
        let i = capture_intent(&c, &org, &quiet);
        assert!(i.captures_nothing());
    }

    #[test]
    fn ai_analysis_cannot_survive_without_a_transcript() {
        let c = {
            let mut c = cfg();
            c.recording_enabled = true;
            c
        };
        let org = OrgSettings {
            recording_enabled: true,
            transcription_enabled: false,
            ai_analysis_enabled: true,
            ..OrgSettings::disabled()
        };
        let opts = DialOptions {
            destination: "+8613800138000".into(),
            source_language: "it".into(),
            target_language: "zh".into(),
            engine_id: "standard".into(),
            project_id: None,
            caller_id: None,
            record: true,
            transcribe: true,
            ai_analysis: true,
            estimated_minutes: 10,
        };
        let i = capture_intent(&c, &org, &opts);
        assert!(i.recording);
        assert!(!i.transcription);
        assert!(!i.ai_analysis, "nothing to analyse");
    }

    #[test]
    fn the_hold_horizon_is_clamped_to_what_the_call_can_reach() {
        let mut c = cfg();
        c.max_call_minutes = 30;
        let org = OrgSettings {
            max_call_minutes: 20,
            ..OrgSettings::disabled()
        };
        // Asking for an hour on a 20-minute cap holds 20 minutes, not 60.
        assert_eq!(hold_minutes(60, &org, &c), 20);
        assert_eq!(hold_minutes(5, &org, &c), 5);
        // Nonsense horizons still hold something.
        assert_eq!(hold_minutes(0, &org, &c), 1);
        assert_eq!(hold_minutes(-4, &org, &c), 1);
    }

    #[test]
    fn a_broken_margin_configuration_is_a_misconfiguration_not_a_refused_call() {
        // The difference matters operationally: one call being refused looks like a
        // destination problem; every call being refused is a deployment problem, and it
        // should read as one.
        let mut c = cfg();
        c.min_gross_margin = 1.5;
        let world = DialWorld {
            org: OrgSettings::disabled(),
            global: global_policy(&c),
            rate: Some(rate("0.02")),
            route_validated: false,
            live_user: 0,
            live_org: 0,
            live_global: 0,
            daily_spend: Decimal::ZERO,
        };
        assert_eq!(
            build_quote(&c, &world, &engine(0.0036), CaptureIntent::default(), 10),
            Err(VoipError::Misconfigured(
                "VOIP_MIN_GROSS_MARGIN / VOIP_COST_SAFETY_BUFFER"
            ))
        );
    }

    #[test]
    fn no_rate_refuses_the_quote_rather_than_pricing_at_zero() {
        let c = cfg();
        let world = DialWorld {
            org: OrgSettings::disabled(),
            global: global_policy(&c),
            rate: None,
            route_validated: false,
            live_user: 0,
            live_org: 0,
            live_global: 0,
            daily_spend: Decimal::ZERO,
        };
        assert_eq!(
            build_quote(&c, &world, &engine(0.0036), CaptureIntent::default(), 10),
            Err(VoipError::Refused(FailureReason::RateUnavailable))
        );
    }

    #[test]
    fn error_codes_are_stable_and_machine_readable() {
        assert_eq!(
            VoipError::Refused(FailureReason::InsufficientCredits).code(),
            "insufficient_credits"
        );
        assert_eq!(
            VoipError::BadNumber(E164Error::TooShort).code(),
            "number_too_short"
        );
        assert_eq!(VoipError::Misconfigured("x").code(), "voip_misconfigured");
        // A database error must never become an API payload.
        assert_eq!(VoipError::Storage.code(), "storage_error");
        assert_eq!(
            VoipError::from(sqlx::Error::RowNotFound),
            VoipError::Storage
        );
    }
}
