//! Org credit pool: atomic spend + grant (spec 0106).
//!
//! Mirrors the consumer ledger pattern (`billing.rs`) — `SELECT … FOR UPDATE`,
//! update balance, append a ledger row — but against `organizations.credits_balance`
//! (INTEGER) and `organization_credits_transactions`. Org credits and consumer
//! DECIMAL credits never mix.

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
        "INSERT INTO organization_credits_transactions (org_id, amount, type, description, session_id)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(org_id)
    .bind(-amount)
    .bind(kind)
    .bind(description)
    .bind(session_id)
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
    }
}
