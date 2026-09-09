//! What a translated phone call costs us, and what we may sell it for (spec 0111, R4–R13).
//!
//! Everything here is `rust_decimal::Decimal`. Not for taste — a per-minute rate like
//! $0.225 has no exact binary form, and `server/src/business/credits.rs` already carries
//! the scar tissue from thirty `f64` additions swallowing a whole credit. R8 is a hard
//! rule and this module is where it is kept.
//!
//! ## Margin, not markup
//!
//! The rest of VoxTranslate prices as `cost × (1 + markup)` (`engine/metadata.rs`). The
//! commercial requirement for VoIP is a **gross margin** floor. These are the same
//! statement seen from two sides:
//!
//! ```text
//! margin = markup / (1 + markup)          markup = margin / (1 - margin)
//! ```
//!
//! so the house default of 25% markup **is** a 20% gross margin. Nothing is being
//! reinvented — a guard is being put over the existing convention, expressed in the units
//! the business actually reasons in, and proven for all inputs rather than for an example.
//!
//! ## Why the tail is rounded UP here and given away elsewhere
//!
//! The meeting meter ([`CreditAccumulator`](crate::business::credits::CreditAccumulator))
//! floors to whole credits and deliberately gives away the sub-cent remainder. Copying
//! that here would breach the margin floor on exactly the calls where it matters most: a
//! five-cent call that forgives $0.0099 has given away a fifth of its revenue and lands
//! near zero margin. So VoIP settles its final tail **upwards**. It is at most one cent,
//! it is on the customer's side of a bill they agreed a per-minute rate for, and it is
//! the difference between the margin invariant holding and being approximately true.

use chrono::{DateTime, Duration, Utc};
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::Decimal;

use crate::telephony::E164;
use crate::voip::state::FailureReason;

/// USD. Named so a signature says what it means.
pub type Usd = Decimal;

/// One org credit is one US cent — the same unit `organizations.credits_balance` uses.
fn usd_per_credit() -> Decimal {
    Decimal::new(1, 2)
}

/// Working precision for money. Six decimal places is a hundredth of a cent: far below
/// anything billable, far above the per-second slices we accumulate.
const MONEY_DP: u32 = 6;

// ---------------------------------------------------------------------------
// Provider cost
// ---------------------------------------------------------------------------

/// Everything the call costs **us**, per minute, itemised.
///
/// Itemised rather than a single number because the reconcile step has to explain a
/// margin miss, and "the call cost more than we thought" is not an explanation. Each
/// field is a per-minute USD amount; one-off costs are amortised by the caller over the
/// expected duration, which is the only honest way to express them as a rate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderCost {
    /// Carrier termination for the destination, plus the provider's per-minute platform
    /// fee. From the rate deck (R5) — never a guess.
    pub telephony: Usd,
    /// STT + translation + TTS for **both** directions (R13), at the chosen tier.
    pub translation: Usd,
    /// Media streaming / forking, where the provider charges for it separately.
    pub media_streaming: Usd,
    /// Call recording, when enabled.
    pub recording: Usd,
    /// Object storage for the recording, amortised over its retention period.
    pub storage: Usd,
    /// AI analysis, taxes, surcharges, allocated number rental — anything else real.
    pub ancillary: Usd,
}

impl ProviderCost {
    /// Total per-minute provider cost.
    pub fn total(&self) -> Usd {
        (self.telephony
            + self.translation
            + self.media_streaming
            + self.recording
            + self.storage
            + self.ancillary)
            .round_dp(MONEY_DP)
    }
}

// ---------------------------------------------------------------------------
// Margin policy
// ---------------------------------------------------------------------------

/// Why a price could not be computed. Distinct from a *call* failure: this is a
/// configuration or input problem and it must stop the call, loudly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PricingError {
    /// `VOIP_MIN_GROSS_MARGIN` outside `[0, 1)`. A margin of 1 means infinite price;
    /// above 1 is nonsense; below 0 means selling at a loss on purpose.
    MarginOutOfRange,
    /// `VOIP_COST_SAFETY_BUFFER` negative — a buffer that reduces the price is not a
    /// safety buffer.
    BufferNegative,
    /// A negative cost. Someone's arithmetic is wrong upstream and pricing it would hide
    /// that.
    NegativeCost,
}

