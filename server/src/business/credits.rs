//! Org credit pool: atomic spend + grant (spec 0106).
//!
//! Mirrors the consumer ledger pattern (`billing.rs`) — `SELECT … FOR UPDATE`,
//! update balance, append a ledger row — but against `organizations.credits_balance`
//! (INTEGER) and `organization_credits_transactions`. Org credits and consumer
//! DECIMAL credits never mix.

use chrono::{DateTime, Utc};
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
    let balance: i32 =
        sqlx::query_scalar("SELECT credits_balance FROM organizations WHERE id = $1 FOR UPDATE")
            .bind(org_id)
            .fetch_one(&mut *tx)
            .await?;
    if amount > 0 && balance < amount {
        tx.rollback().await?;
        return Ok(OrgCharge::Insufficient {
            balance,
            required: amount,
        });
    }
    let new_balance = balance - amount;
    sqlx::query("UPDATE organizations SET credits_balance = $2, updated_at = now() WHERE id = $1")
        .bind(org_id)
        .bind(new_balance)
        .execute(&mut *tx)
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
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
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

/// True when the org has a live, unlapsed subscription: status `active` AND its
/// period hasn't ended. A NULL `current_period_end` counts as active — a
/// freshly-checked-out Stripe sub has no period end until its first invoice
/// webhook lands. Admin-gifted subscriptions always set a period end, so this is
/// what makes a gift auto-expire (there is no background scheduler to flip the
/// status). Missing org → not active.
pub async fn org_subscription_active(pool: &Pool, org_id: Uuid) -> Result<bool, sqlx::Error> {
    let active: Option<bool> = sqlx::query_scalar(
        "SELECT subscription_status = 'active'
                AND (current_period_end IS NULL OR current_period_end > now())
         FROM organizations WHERE id = $1",
    )
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

/// Credits to charge for transcription: 5 per hour of audio, rounded up.
pub fn transcription_credits(duration_seconds: i64) -> i32 {
    (ceil_div(duration_seconds, 3600) * 5) as i32
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

/// Credits to deduct for one minute of a voice-assistant session.
///
/// Formula: `ceil(cost_per_minute × (1 + markup) × 100)` where 100 credits = $1.
/// The markup is stored as a fraction (e.g. 0.25 = 25%). The ceiling ensures we
/// never under-charge for fractional cents.
///
/// Example (default config): `ceil(0.30 × 1.25 × 100) = ceil(37.5) = 38`.
pub fn voice_assistant_minute_credits(cfg: &crate::config::VoiceAssistantConfig) -> i32 {
    let raw = cfg.cost_per_minute * (1.0 + cfg.markup) * 100.0;
    raw.ceil() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credit_rates_round_up() {
        assert_eq!(recording_credits(0), 0);
        assert_eq!(recording_credits(1), 1);
        assert_eq!(recording_credits(60), 1);
        assert_eq!(recording_credits(61), 2);

        assert_eq!(transcription_credits(0), 0);
        assert_eq!(transcription_credits(3600), 5);
        assert_eq!(transcription_credits(3601), 10);

        assert_eq!(translation_credits(0), 0);
        assert_eq!(translation_credits(1000), 2);
        assert_eq!(translation_credits(1001), 4);

        assert_eq!(insight_credits(), 3);
    }
}
