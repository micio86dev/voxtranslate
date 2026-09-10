//! Everything that must be true before a telephone rings (spec 0111, R1–R6, R22).
//!
//! Every check here happens **before** the provider is contacted. That is the whole
//! design: once a call is dialed the money is committed and the damage is done, so a
//! control that fires after the fact is a report, not a control.
//!
//! It is a pure function over a snapshot of the world. The database work — counting live
//! calls, summing today's spend, looking up the rate — happens in the caller and is
//! handed in, so every branch is exhaustively testable without a database, and so the
//! ordering of the refusals is visible in one place instead of scattered through a
//! handler.
//!
//! ## Why the order matters
//!
//! The first failing check is the one reported, so the order decides what the user is
//! told. It runs from "you may not use this feature at all" through "not to that number"
//! to "not right now" — which is both the most useful order for a human and the one that
//! leaks the least: someone probing for expensive destinations learns their organization
//! is not entitled before they learn anything about our rate deck.

use rust_decimal::Decimal;

use crate::telephony::E164;
use crate::voip::pricing::Rate;
use crate::voip::state::FailureReason;

/// Per-organization VoIP policy, as stored in `voip_org_settings`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrgPolicy {
    pub enabled: bool,
    /// ISO 3166-1 alpha-2, uppercase. **Empty means "no allow-list"**, not "no countries".
    /// An accidentally-empty array must not ban the world.
    pub allowed_countries: Vec<String>,
    pub blocked_countries: Vec<String>,
    pub allow_international: bool,
    pub max_concurrent_per_user: i32,
    pub max_concurrent_per_org: i32,
    /// Credits. `None` = no org ceiling beyond the global one.
    pub monthly_spend_limit_credits: Option<i32>,
}

impl Default for OrgPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            allowed_countries: Vec::new(),
            blocked_countries: Vec::new(),
            allow_international: true,
            max_concurrent_per_user: 2,
            max_concurrent_per_org: 10,
            monthly_spend_limit_credits: None,
        }
    }
}

/// Deployment-wide limits, from [`crate::config::VoipConfig`].
#[derive(Debug, Clone, PartialEq)]
pub struct GlobalPolicy {
    pub rollout_stage: RolloutStage,
    pub require_eu_processing: bool,
    /// USD per minute. A destination above this is refused before dialing.
    pub max_destination_rate: Decimal,
    pub daily_provider_spend_limit: Decimal,
    pub max_concurrent_global: i32,
    pub allow_international: bool,
    pub allowed_countries: Vec<String>,
    pub blocked_countries: Vec<String>,
    pub china_enabled: bool,
    pub china_require_validated_route: bool,
}

/// Controlled rollout (spec 0111 §7). Reversible at every step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolloutStage {
    /// Nobody. The kill switch.
    Disabled,
    /// Internal organizations only.
    Internal,
    /// Explicitly listed beta organizations.
    Beta,
    /// Any organization with a live Business/Enterprise subscription.
    Business,
    /// Same as Business today; kept distinct so "we went GA" is a recorded decision
    /// rather than an inference from config.
    Ga,
}

impl RolloutStage {
    pub fn parse(raw: &str) -> Self {
        match raw {
            "internal" => Self::Internal,
            "beta" => Self::Beta,
            "business" => Self::Business,
            "ga" => Self::Ga,
            // Anything unrecognised is OFF. A typo in an env var must not open a paid
            // feature to everyone.
            _ => Self::Disabled,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Internal => "internal",
            Self::Beta => "beta",
            Self::Business => "business",
            Self::Ga => "ga",
        }
    }
}

/// A snapshot of everything the decision depends on.
#[derive(Debug, Clone)]
pub struct DialContext<'a> {
    pub destination: &'a E164,
    pub org: &'a OrgPolicy,
    pub global: &'a GlobalPolicy,
    /// A live, unlapsed Business/Enterprise subscription — `credits::org_subscription_active`.
    pub subscription_active: bool,
    /// Whether this org is on the beta/internal list for the current stage.
    pub org_in_rollout_list: bool,
    /// Whether the chosen translation tier can guarantee EU-only processing.
    ///
    /// **False for every tier today** — see `docs/voip-data-flow.md`. Passed in rather
    /// than looked up here so the gate has one meaning and one place.
    pub tier_supports_eu_only: bool,
    /// Whether the telephony provider itself is EU-anchored.
    pub provider_eu_telephony: bool,
    /// The destination rate, already looked up and freshness-checked. `None` means no
    /// usable rate — which refuses the call (R5) rather than guessing a price.
    pub rate: Option<&'a Rate>,
    /// A recorded, in-country, unexpired route validation exists for this destination.
    pub route_validated: bool,
    pub live_calls_user: i32,
    pub live_calls_org: i32,
    pub live_calls_global: i32,
    /// Provider cost incurred today, USD.
    pub daily_spend_usd: Decimal,
    /// The org's home country, for deciding what counts as international.
    pub org_country: Option<&'a str>,
}