impl PricingError {
    pub fn code(self) -> &'static str {
        match self {
            Self::MarginOutOfRange => "margin_out_of_range",
            Self::BufferNegative => "buffer_negative",
            Self::NegativeCost => "negative_cost",
        }
    }
}

/// The commercial guard: never sell below `min_gross_margin`, having first inflated the
/// observed cost by `safety_buffer` to absorb the difference between the rate deck and
/// the invoice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarginPolicy {
    min_gross_margin: Decimal,
    safety_buffer: Decimal,
}

impl MarginPolicy {
    /// `min_gross_margin` and `safety_buffer` are FRACTIONS (`0.20` = 20%), matching the
    /// repo convention where `*_PERCENT` env vars are divided by 100 before they get here.
    pub fn new(min_gross_margin: Decimal, safety_buffer: Decimal) -> Result<Self, PricingError> {
        if min_gross_margin < Decimal::ZERO || min_gross_margin >= Decimal::ONE {
            return Err(PricingError::MarginOutOfRange);
        }
        if safety_buffer < Decimal::ZERO {
            return Err(PricingError::BufferNegative);
        }
        Ok(Self {
            min_gross_margin,
            safety_buffer,
        })
    }

    pub fn min_gross_margin(&self) -> Decimal {
        self.min_gross_margin
    }

    pub fn safety_buffer(&self) -> Decimal {
        self.safety_buffer
    }

    /// The equivalent markup, for talking to the rest of the codebase in its own units.
    pub fn equivalent_markup(&self) -> Decimal {
        self.min_gross_margin / (Decimal::ONE - self.min_gross_margin)
    }

    /// Customer price per minute for a given provider cost.
    ///
    /// `price = cost × (1 + buffer) / (1 - margin)`, rounded **up** at [`MONEY_DP`].
    /// Rounding up is not a rounding preference: rounding to nearest can land a hair
    /// under the floor, and the floor is the thing this function exists to guarantee.
    pub fn price_per_minute(&self, cost: Usd) -> Result<Usd, PricingError> {
        if cost < Decimal::ZERO {
            return Err(PricingError::NegativeCost);
        }
        let buffered = cost * (Decimal::ONE + self.safety_buffer);
        let price = buffered / (Decimal::ONE - self.min_gross_margin);
        Ok(round_up(price, MONEY_DP))
    }

    /// Whether a realised (charge, cost) pair respects the floor. Used by the
    /// post-call reconcile to raise a margin alarm instead of absorbing a loss (R12).
    ///
    /// A zero charge on a zero cost is fine — a call that cost nothing and was billed
    /// nothing has not broken any promise. A zero charge on a positive cost has.
    pub fn is_respected(&self, charge: Usd, cost: Usd) -> bool {
        match realised_margin(charge, cost) {
            Some(m) => m >= self.min_gross_margin,
            // No charge at all: acceptable only if it also cost us nothing.
            None => cost <= Decimal::ZERO,
        }
    }
}

/// Realised gross margin `(charge - cost) / charge`. `None` when nothing was charged,
/// because a margin on zero revenue is not a number, and returning 0 or 1 there would
/// quietly pass or fail the guard for the wrong reason.
pub fn realised_margin(charge: Usd, cost: Usd) -> Option<Decimal> {
    if charge <= Decimal::ZERO {
        return None;
    }
    Some(((charge - cost) / charge).round_dp(MONEY_DP))
}

/// Round away from zero at `dp` places. `Decimal::round_dp` rounds to nearest, which can
/// land a fraction of a cent under the margin floor.
fn round_up(v: Decimal, dp: u32) -> Decimal {
    let scaled = v * Decimal::from(10u64.pow(dp));
    let ceiled = scaled.ceil();
    (ceiled / Decimal::from(10u64.pow(dp))).round_dp(dp)
}

// ---------------------------------------------------------------------------
// Quote
// ---------------------------------------------------------------------------

/// What the dialer shows before the call and what the reservation is sized from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quote {
    /// Itemised provider cost per minute.
    pub cost: ProviderCost,
    /// Customer price per minute, honouring the margin floor.
    pub price_per_minute: Usd,
    /// Credits to hold for `estimated_minutes` (R9). Always at least 1 for a call that
    /// can connect: holding zero would let an empty pool start a call.
    pub reserve_credits: i32,
    /// What the hold was sized against.
    pub estimated_minutes: i32,
}

