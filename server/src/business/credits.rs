//! Org credit pool: atomic spend + grant (spec 0106).
//!
//! Mirrors the consumer ledger pattern (`billing.rs`) — `SELECT … FOR UPDATE`,
//! update balance, append a ledger row — but against `organizations.credits_balance`
//! (INTEGER) and `organization_credits_transactions`. Org credits and consumer
//! DECIMAL credits never mix.

use chrono::{DateTime, Utc};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use uuid::Uuid;

use crate::db::Pool;

/// Outcome of an attempted org-credit deduction.
pub enum OrgCharge {
    /// Charged; `balance_after` is the org's new balance.
    Charged { balance_after: i32 },
    /// Not enough credits; nothing was deducted.
    Insufficient { balance: i32, required: i32 },
}

/// Deduct `amount` (≥ 0) credits from the org pool, writing a signed (negative)
/// ledger row, atomically. Returns [`OrgCharge::Insufficient`] (no-op) when the
/// balance can't cover it.
///
/// Opens and commits its own transaction; use [`deduct_org_credits_tx`] to fold the
/// deduction into a caller-provided transaction (e.g. so a session-record INSERT and
/// its charge commit or roll back together).
pub async fn deduct_org_credits(
    pool: &Pool,
    org_id: Uuid,
    amount: i32,
    kind: &str,
    session_id: Option<Uuid>,
    actor_id: Option<Uuid>,
    description: &str,
) -> Result<OrgCharge, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let charge = deduct_org_credits_tx(
        &mut tx,
        org_id,
        amount,
        kind,
        session_id,
        actor_id,
        description,
    )
    .await?;
    // On Insufficient the row was locked (FOR UPDATE) but nothing was written, so the
    // commit is a harmless no-op releasing the lock; on Charged it persists the writes.
    tx.commit().await?;
    Ok(charge)
}

/// Same balance-check + UPDATE + ledger-INSERT as [`deduct_org_credits`], but running
/// inside a caller-provided transaction so the deduction is atomic with the caller's
/// other writes. The caller owns commit/rollback: this function performs NO commit and
/// only rolls nothing back itself. On [`OrgCharge::Insufficient`] no rows are written
/// (the `FOR UPDATE` lock is held until the caller's tx ends), so the caller may still
/// commit its other work if it chooses (e.g. record a broadcast without a charge).
pub async fn deduct_org_credits_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org_id: Uuid,
    amount: i32,
    kind: &str,
    session_id: Option<Uuid>,
    actor_id: Option<Uuid>,
    description: &str,
) -> Result<OrgCharge, sqlx::Error> {
    let balance: i32 =
        sqlx::query_scalar("SELECT credits_balance FROM organizations WHERE id = $1 FOR UPDATE")
            .bind(org_id)
            .fetch_one(&mut **tx)
            .await?;
    if amount > 0 && balance < amount {
        return Ok(OrgCharge::Insufficient {
            balance,
            required: amount,
        });
    }
    let new_balance = balance - amount;
    sqlx::query("UPDATE organizations SET credits_balance = $2, updated_at = now() WHERE id = $1")
        .bind(org_id)
        .bind(new_balance)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "INSERT INTO organization_credits_transactions
            (org_id, amount, type, description, session_id, actor_id)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(org_id)
    .bind(-amount)
    .bind(kind)
    .bind(description)
    .bind(session_id)
    .bind(actor_id)
    .execute(&mut **tx)
    .await?;
    Ok(OrgCharge::Charged {
        balance_after: new_balance,
    })
}

/// Add `amount` (≥ 0) credits to the org pool (purchase / subscription grant),
/// writing a signed (positive) ledger row, atomically. Returns the new balance.
pub async fn add_org_credits(
    pool: &Pool,
    org_id: Uuid,
    amount: i32,
    kind: &str,
    description: &str,
    stripe_payment_intent_id: Option<&str>,
) -> Result<i32, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let balance: i32 =
        sqlx::query_scalar("SELECT credits_balance FROM organizations WHERE id = $1 FOR UPDATE")
            .bind(org_id)
            .fetch_one(&mut *tx)
            .await?;
    let new_balance = balance + amount;
    sqlx::query("UPDATE organizations SET credits_balance = $2, updated_at = now() WHERE id = $1")
        .bind(org_id)
        .bind(new_balance)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO organization_credits_transactions
            (org_id, amount, type, description, stripe_payment_intent_id)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(org_id)
    .bind(amount)
    .bind(kind)
    .bind(description)
    .bind(stripe_payment_intent_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(new_balance)
}