/// Run every pre-dial check. `Ok(())` means the call may be placed.
pub fn check(ctx: &DialContext<'_>) -> Result<(), FailureReason> {
    entitlement(ctx)?;
    destination(ctx)?;
    china(ctx)?;
    eu_processing(ctx)?;
    price(ctx)?;
    capacity(ctx)?;
    Ok(())
}

/// May this organization use the feature at all?
fn entitlement(ctx: &DialContext<'_>) -> Result<(), FailureReason> {
    let allowed_by_stage = match ctx.global.rollout_stage {
        RolloutStage::Disabled => false,
        RolloutStage::Internal | RolloutStage::Beta => ctx.org_in_rollout_list,
        RolloutStage::Business | RolloutStage::Ga => ctx.subscription_active,
    };
    // The org's own switch is required in every stage, including GA. Being on the beta
    // list is permission for the org to turn it on, never a substitute for having done so.
    if !allowed_by_stage || !ctx.org.enabled {
        return Err(FailureReason::DestinationNotAllowed);
    }
    // Even a listed beta org needs a live subscription: this is a Business/Enterprise
    // feature and it spends real money.
    if !ctx.subscription_active {
        return Err(FailureReason::DestinationNotAllowed);
    }
    Ok(())
}

/// May we call THAT number?
fn destination(ctx: &DialContext<'_>) -> Result<(), FailureReason> {
    let region = ctx.destination.region();

    // Block-lists win over allow-lists, always. Two settings that disagree must resolve
    // the safe way, and an operator who blocked a country meant it.
    if contains(&ctx.global.blocked_countries, region)
        || contains(&ctx.org.blocked_countries, region)
    {
        return Err(FailureReason::DestinationNotAllowed);
    }

    // An EMPTY allow-list means "no allow-list configured". Reading it as "no countries
    // permitted" would turn a fresh install into a feature that silently never works.
    if !ctx.global.allowed_countries.is_empty() && !contains(&ctx.global.allowed_countries, region)
    {
        return Err(FailureReason::DestinationNotAllowed);
    }
    if !ctx.org.allowed_countries.is_empty() && !contains(&ctx.org.allowed_countries, region) {
        return Err(FailureReason::DestinationNotAllowed);
    }

    // International is only meaningful relative to a home country. Without one we cannot
    // tell, and refusing everything would be worse than the switch not applying — so the
    // switch applies where it can be evaluated, and the country lists cover the rest.
    if let Some(home) = ctx.org_country {
        let is_international = !home.eq_ignore_ascii_case(region);
        if is_international && (!ctx.global.allow_international || !ctx.org.allow_international) {
            return Err(FailureReason::DestinationNotAllowed);
        }
    }

    Ok(())
}

/// The China gate (D10). Off by default and evidence-driven.
fn china(ctx: &DialContext<'_>) -> Result<(), FailureReason> {
    if ctx.destination.region() != "CN" {
        return Ok(());
    }
    if !ctx.global.china_enabled {
        return Err(FailureReason::DestinationNotAllowed);
    }
    // The master switch alone is not evidence. A recorded, physically-in-country,
    // unexpired validation is — see docs/voip-china-validation.md.
    if ctx.global.china_require_validated_route && !ctx.route_validated {
        return Err(FailureReason::DestinationNotAllowed);
    }
    Ok(())
}

/// EU-only processing (R22).
fn eu_processing(ctx: &DialContext<'_>) -> Result<(), FailureReason> {
    if !ctx.global.require_eu_processing {
        return Ok(());
    }
    // BOTH halves. Telephony in Frankfurt with translation in Singapore is not EU-only
    // processing, and treating the provider's region as sufficient is the exact mistake
    // the GDPR review found being made about the product as a whole.
    if !ctx.provider_eu_telephony || !ctx.tier_supports_eu_only {
        return Err(FailureReason::EuProcessingUnavailable);
    }
    Ok(())
}