/// Build a quote. `estimated_minutes` is the reservation horizon, not a promise about
/// duration — the meter settles against reality afterwards (R10).
pub fn quote(
    policy: &MarginPolicy,
    cost: ProviderCost,
    estimated_minutes: i32,
) -> Result<Quote, PricingError> {
    let minutes = estimated_minutes.max(1);
    let price_per_minute = policy.price_per_minute(cost.total())?;
    let hold_usd = price_per_minute * Decimal::from(minutes);
    let reserve_credits = credits_ceil(hold_usd).max(1);
    Ok(Quote {
        cost,
        price_per_minute,
        reserve_credits,
        estimated_minutes: minutes,
    })
}

/// USD → whole credits, rounded **up**. Used for holds and for the settlement tail; see
/// the module docs for why VoIP rounds up where the meeting meter rounds down.
pub fn credits_ceil(usd: Usd) -> i32 {
    if usd <= Decimal::ZERO {
        return 0;
    }
    (usd / usd_per_credit()).ceil().to_i32().unwrap_or(i32::MAX)
}

/// Whole credits → USD.
pub fn credits_to_usd(credits: i32) -> Usd {
    Decimal::from(credits) * usd_per_credit()
}

/// What a connected call of `seconds` owes at `price_per_minute`, before rounding to
/// credits.
///
/// Multiplies by the elapsed seconds *before* dividing by 60 — the same rule
/// `MinuteRateMeter` documents. Deriving a per-second price first and summing it is the
/// bug that billed a $0.26 minute as 25 credits.
pub fn owed_usd(price_per_minute: Usd, seconds: u64) -> Usd {
    (price_per_minute * Decimal::from(seconds) / Decimal::from(60u64)).round_dp(MONEY_DP)
}

/// Credits due for a completed call of `seconds`. Rounded up, so the realised charge is
/// never below the quoted price (which is what keeps R7 true end to end).
pub fn settle_credits(price_per_minute: Usd, seconds: u64) -> i32 {
    credits_ceil(owed_usd(price_per_minute, seconds))
}

// ---------------------------------------------------------------------------
// Rate deck
// ---------------------------------------------------------------------------

/// One destination rate, as synced from the provider's public pricing (R5, D11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rate {
    /// E.164 digits without `+`. Longest match wins.
    pub prefix: String,
    /// What the provider charges us per minute for this destination.
    pub cost_per_minute: Usd,
    /// Human label, for the dialer and for support ("China Mobile").
    pub description: String,
    /// When this row was synced. Freshness is a correctness property, not metadata.
    pub fetched_at: DateTime<Utc>,
}

/// The synced rate deck.
///
/// Deliberately fails closed: an unknown or stale destination refuses the call rather
/// than dialing it on a guessed price (R5). That is a real availability cost, taken on
/// purpose — the alternative is discovering the price on the invoice.
#[derive(Debug, Clone, Default)]
pub struct RateDeck {
    rates: Vec<Rate>,
}

impl RateDeck {
    pub fn new(mut rates: Vec<Rate>) -> Self {
        // Longest prefix first, so `lookup` is a linear scan that stops at the best match
        // instead of scoring every row.
        rates.sort_by(|a, b| {
            b.prefix
                .len()
                .cmp(&a.prefix.len())
                .then_with(|| a.prefix.cmp(&b.prefix))
        });
        Self { rates }
    }

    pub fn is_empty(&self) -> bool {
        self.rates.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rates.len()
    }

    /// Longest-prefix lookup, with a freshness check.
    ///
    /// `max_age` of `None` disables the staleness rule — for tests and for a deployment
    /// that pins a rate deck deliberately. It is not the default anywhere.
    pub fn lookup(
        &self,
        number: &E164,
        now: DateTime<Utc>,
        max_age: Option<Duration>,
    ) -> Result<&Rate, FailureReason> {
        let digits = number.digits();
        let rate = self
            .rates
            .iter()
            .find(|r| digits.starts_with(&r.prefix))
            .ok_or(FailureReason::RateUnavailable)?;
        if let Some(max_age) = max_age {
            if now.signed_duration_since(rate.fetched_at) > max_age {
                return Err(FailureReason::RateUnavailable);
            }
        }
        Ok(rate)
    }
}