/// The single definition of "this org's subscription is live right now".
///
/// A gifted subscription has no Stripe behind it, so nothing ever flips its
/// `subscription_status` to 'canceled' when the paid period runs out — the row
/// says 'active' forever and only the date tells the truth. Every place that
/// gates on a subscription, and every payload that reports one, must apply the
/// same rule, so it is written once here. Column names are unqualified so the
/// fragment drops into a join as-is (`organization_members` has neither column).
pub const SUBSCRIPTION_ACTIVE_SQL: &str = "(subscription_status = 'active' \
     AND (current_period_end IS NULL OR current_period_end > now()))";

/// True when the org has a live, unlapsed subscription: status `active` AND its
/// period hasn't ended. A NULL `current_period_end` counts as active — a
/// freshly-checked-out Stripe sub has no period end until its first invoice
/// webhook lands. Admin-gifted subscriptions always set a period end, so this is
/// what makes a gift auto-expire (there is no background scheduler to flip the
/// status). Missing org → not active.
pub async fn org_subscription_active(pool: &Pool, org_id: Uuid) -> Result<bool, sqlx::Error> {
    let active: Option<bool> = sqlx::query_scalar(&format!(
        "SELECT {SUBSCRIPTION_ACTIVE_SQL} FROM organizations WHERE id = $1"
    ))
    .bind(org_id)
    .fetch_optional(pool)
    .await?;
    Ok(active.unwrap_or(false))
}

/// Outcome of an admin gift-subscription action (`POST /api/admin/org/gift-subscription`).
pub enum GiftOutcome {
    /// Applied. The org is on `plan`, active until `current_period_end`, and its
    /// pool grew by `credits_granted` to `credits_balance`.
    Gifted {
        credits_balance: i32,
        credits_granted: i32,
        current_period_end: DateTime<Utc>,
    },
    /// No such organization.
    NotFound,
    /// The org has a live Stripe subscription — gifting would desync billing.
    ManagedByStripe,
}