/// Do we know what it costs, and can we afford to find out?
fn price(ctx: &DialContext<'_>) -> Result<(), FailureReason> {
    // Fail closed. No fresh rate ⇒ no call. Dialing on a guessed price means discovering
    // it on the invoice.
    let Some(rate) = ctx.rate else {
        return Err(FailureReason::RateUnavailable);
    };
    if rate.cost_per_minute > ctx.global.max_destination_rate {
        return Err(FailureReason::DestinationTooExpensive);
    }
    if ctx.daily_spend_usd >= ctx.global.daily_provider_spend_limit {
        return Err(FailureReason::DestinationTooExpensive);
    }
    Ok(())
}

/// Is there room right now? (R6)
fn capacity(ctx: &DialContext<'_>) -> Result<(), FailureReason> {
    if ctx.live_calls_user >= ctx.org.max_concurrent_per_user
        || ctx.live_calls_org >= ctx.org.max_concurrent_per_org
        || ctx.live_calls_global >= ctx.global.max_concurrent_global
    {
        return Err(FailureReason::ConcurrencyLimit);
    }
    Ok(())
}

/// Case-insensitive membership, so a country list written in lower case still works.
fn contains(list: &[String], region: &str) -> bool {
    list.iter().any(|c| c.eq_ignore_ascii_case(region))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn n(raw: &str) -> E164 {
        E164::parse(raw).expect("test number")
    }

    fn rate(cost: &str) -> Rate {
        Rate {
            prefix: "39".into(),
            cost_per_minute: cost.parse().unwrap(),
            description: "test".into(),
            fetched_at: Utc::now(),
        }
    }

    fn global() -> GlobalPolicy {
        GlobalPolicy {
            rollout_stage: RolloutStage::Ga,
            require_eu_processing: false,
            max_destination_rate: "1.00".parse().unwrap(),
            daily_provider_spend_limit: "50".parse().unwrap(),
            max_concurrent_global: 50,
            allow_international: true,
            allowed_countries: Vec::new(),
            blocked_countries: Vec::new(),
            china_enabled: false,
            china_require_validated_route: true,
        }
    }

    fn org() -> OrgPolicy {
        OrgPolicy {
            enabled: true,
            ..Default::default()
        }
    }

    /// A context that passes every check, so each test can break exactly one thing.
    struct Fx {
        dest: E164,
        org: OrgPolicy,
        global: GlobalPolicy,
        rate: Rate,
    }

    impl Fx {
        fn new() -> Self {
            Self {
                dest: n("+393201234567"),
                org: org(),
                global: global(),
                rate: rate("0.02"),
            }
        }

        fn ctx(&self) -> DialContext<'_> {
            DialContext {
                destination: &self.dest,
                org: &self.org,
                global: &self.global,
                subscription_active: true,
                org_in_rollout_list: false,
                tier_supports_eu_only: false,
                provider_eu_telephony: true,
                rate: Some(&self.rate),
                route_validated: false,
                live_calls_user: 0,
                live_calls_org: 0,
                live_calls_global: 0,
                daily_spend_usd: Decimal::ZERO,
                org_country: Some("IT"),
            }
        }
    }

    // ---- the baseline ---------------------------------------------------------

    #[test]
    fn a_well_formed_call_from_an_entitled_org_is_allowed() {
        assert_eq!(check(&Fx::new().ctx()), Ok(()));
    }

    // ---- entitlement (R1) -----------------------------------------------------

    #[test]
    fn the_kill_switch_stops_everyone_including_paying_customers() {
        let mut f = Fx::new();
        f.global.rollout_stage = RolloutStage::Disabled;
        let mut c = f.ctx();
        c.org_in_rollout_list = true;
        assert_eq!(check(&c), Err(FailureReason::DestinationNotAllowed));
    }

    #[test]
    fn beta_admits_only_listed_organizations() {
        let mut f = Fx::new();
        f.global.rollout_stage = RolloutStage::Beta;

        let mut listed = f.ctx();
        listed.org_in_rollout_list = true;
        assert_eq!(check(&listed), Ok(()));

        let unlisted = f.ctx(); // org_in_rollout_list defaults to false
        assert_eq!(check(&unlisted), Err(FailureReason::DestinationNotAllowed));
    }

    #[test]
    fn a_listed_beta_org_still_needs_a_live_subscription() {
        // Being on the beta list is permission to try it, not a free tier. It spends real
        // money on every call.
        let mut f = Fx::new();
        f.global.rollout_stage = RolloutStage::Beta;
        let mut c = f.ctx();
        c.org_in_rollout_list = true;
        c.subscription_active = false;
        assert_eq!(check(&c), Err(FailureReason::DestinationNotAllowed));
    }

    #[test]
    fn the_orgs_own_switch_is_required_in_every_stage() {
        // Including GA, and including for a listed beta org. Rollout decides who MAY turn
        // it on; it never turns it on for them.
        for stage in [
            RolloutStage::Internal,
            RolloutStage::Beta,
            RolloutStage::Business,
            RolloutStage::Ga,
        ] {
            let mut f = Fx::new();
            f.global.rollout_stage = stage;
            f.org.enabled = false;
            let mut c = f.ctx();
            c.org_in_rollout_list = true;
            assert_eq!(
                check(&c),
                Err(FailureReason::DestinationNotAllowed),
                "{}",
                stage.as_str()
            );
        }
    }

    #[test]
    fn a_lapsed_subscription_stops_new_calls() {
        let f = Fx::new();
        let mut c = f.ctx();
        c.subscription_active = false;
        assert_eq!(check(&c), Err(FailureReason::DestinationNotAllowed));
    }

    #[test]
    fn an_unrecognised_rollout_stage_is_off_not_on() {
        // A typo in an env var must not open a paid feature to the world.
        assert_eq!(RolloutStage::parse("Ga"), RolloutStage::Disabled);
        assert_eq!(RolloutStage::parse(""), RolloutStage::Disabled);
        assert_eq!(RolloutStage::parse("everyone"), RolloutStage::Disabled);
        assert_eq!(RolloutStage::parse("ga"), RolloutStage::Ga);
        for s in [
            RolloutStage::Disabled,
            RolloutStage::Internal,
            RolloutStage::Beta,
            RolloutStage::Business,
            RolloutStage::Ga,
        ] {
            assert_eq!(RolloutStage::parse(s.as_str()), s);
        }
    }

    // ---- destination policy (R3) ----------------------------------------------

    #[test]
    fn a_blocked_country_is_refused_at_either_level() {
        let mut f = Fx::new();
        f.global.blocked_countries = vec!["IT".into()];
        assert_eq!(check(&f.ctx()), Err(FailureReason::DestinationNotAllowed));

        let mut f = Fx::new();
        f.org.blocked_countries = vec!["it".into()]; // case must not matter
        assert_eq!(check(&f.ctx()), Err(FailureReason::DestinationNotAllowed));
    }

    #[test]
    fn a_block_beats_an_allow_when_the_two_disagree() {
        // An operator who blocked a country meant it. Resolving the conflict the other way
        // would let an allow-list quietly override a deliberate block.
        let mut f = Fx::new();
        f.org.allowed_countries = vec!["IT".into()];
        f.global.blocked_countries = vec!["IT".into()];
        assert_eq!(check(&f.ctx()), Err(FailureReason::DestinationNotAllowed));
    }

    #[test]
    fn an_empty_allow_list_means_no_allow_list_not_no_countries() {
        // Reading it the other way turns a fresh install into a feature that never works
        // and gives no reason why.
        let f = Fx::new();
        assert!(f.org.allowed_countries.is_empty());
        assert!(f.global.allowed_countries.is_empty());
        assert_eq!(check(&f.ctx()), Ok(()));
    }

    #[test]
    fn a_non_empty_allow_list_excludes_everything_else() {
        let mut f = Fx::new();
        f.org.allowed_countries = vec!["DE".into(), "FR".into()];
        assert_eq!(check(&f.ctx()), Err(FailureReason::DestinationNotAllowed));
        f.org.allowed_countries.push("IT".into());
        assert_eq!(check(&f.ctx()), Ok(()));
    }

    #[test]
    fn an_allow_list_of_the_united_states_does_not_admit_the_caribbean() {
        // The classic toll-fraud play: premium-rate Caribbean ranges answer to +1 too, so
        // a policy that resolves +1 to "US" wholesale is an open door.
        let mut f = Fx::new();
        f.dest = n("+12685551234"); // Antigua
        f.rate = rate("0.02");
        f.org.allowed_countries = vec!["US".into()];
        f.org.allow_international = true;
        f.global.allow_international = true;
        assert_eq!(check(&f.ctx()), Err(FailureReason::DestinationNotAllowed));

        // …and an actual US number is fine.
        f.dest = n("+12125551234");
        assert_eq!(check(&f.ctx()), Ok(()));
    }

    #[test]
    fn turning_international_off_keeps_domestic_calls_working() {
        let mut f = Fx::new();
        f.org.allow_international = false;

        // Same country as the org: allowed.
        assert_eq!(check(&f.ctx()), Ok(()));

        // Abroad: refused.
        f.dest = n("+4915112345678");
        assert_eq!(check(&f.ctx()), Err(FailureReason::DestinationNotAllowed));
    }

    #[test]
    fn without_a_home_country_the_international_switch_cannot_apply() {
        // We cannot tell what "international" means, and refusing everything would be
        // worse than the switch not applying — the country lists cover that case instead.
        let mut f = Fx::new();
        f.global.allow_international = false;
        let mut c = f.ctx();
        c.org_country = None;
        assert_eq!(check(&c), Ok(()));
    }

    // ---- China (D10) ----------------------------------------------------------

    #[test]
    fn china_is_closed_by_default() {
        let mut f = Fx::new();
        f.dest = n("+8613800138000");
        f.rate = rate("0.03");
        assert!(!f.global.china_enabled);
        assert_eq!(check(&f.ctx()), Err(FailureReason::DestinationNotAllowed));
    }

    #[test]
    fn enabling_china_is_not_enough_without_recorded_evidence() {
        // The master switch exists to be able to turn China OFF quickly. It is not a
        // shortcut to turning it on.
        let mut f = Fx::new();
        f.dest = n("+8613800138000");
        f.rate = rate("0.03");
        f.global.china_enabled = true;
        assert_eq!(check(&f.ctx()), Err(FailureReason::DestinationNotAllowed));

        let mut c = f.ctx();
        c.route_validated = true;
        assert_eq!(check(&c), Ok(()));
    }

    #[test]
    fn the_validation_requirement_can_be_waived_deliberately() {
        // For the staging window in which the validation itself is performed.
        let mut f = Fx::new();
        f.dest = n("+8613800138000");
        f.rate = rate("0.03");
        f.global.china_enabled = true;
        f.global.china_require_validated_route = false;
        assert_eq!(check(&f.ctx()), Ok(()));
    }

    #[test]
    fn the_china_gate_does_not_touch_other_destinations() {
        let mut f = Fx::new();
        f.global.china_enabled = false;
        assert_eq!(check(&f.ctx()), Ok(()), "Italy is unaffected");
    }

    // ---- EU processing (R22) --------------------------------------------------

    #[test]
    fn eu_only_mode_refuses_every_call_today_and_that_is_correct() {
        // No tier can satisfy it: Standard reaches Alibaba in Singapore and Groq handles
        // text in the US on every tier. A flag that claimed otherwise would be worse than
        // no flag. See docs/voip-data-flow.md.
        let mut f = Fx::new();
        f.global.require_eu_processing = true;
        let c = f.ctx();
        assert!(!c.tier_supports_eu_only);
        assert_eq!(check(&c), Err(FailureReason::EuProcessingUnavailable));
    }

    #[test]
    fn eu_telephony_alone_does_not_satisfy_eu_only_processing() {
        // THE mistake this gate exists to prevent: a Frankfurt anchorsite says nothing
        // about where the translation happens.
        let mut f = Fx::new();
        f.global.require_eu_processing = true;
        let mut c = f.ctx();
        c.provider_eu_telephony = true;
        c.tier_supports_eu_only = false;
        assert_eq!(check(&c), Err(FailureReason::EuProcessingUnavailable));
    }

    #[test]
    fn an_eu_tier_on_a_non_eu_provider_is_also_refused() {
        let mut f = Fx::new();
        f.global.require_eu_processing = true;
        let mut c = f.ctx();
        c.tier_supports_eu_only = true;
        c.provider_eu_telephony = false;
        assert_eq!(check(&c), Err(FailureReason::EuProcessingUnavailable));
    }

    #[test]
    fn eu_only_mode_allows_a_call_when_both_halves_are_genuinely_eu() {
        // The state the two pending migrations are for.
        let mut f = Fx::new();
        f.global.require_eu_processing = true;
        let mut c = f.ctx();
        c.tier_supports_eu_only = true;
        c.provider_eu_telephony = true;
        assert_eq!(check(&c), Ok(()));
    }

    #[test]
    fn with_the_flag_off_a_non_eu_tier_proceeds() {
        let f = Fx::new();
        assert!(!f.global.require_eu_processing);
        assert_eq!(check(&f.ctx()), Ok(()));
    }

    // ---- price (R4, R5) -------------------------------------------------------

    #[test]
    fn no_rate_means_no_call() {
        let f = Fx::new();
        let mut c = f.ctx();
        c.rate = None;
        assert_eq!(check(&c), Err(FailureReason::RateUnavailable));
    }

    #[test]
    fn a_destination_above_the_ceiling_is_refused_before_dialing() {
        // The single most effective anti-toll-fraud control there is.
        let mut f = Fx::new();
        f.rate = rate("2.50");
        assert_eq!(check(&f.ctx()), Err(FailureReason::DestinationTooExpensive));

        // Exactly at the ceiling is allowed; the check is strictly greater.
        f.rate = rate("1.00");
        assert_eq!(check(&f.ctx()), Ok(()));
    }

    #[test]
    fn the_daily_spend_limit_stops_dialing_once_reached() {
        let f = Fx::new();
        let mut c = f.ctx();
        c.daily_spend_usd = "50".parse().unwrap();
        assert_eq!(check(&c), Err(FailureReason::DestinationTooExpensive));

        c.daily_spend_usd = "49.99".parse().unwrap();
        assert_eq!(check(&c), Ok(()));
    }

    // ---- capacity (R6) --------------------------------------------------------

    #[test]
    fn each_concurrency_cap_is_enforced_independently() {
        let f = Fx::new();
        for (user, org_live, global_live) in [(2, 0, 0), (0, 10, 0), (0, 0, 50)] {
            let mut c = f.ctx();
            c.live_calls_user = user;
            c.live_calls_org = org_live;
            c.live_calls_global = global_live;
            assert_eq!(
                check(&c),
                Err(FailureReason::ConcurrencyLimit),
                "user={user} org={org_live} global={global_live}"
            );
        }
    }

    #[test]
    fn being_one_below_every_cap_is_allowed() {
        let f = Fx::new();
        let mut c = f.ctx();
        c.live_calls_user = 1;
        c.live_calls_org = 9;
        c.live_calls_global = 49;
        assert_eq!(check(&c), Ok(()));
    }

    // ---- ordering -------------------------------------------------------------

    #[test]
    fn entitlement_is_reported_before_anything_about_our_rates() {
        // Someone probing for expensive destinations learns their org is not entitled
        // before they learn anything about the rate deck.
        let mut f = Fx::new();
        f.org.enabled = false;
        f.rate = rate("9.99");
        let mut c = f.ctx();
        c.rate = None;
        c.live_calls_global = 9999;
        assert_eq!(
            check(&c),
            Err(FailureReason::DestinationNotAllowed),
            "entitlement must win over price and capacity"
        );
    }

    #[test]
    fn a_forbidden_destination_is_reported_before_its_price() {
        let mut f = Fx::new();
        f.org.blocked_countries = vec!["IT".into()];
        f.rate = rate("9.99");
        assert_eq!(check(&f.ctx()), Err(FailureReason::DestinationNotAllowed));
    }

    #[test]
    fn the_eu_gate_is_reported_before_capacity() {
        // "We cannot process this in the EU" is actionable; "try again later" is not, and
        // reporting the transient reason would hide the permanent one.
        let mut f = Fx::new();
        f.global.require_eu_processing = true;
        let mut c = f.ctx();
        c.live_calls_global = 9999;
        assert_eq!(check(&c), Err(FailureReason::EuProcessingUnavailable));
    }

    #[test]
    fn every_refusal_carries_a_reason_the_dashboard_can_localise() {
        // These strings are API surface: the UI translates them, it does not parse prose.
        for r in [
            FailureReason::DestinationNotAllowed,
            FailureReason::DestinationTooExpensive,
            FailureReason::RateUnavailable,
            FailureReason::EuProcessingUnavailable,
            FailureReason::ConcurrencyLimit,
        ] {
            assert!(!r.as_str().is_empty());
            assert!(r
                .as_str()
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '_'));
        }
    }
}