/// Convert a per-minute `f64` from config (every existing `*_COST_PER_MINUTE` is one) into
/// `Decimal` without going through a lossy float path in the money math itself.
pub fn usd_from_config(v: f64) -> Usd {
    Decimal::from_f64(v)
        .unwrap_or(Decimal::ZERO)
        .round_dp(MONEY_DP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn d(s: &str) -> Decimal {
        s.parse().expect("decimal literal")
    }

    fn policy(margin: &str, buffer: &str) -> MarginPolicy {
        MarginPolicy::new(d(margin), d(buffer)).expect("valid policy")
    }

    fn cost_of(total: &str) -> ProviderCost {
        ProviderCost {
            telephony: d(total),
            ..Default::default()
        }
    }

    // ---- the margin identity --------------------------------------------------

    #[test]
    fn twenty_percent_margin_is_the_house_twenty_five_percent_markup() {
        // The whole reason this module is a guard and not a new pricing scheme. If this
        // ever fails, VoIP and the meeting tiers have silently diverged on price.
        let p = policy("0.20", "0");
        assert_eq!(p.equivalent_markup(), d("0.25"));
        // …and the two formulas agree on an actual number.
        let cost = d("0.008");
        assert_eq!(p.price_per_minute(cost).unwrap(), d("0.01"));
        assert_eq!((cost * d("1.25")).round_dp(6), d("0.01"));
    }

    #[test]
    fn a_markup_is_not_a_margin_and_the_difference_is_the_point() {
        // Selling at cost × 1.20 yields a 16.67% margin, not 20%. This is the arithmetic
        // error the requirement calls out by name.
        let cost = d("1.00");
        let naive = cost * d("1.20");
        assert_eq!(
            realised_margin(naive, cost).unwrap().round_dp(4),
            d("0.1667")
        );
        // The correct price for a 20% margin is 1.25, not 1.20.
        assert_eq!(
            policy("0.20", "0").price_per_minute(cost).unwrap(),
            d("1.25")
        );
    }

    // ---- the invariant, as a property (R7) ------------------------------------

    proptest! {
        /// The invariant the business is sold on: for ANY cost, margin and buffer, the
        /// quoted price yields at least the configured gross margin. Examples cannot
        /// establish this; the whole point is that no input escapes it.
        #[test]
        fn quoted_price_never_breaches_the_margin_floor(
            cost_micros in 0i64..50_000_000i64,      // $0 .. $50/min
            margin_bp in 0i64..9_500i64,             // 0% .. 95%
            buffer_bp in 0i64..5_000i64,             // 0% .. 50%
        ) {
            let cost = Decimal::new(cost_micros, 6);
            let policy = MarginPolicy::new(
                Decimal::new(margin_bp, 4),
                Decimal::new(buffer_bp, 4),
            ).unwrap();

            let price = policy.price_per_minute(cost).unwrap();
            prop_assert!(policy.is_respected(price, cost),
                "cost {cost} margin {} buffer {} -> price {price} gives {:?}",
                policy.min_gross_margin(), policy.safety_buffer(),
                realised_margin(price, cost));
        }

        /// The floor must survive the trip through whole credits too. Charging in cents
        /// is where a correct price quietly becomes an incorrect invoice.
        #[test]
        fn settled_credits_never_breach_the_margin_floor(
            cost_micros in 1i64..5_000_000i64,       // $0.000001 .. $5/min
            margin_bp in 0i64..8_000i64,             // 0% .. 80%
            seconds in 1u64..7_200u64,               // 1s .. 2h
        ) {
            let cost_per_minute = Decimal::new(cost_micros, 6);
            let policy = MarginPolicy::new(Decimal::new(margin_bp, 4), Decimal::ZERO).unwrap();
            let price = policy.price_per_minute(cost_per_minute).unwrap();

            let charged = credits_to_usd(settle_credits(price, seconds));
            let incurred = owed_usd(cost_per_minute, seconds);

            prop_assert!(policy.is_respected(charged, incurred),
                "{seconds}s at cost {cost_per_minute}/min, margin {}: charged {charged} \
                 against cost {incurred} -> {:?}",
                policy.min_gross_margin(), realised_margin(charged, incurred));
        }

        /// Rounding up is never *wildly* generous to us either: the settled charge stays
        /// within one credit of what was actually owed. Without this, "round up" could
        /// hide an order-of-magnitude bug and still pass the margin property.
        #[test]
        fn settlement_overcharges_by_less_than_one_credit(
            price_micros in 1i64..10_000_000i64,
            seconds in 1u64..7_200u64,
        ) {
            let price = Decimal::new(price_micros, 6);
            let owed = owed_usd(price, seconds);
            let charged = credits_to_usd(settle_credits(price, seconds));
            prop_assert!(charged >= owed, "charged {charged} < owed {owed}");
            prop_assert!(charged - owed < usd_per_credit(),
                "charged {charged} owed {owed}: overcharge is a whole credit or more");
        }
    }

    // ---- worked examples the property cannot express --------------------------

    #[test]
    fn a_cheap_call_still_clears_the_floor_after_rounding() {
        // The case that killed the "give away the tail" approach: a five-cent call that
        // forgives $0.0099 has given away a fifth of its revenue.
        let p = policy("0.20", "0");
        let cost_per_minute = d("0.04");
        let price = p.price_per_minute(cost_per_minute).unwrap();
        assert_eq!(price, d("0.05"));

        let charged = credits_to_usd(settle_credits(price, 60));
        assert!(p.is_respected(charged, owed_usd(cost_per_minute, 60)));

        // Explicitly: flooring instead would have breached it.
        let floored = credits_to_usd(
            (owed_usd(price, 60) / usd_per_credit())
                .floor()
                .to_i32()
                .unwrap(),
        );
        let _ = floored; // 0.05 lands exactly on a credit here…
                         // …so take a rate that does not: $0.0333/min cost.
        let cost2 = d("0.0333");
        let price2 = p.price_per_minute(cost2).unwrap();
        let owed2 = owed_usd(price2, 37);
        let floored2 = credits_to_usd((owed2 / usd_per_credit()).floor().to_i32().unwrap());
        let ceiled2 = credits_to_usd(settle_credits(price2, 37));
        let incurred2 = owed_usd(cost2, 37);
        assert!(
            !p.is_respected(floored2, incurred2),
            "the floored charge {floored2} was supposed to breach the floor against {incurred2}"
        );
        assert!(p.is_respected(ceiled2, incurred2));
    }

    #[test]
    fn an_expensive_destination_is_priced_the_same_way_as_a_cheap_one() {
        // Satellite-grade rate: no special case, no magic ceiling, same formula.
        let p = policy("0.20", "0.10");
        let price = p.price_per_minute(d("4.00")).unwrap();
        // 4.00 × 1.10 / 0.80 = 5.50
        assert_eq!(price, d("5.5"));
        assert!(p.is_respected(price, d("4.00")));
    }

    #[test]
    fn the_safety_buffer_widens_the_margin_it_does_not_narrow_it() {
        let cost = d("1.00");
        let no_buffer = policy("0.20", "0").price_per_minute(cost).unwrap();
        let buffered = policy("0.20", "0.10").price_per_minute(cost).unwrap();
        assert!(buffered > no_buffer);
        // Against the OBSERVED cost the realised margin is now better than the floor…
        assert!(realised_margin(buffered, cost).unwrap() > d("0.20"));
        // …which is the point: it absorbs the cost being higher than the rate deck said.
        let actual_cost = cost * d("1.10");
        assert!(policy("0.20", "0.10").is_respected(buffered, actual_cost));
    }

    #[test]
    fn zero_cost_is_priced_at_zero_and_that_is_not_a_breach() {
        let p = policy("0.20", "0.10");
        assert_eq!(p.price_per_minute(Decimal::ZERO).unwrap(), Decimal::ZERO);
        assert!(p.is_respected(Decimal::ZERO, Decimal::ZERO));
        // But free on a real cost IS a breach, and must be reported as one.
        assert!(!p.is_respected(Decimal::ZERO, d("0.01")));
    }

    #[test]
    fn both_conversational_directions_are_in_the_cost() {
        // R13. One direction's translation would look profitable and bill half the truth.
        let one_way = d("0.0045");
        let cost = ProviderCost {
            telephony: d("0.012"),
            translation: one_way * Decimal::TWO,
            ..Default::default()
        };
        assert_eq!(cost.total(), d("0.021"));
        let p = policy("0.20", "0");
        let priced_both = p.price_per_minute(cost.total()).unwrap();
        let priced_one = p.price_per_minute(d("0.012") + one_way).unwrap();
        assert!(
            priced_both > priced_one,
            "pricing one direction undercharges every call"
        );
    }

    #[test]
    fn the_itemised_cost_sums_every_component() {
        let cost = ProviderCost {
            telephony: d("0.010"),
            translation: d("0.009"),
            media_streaming: d("0.001"),
            recording: d("0.002"),
            storage: d("0.0005"),
            ancillary: d("0.0015"),
        };
        assert_eq!(cost.total(), d("0.024"));
    }

    // ---- configuration guards -------------------------------------------------

    #[test]
    fn a_nonsensical_policy_is_refused_rather_than_clamped() {
        // Clamping would let a typo in an env var ship a price nobody chose.
        assert_eq!(
            MarginPolicy::new(Decimal::ONE, Decimal::ZERO).unwrap_err(),
            PricingError::MarginOutOfRange
        );
        assert_eq!(
            MarginPolicy::new(d("1.5"), Decimal::ZERO).unwrap_err(),
            PricingError::MarginOutOfRange
        );
        assert_eq!(
            MarginPolicy::new(d("-0.1"), Decimal::ZERO).unwrap_err(),
            PricingError::MarginOutOfRange
        );
        assert_eq!(
            MarginPolicy::new(d("0.2"), d("-0.01")).unwrap_err(),
            PricingError::BufferNegative
        );
        // A zero margin is legal — an internal or promotional deployment may want it —
        // and is therefore a decision, not an accident.
        assert!(MarginPolicy::new(Decimal::ZERO, Decimal::ZERO).is_ok());
    }

    #[test]
    fn a_negative_cost_is_refused() {
        assert_eq!(
            policy("0.2", "0").price_per_minute(d("-0.01")).unwrap_err(),
            PricingError::NegativeCost
        );
    }

    // ---- per-tick math --------------------------------------------------------

    #[test]
    fn a_whole_minute_is_exact_and_ticks_self_correct() {
        // The `MinuteRateMeter` lesson: $0.26/6 has no exact decimal form, so summing
        // six per-tick amounts loses a cent. Multiplying elapsed seconds first does not.
        let price = d("0.26");
        assert_eq!(owed_usd(price, 60), d("0.26"));
        let naive_tick = (price / Decimal::from(6u64)).round_dp(6);
        let summed = naive_tick * Decimal::from(6u64);
        assert!(
            summed < d("0.26"),
            "the naive derivation really does lose money"
        );
        // Intermediate ticks are monotonic and land exactly on the minute.
        let mut last = Decimal::ZERO;
        for s in 1..=60u64 {
            let now = owed_usd(price, s);
            assert!(now >= last, "the meter went backwards at {s}s");
            last = now;
        }
        assert_eq!(last, d("0.26"));
    }

    // ---- quotes and reservations ---------------------------------------------

    #[test]
    fn a_quote_reserves_enough_and_never_zero() {
        let p = policy("0.20", "0.10");
        let q = quote(&p, cost_of("0.04"), 10).unwrap();
        // 0.04 × 1.10 / 0.80 = 0.055/min → 10 min = $0.55 → 55 credits.
        assert_eq!(q.price_per_minute, d("0.055"));
        assert_eq!(q.reserve_credits, 55);
        assert_eq!(q.estimated_minutes, 10);

        // A free destination still holds one credit: a zero hold would let an empty pool
        // start a call, and the pool is what stops runaway spend.
        let free = quote(&p, ProviderCost::default(), 10).unwrap();
        assert_eq!(free.reserve_credits, 1);

        // A nonsense horizon is clamped to one minute rather than reserving nothing.
        assert_eq!(quote(&p, cost_of("0.04"), 0).unwrap().estimated_minutes, 1);
        assert_eq!(quote(&p, cost_of("0.04"), -5).unwrap().estimated_minutes, 1);
    }

    #[test]
    fn credits_conversion_rounds_up_and_round_trips() {
        assert_eq!(credits_ceil(d("0.01")), 1);
        assert_eq!(credits_ceil(d("0.0101")), 2, "any part of a cent is a cent");
        assert_eq!(credits_ceil(d("0.10")), 10);
        assert_eq!(credits_ceil(Decimal::ZERO), 0);
        assert_eq!(credits_ceil(d("-1")), 0);
        assert_eq!(credits_to_usd(55), d("0.55"));
    }

    // ---- rate deck ------------------------------------------------------------

    fn rate(prefix: &str, cost: &str, age_hours: i64) -> Rate {
        Rate {
            prefix: prefix.into(),
            cost_per_minute: d(cost),
            description: prefix.into(),
            fetched_at: Utc::now() - Duration::hours(age_hours),
        }
    }

    fn n(raw: &str) -> E164 {
        E164::parse(raw).expect("test number")
    }

    #[test]
    fn the_longest_matching_prefix_wins() {
        // The case that matters commercially: mobile ranges cost more than the country
        // baseline, and a shortest-match deck would sell every Chinese mobile at the
        // landline rate.
        let deck = RateDeck::new(vec![
            rate("86", "0.020", 1),
            rate("8613", "0.045", 1),
            rate("861380", "0.060", 1),
        ]);
        let now = Utc::now();
        assert_eq!(
            deck.lookup(&n("+8613800138000"), now, None)
                .unwrap()
                .cost_per_minute,
            d("0.060")
        );
        assert_eq!(
            deck.lookup(&n("+8613911112222"), now, None)
                .unwrap()
                .cost_per_minute,
            d("0.045")
        );
        assert_eq!(
            deck.lookup(&n("+862112345678"), now, None)
                .unwrap()
                .cost_per_minute,
            d("0.020")
        );
    }

    #[test]
    fn an_unknown_destination_refuses_the_call_it_does_not_guess() {
        let deck = RateDeck::new(vec![rate("39", "0.010", 1)]);
        assert_eq!(
            deck.lookup(&n("+8613800138000"), Utc::now(), None)
                .unwrap_err(),
            FailureReason::RateUnavailable
        );
    }

    #[test]
    fn an_empty_deck_refuses_everything() {
        // A deployment whose sync has never run must not dial at all. Failing open here
        // means discovering the price on the invoice.
        let deck = RateDeck::default();
        assert!(deck.is_empty());
        assert_eq!(
            deck.lookup(&n("+393201234567"), Utc::now(), None)
                .unwrap_err(),
            FailureReason::RateUnavailable
        );
    }

    #[test]
    fn a_stale_rate_is_treated_as_no_rate() {
        let deck = RateDeck::new(vec![rate("39", "0.010", 48)]);
        let now = Utc::now();
        assert_eq!(
            deck.lookup(&n("+393201234567"), now, Some(Duration::hours(24)))
                .unwrap_err(),
            FailureReason::RateUnavailable
        );
        // Inside the window it is fine, and with the check disabled it is always fine.
        assert!(deck
            .lookup(&n("+393201234567"), now, Some(Duration::hours(72)))
            .is_ok());
        assert!(deck.lookup(&n("+393201234567"), now, None).is_ok());
    }

    #[test]
    fn ties_between_equal_length_prefixes_are_deterministic() {
        // Two rows of the same length must not resolve by insertion order, or the same
        // number prices differently after a re-sync.
        let a = RateDeck::new(vec![rate("391", "0.010", 1), rate("392", "0.020", 1)]);
        let b = RateDeck::new(vec![rate("392", "0.020", 1), rate("391", "0.010", 1)]);
        let now = Utc::now();
        let num = n("+393201234567");
        assert_eq!(
            a.lookup(&num, now, None).map(|r| r.prefix.clone()),
            b.lookup(&num, now, None).map(|r| r.prefix.clone())
        );
    }

    #[test]
    fn config_floats_become_decimals_without_dragging_binary_error_in() {
        // 0.0036 is not representable in binary; going through Decimal at the boundary is
        // what keeps the rest of this module exact.
        assert_eq!(usd_from_config(0.0036), d("0.0036"));
        assert_eq!(usd_from_config(0.008), d("0.008"));
        assert_eq!(usd_from_config(0.0), Decimal::ZERO);
    }
}