/// Admin gift: put an org on `plan` with an ACTIVE, non-renewing subscription for
/// `months` months and top its credit pool up by `credits`, all in one
/// transaction. The period is *extended* (never shortened) past any existing
/// gifted period, so re-gifting stacks. Refuses when a Stripe-managed
/// subscription exists, so the gift can never clobber live billing state (the
/// org webhook is the source of truth there). `cancel_at_period_end` is set so the
/// dashboard shows the gift as ending — there is no Stripe sub to auto-renew it.
pub async fn gift_subscription(
    pool: &Pool,
    org_id: Uuid,
    plan: &str,
    months: i32,
    credits: i32,
) -> Result<GiftOutcome, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Lock the row; bail before any write if the org is gone or Stripe-managed.
    let row: Option<(Uuid, Option<String>)> = sqlx::query_as(
        "SELECT id, stripe_subscription_id FROM organizations WHERE id = $1 FOR UPDATE",
    )
    .bind(org_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((_, stripe_sub)) = row else {
        tx.rollback().await?;
        return Ok(GiftOutcome::NotFound);
    };
    if stripe_sub.is_some() {
        tx.rollback().await?;
        return Ok(GiftOutcome::ManagedByStripe);
    }

    let (credits_balance, current_period_end): (i32, DateTime<Utc>) = sqlx::query_as(
        "UPDATE organizations SET
            plan = $2,
            subscription_status = 'active',
            subscription_interval = 'month',
            cancel_at_period_end = TRUE,
            current_period_end =
                GREATEST(COALESCE(current_period_end, now()), now())
                + make_interval(months => $3),
            credits_balance = credits_balance + $4,
            updated_at = now()
         WHERE id = $1
         RETURNING credits_balance, current_period_end",
    )
    .bind(org_id)
    .bind(plan)
    .bind(months)
    .bind(credits)
    .fetch_one(&mut *tx)
    .await?;

    // Ledger the grant (skip a zero-credit gift — e.g. a pure plan extension).
    if credits > 0 {
        sqlx::query(
            "INSERT INTO organization_credits_transactions (org_id, amount, type, description)
             VALUES ($1, $2, 'gift', $3)",
        )
        .bind(org_id)
        .bind(credits)
        .bind(format!("admin gift: {months} month(s) of {plan}"))
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(GiftOutcome::Gifted {
        credits_balance,
        credits_granted: credits,
        current_period_end,
    })
}

/// Ceiling division for non-negative i64 (`i64::div_ceil` is still unstable).
fn ceil_div(n: i64, d: i64) -> i64 {
    let n = n.max(0);
    (n + d - 1) / d
}

/// Credits to charge for a recording: 1 per minute, rounded up.
pub fn recording_credits(duration_seconds: i64) -> i32 {
    ceil_div(duration_seconds, 60) as i32
}

/// Credits to charge for batch transcription, derived from what it costs us.
///
/// This was a flat `5` credits an hour — $0.05 — with no cost written down
/// anywhere behind it. Deepgram bills Nova-2 pre-recorded at **$0.258 an hour**
/// (the org's own console, confirmed 2026-09-07), so every transcribed hour lost
/// roughly $0.21: we recovered under a fifth of the bill. The giveaway was
/// internal, and visible without knowing Deepgram's price at all — recording an
/// hour cost 60 credits while transcribing that same hour cost 5.
///
/// Now it follows the same rule as every metered rate: `cost × (1 + markup)`,
/// at 100 credits = $1, from `DEEPGRAM_COST_PER_MINUTE` and
/// `DEEPGRAM_MARKUP_PERCENT`. Rounded UP, because this is one discrete job
/// rather than a stream with a next tick to carry a remainder into.
pub fn transcription_credits(duration_seconds: i64, cost_per_minute: f64, markup: f64) -> i32 {
    let minutes = Decimal::from(duration_seconds.max(0)) / Decimal::from(60u64);
    let price = minute_price_usd(cost_per_minute, markup) * minutes;
    (price * Decimal::from(100u64))
        .ceil()
        .to_i32()
        .unwrap_or(i32::MAX)
}

/// Credits to charge for a transcript translation: 2 per 1000 words, rounded up.
pub fn translation_credits(word_count: i64) -> i32 {
    (ceil_div(word_count, 1000) * 2) as i32
}

/// Credits to charge for one insights query/report — a flat fee per LLM synthesis
/// (embedding + one Groq call over bounded context). Comparable to a transcript
/// translation; charged only when the synthesis succeeds.
pub fn insight_credits() -> i32 {
    3
}

/// What one minute of a per-minute feature costs the customer, in USD:
/// `cost × (1 + markup)`. The price the credit math starts from.
pub fn minute_price_usd(cost_per_minute: f64, markup: f64) -> Decimal {
    Decimal::from_f64_retain(cost_per_minute * (1.0 + markup))
        .unwrap_or(Decimal::ZERO)
        .round_dp(6)
}

/// Bills a CONSTANT per-minute price against a clock, in whole org credits.
///
/// Each call recomputes what the whole session owes from the elapsed time and
/// subtracts what has already been charged. That is the point: nothing
/// accumulates, so nothing drifts. Deriving a fixed per-tick amount by dividing
/// the minute price first looks equivalent and is not — `$0.26 / 6` has no exact
/// decimal form, so six of those ticks sum to just under $0.26 and the minute
/// silently bills 25 credits instead of 26. Multiplying by the elapsed seconds
/// before dividing keeps every whole minute exact and lets the intermediate
/// ticks self-correct.
///
/// Use this where the rate is fixed and the clock is authoritative (the
/// assistants). Where each tick's cost genuinely differs — a call meter scaling
/// with who is speaking — use [`CreditAccumulator`] instead.
#[derive(Debug)]
pub struct MinuteRateMeter {
    price_per_minute_usd: Decimal,
    charged: i32,
}

impl MinuteRateMeter {
    pub fn new(cost_per_minute: f64, markup: f64) -> Self {
        Self {
            price_per_minute_usd: minute_price_usd(cost_per_minute, markup),
            charged: 0,
        }
    }

    /// Credits now due, given the session's total elapsed seconds. Returns 0
    /// while the next whole credit has not been earned yet.
    pub fn due(&mut self, elapsed_secs: u64) -> i32 {
        let owed_usd =
            self.price_per_minute_usd * Decimal::from(elapsed_secs) / Decimal::from(60u64);
        let owed_credits = (owed_usd * Decimal::from(100u64))
            .floor()
            .to_i32()
            .unwrap_or(i32::MAX);
        let due = owed_credits - self.charged;
        if due <= 0 {
            return 0;
        }
        self.charged = owed_credits;
        due
    }

    /// Credits charged so far.
    pub fn charged(&self) -> i32 {
        self.charged
    }
}

/// Turns a stream of fractional USD charges into whole org credits.
///
/// The org pool is `INTEGER` at **100 credits = $1**, so a per-second charge
/// worth a fraction of a cent cannot be handed to it as it happens. The
/// established alternative — `ceil` a whole minute, as the assistants do —
/// over-charges every single minute rather than occasionally: with the default
/// rates it lands exactly on a half credit (`0.30 × 1.25 × 100 = 37.5`,
/// `0.18 × 1.25 × 100 = 22.5`), so it rounds up 0.5 credits every minute of
/// every session. Over half an hour that is 15 credits the customer never owed.
///
/// So accumulate instead: hand over whole credits as they actually accrue and
/// keep the remainder for the next tick. The error never exceeds one credit and
/// never compounds, and the customer is never charged for time they did not use.
///
/// The running total is a [`Decimal`], not an `f64`. A rate like $0.225/min has
/// no exact binary representation, and thirty additions of it drift far enough
/// to swallow a whole credit — which is the kind of bug that only shows up on
/// the invoice.
#[derive(Debug, Default)]
pub struct CreditAccumulator {
    /// Cost incurred but not yet charged, in USD. Always under one credit after
    /// a call to [`take`](Self::take).
    owed_usd: Decimal,
}

impl CreditAccumulator {
    /// One org credit is one US cent.
    fn usd_per_credit() -> Decimal {
        Decimal::new(1, 2)
    }

    /// Record `usd` of cost and return the whole credits now due (0 while the
    /// running total is still under a cent). A non-positive charge is ignored.
    pub fn take(&mut self, usd: Decimal) -> i32 {
        if usd > Decimal::ZERO {
            self.owed_usd += usd;
        }
        let credits = (self.owed_usd / Self::usd_per_credit()).floor();
        if credits < Decimal::ONE {
            return 0;
        }
        self.owed_usd -= credits * Self::usd_per_credit();
        credits.to_i32().unwrap_or(i32::MAX)
    }

    /// Cost carried over, in USD — under one credit by construction. The tail a
    /// session ends on: too small to charge, deliberately given away rather than
    /// rounded up.
    pub fn carried_usd(&self) -> Decimal {
        self.owed_usd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usd(cents: i64) -> Decimal {
        Decimal::new(cents, 4) // e.g. usd(40) = $0.0040
    }

    /// Regression: the assistants used to round a whole minute UP to an integer
    /// number of credits and then divide that integer by the ticks in a minute.
    /// Both steps lose money the same way — `ceil(22.5) = 23`, then
    /// `23 * 10 / 60 = 3` credits a tick, i.e. 18 a minute against the 22.5
    /// actually owed. A fifth of the revenue, gone quietly.
    #[test]
    fn per_tick_billing_matches_the_minute_price_it_is_derived_from() {
        let price = minute_price_usd(0.18, 0.25); // $0.225/min = 22.5 credits
        let per_tick = price * Decimal::from(10) / Decimal::from(60);
        let mut acc = CreditAccumulator::default();
        let charged: i32 = (0..6).map(|_| acc.take(per_tick)).sum();
        assert_eq!(charged, 22, "a minute of six 10s ticks");
        // The half credit is carried, not dropped and not rounded up.
        assert_eq!(acc.carried_usd(), Decimal::new(50, 4));

        // The old arithmetic, spelled out here rather than kept alive as a
        // production function nothing calls any more.
        let old_ceiled_minute = (0.18f64 * 1.25 * 100.0).ceil() as i32; // 23
        let old_per_minute = old_ceiled_minute * 10 / 60 * 6; // 18
        assert_eq!(old_per_minute, 18, "the leak this test exists to prevent");
        assert!(charged > old_per_minute);
    }

    #[test]
    fn minute_rate_meter_bills_a_whole_minute_exactly() {
        // $0.26/min = exactly 26 credits. Pre-dividing into six ticks loses one:
        // 0.26/6 has no exact decimal form. Billing off the clock does not.
        let mut m = MinuteRateMeter::new(0.20, 0.30);
        let charged: i32 = (1..=6).map(|t| m.due(t * 10)).sum();
        assert_eq!(charged, 26);
        assert_eq!(m.charged(), 26);
    }

    #[test]
    fn minute_rate_meter_carries_a_half_credit_into_the_next_minute() {
        // $0.225/min = 22.5 credits.
        let mut m = MinuteRateMeter::new(0.18, 0.25);
        let first: i32 = (1..=6).map(|t| m.due(t * 10)).sum();
        assert_eq!(first, 22, "the half credit is not charged early");
        let second: i32 = (7..=12).map(|t| m.due(t * 10)).sum();
        assert_eq!(first + second, 45, "and it arrives with the second minute");
    }

    #[test]
    fn minute_rate_meter_never_bills_the_same_second_twice() {
        let mut m = MinuteRateMeter::new(0.18, 0.25);
        assert_eq!(m.due(60), 22);
        // A repeated or out-of-order tick owes nothing more.
        assert_eq!(m.due(60), 0);
        assert_eq!(m.due(30), 0);
        assert_eq!(m.charged(), 22);
    }

    #[test]
    fn accumulator_charges_only_whole_credits_and_keeps_the_rest() {
        let mut acc = CreditAccumulator::default();
        // Under a cent → nothing is due yet.
        assert_eq!(acc.take(usd(40)), 0);
        assert_eq!(acc.take(usd(40)), 0);
        // Crossing the cent charges exactly one credit, not the ceiling of three.
        assert_eq!(acc.take(usd(40)), 1);
        assert_eq!(acc.carried_usd(), Decimal::new(20, 4));
    }

    #[test]
    fn accumulator_hands_over_several_credits_at_once() {
        let mut acc = CreditAccumulator::default();
        assert_eq!(acc.take(Decimal::new(750, 4)), 7);
        assert_eq!(acc.carried_usd(), Decimal::new(50, 4));
    }

    /// Over a long session the accumulator bills exactly what was used — no
    /// drift from repeated fractional additions, which is why it holds a
    /// `Decimal` and not an `f64`.
    #[test]
    fn accumulator_bills_a_long_session_exactly() {
        // The help assistant's default rate: $0.225/min → 22.5 credits/min.
        let per_minute = Decimal::new(2250, 4);
        let mut acc = CreditAccumulator::default();
        let mut charged = 0;
        for _ in 0..30 {
            charged += acc.take(per_minute);
        }
        // 30 min × 22.5 = 675 credits owed, and every one of them is charged.
        assert_eq!(charged, 675);
        assert_eq!(acc.carried_usd(), Decimal::ZERO);
        // Rounding each minute up instead would have taken 690.
        assert_eq!((0.18f64 * 1.25 * 100.0).ceil() as i32 * 30, 690);
    }

    #[test]
    fn accumulator_ignores_a_non_positive_charge() {
        let mut acc = CreditAccumulator::default();
        assert_eq!(acc.take(Decimal::ZERO), 0);
        assert_eq!(acc.take(Decimal::new(-100, 2)), 0);
        assert_eq!(acc.carried_usd(), Decimal::ZERO);
    }

    /// Deepgram's own rate for what we actually call: Nova-2 pre-recorded at
    /// $0.258/hour. With the standard 25% markup an hour costs the customer
    /// $0.3225 — 33 credits, against the flat 5 this used to charge.
    #[test]
    fn transcription_is_priced_off_the_real_deepgram_rate() {
        const COST_PER_MIN: f64 = 0.0043; // $0.258/hour
        assert_eq!(transcription_credits(3600, COST_PER_MIN, 0.25), 33);
        assert_eq!(
            transcription_credits(1800, COST_PER_MIN, 0.25),
            17,
            "half an hour"
        );
        assert_eq!(transcription_credits(0, COST_PER_MIN, 0.25), 0);
        // A few seconds still costs a credit — rounded up, never given away.
        assert_eq!(transcription_credits(5, COST_PER_MIN, 0.25), 1);
        // The old flat rate recovered under a fifth of the bill.
        assert!(transcription_credits(3600, COST_PER_MIN, 0.25) > 5 * 6);
    }

    /// A different contracted rate flows straight through — the number lives in
    /// config, not in this function.
    #[test]
    fn transcription_follows_whatever_rate_is_configured() {
        // Nova-3 multilingual list price, $0.0052/min = $0.312/hour.
        assert_eq!(transcription_credits(3600, 0.0052, 0.25), 39);
        // No markup: the customer pays exactly what we do.
        assert_eq!(transcription_credits(3600, 0.0043, 0.0), 26);
    }

    #[test]
    fn credit_rates_round_up() {
        assert_eq!(recording_credits(0), 0);
        assert_eq!(recording_credits(1), 1);
        assert_eq!(recording_credits(60), 1);
        assert_eq!(recording_credits(61), 2);

        assert_eq!(translation_credits(0), 0);
        assert_eq!(translation_credits(1000), 2);
        assert_eq!(translation_credits(1001), 4);

        assert_eq!(insight_credits(), 3);
    }
}
