//! Billing service: atomic credit ledger over the `users` /
//! `credit_transactions` / `usage_sessions` tables.
//!
//! Every balance mutation runs inside a transaction that takes a `SELECT … FOR
//! UPDATE` row lock on the user, so concurrent deductions (e.g. the usage meter
//! racing a top-up) can never over-spend or interleave a stale balance. Money is
//! [`Decimal`] end-to-end; conversion to `f64` happens only at the API edge.

use chrono::{DateTime, Utc};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::Serialize;
use sqlx::FromRow;
use uuid::Uuid;

use crate::db::Pool;

/// Credit operations against the database. Cheap to clone (the pool is an `Arc`).
#[derive(Clone)]
pub struct BillingService {
    pool: Pool,
    /// Minimum balance required to join a call.
    min_balance_to_join: Decimal,
}

impl BillingService {
    pub fn new(pool: Pool, min_balance_to_join: Decimal) -> Self {
        Self {
            pool,
            min_balance_to_join,
        }
    }

    /// Current balance for a user.
    pub async fn get_balance(&self, user_id: Uuid) -> Result<Decimal, sqlx::Error> {
        sqlx::query_scalar("SELECT balance FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&self.pool)
            .await
    }

    /// Whether the user has enough credit to join a call.
    pub async fn can_join(&self, user_id: Uuid) -> Result<bool, sqlx::Error> {
        Ok(self.get_balance(user_id).await? >= self.min_balance_to_join)
    }

    /// A user's Google avatar URL, if any.
    pub async fn get_avatar(&self, user_id: Uuid) -> Result<Option<String>, sqlx::Error> {
        let avatar: Option<String> =
            sqlx::query_scalar("SELECT avatar_url FROM users WHERE id = $1")
                .bind(user_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(avatar)
    }

    /// A user's stored Cartesia cloned-voice id (spec 0108), if any. `None` until they
    /// complete the pre-join voice-prep step.
    pub async fn get_cartesia_voice_id(
        &self,
        user_id: Uuid,
    ) -> Result<Option<String>, sqlx::Error> {
        let voice_id: Option<String> =
            sqlx::query_scalar("SELECT cartesia_voice_id FROM users WHERE id = $1")
                .bind(user_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(voice_id)
    }

    /// Persist a user's Cartesia cloned-voice id after Instant Voice Cloning (spec 0108).
    pub async fn set_cartesia_voice_id(
        &self,
        user_id: Uuid,
        voice_id: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE users SET cartesia_voice_id = $1, updated_at = now() WHERE id = $2")
            .bind(voice_id)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Persist a user's Vox Voices preferences (migration 033). A NULL argument leaves that
    /// column unchanged (COALESCE), so the engine choice and the voice can be updated
    /// independently — the client sends only the field it changed.
    pub async fn set_tts_prefs(
        &self,
        user_id: Uuid,
        engine_pref: Option<&str>,
        voice_id: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE users SET tts_engine_pref = COALESCE($1, tts_engine_pref), \
             tts_voice_id = COALESCE($2, tts_voice_id), updated_at = now() WHERE id = $3",
        )
        .bind(engine_pref)
        .bind(voice_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Persist a user's preferred language (migration 045). Called from
    /// `POST /api/user/language`; NULL clears the preference.
    pub async fn set_language(
        &self,
        user_id: Uuid,
        language: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE users SET language = $1, updated_at = now() WHERE id = $2")
            .bind(language)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Add credit (purchase / free grant / refund). Locks the user row, bumps the
    /// balance, and writes a ledger entry. Returns the new balance.
    pub async fn add_credits(
        &self,
        user_id: Uuid,
        amount: Decimal,
        kind: &str,
        description: Option<&str>,
        stripe_event_id: Option<&str>,
    ) -> Result<Decimal, BillingError> {
        let mut tx = self.pool.begin().await?;
        let balance: Decimal =
            sqlx::query_scalar("SELECT balance FROM users WHERE id = $1 FOR UPDATE")
                .bind(user_id)
                .fetch_one(&mut *tx)
                .await?;
        let new_balance = (balance + amount).round_dp(6);
        sqlx::query("UPDATE users SET balance = $2, updated_at = now() WHERE id = $1")
            .bind(user_id)
            .bind(new_balance)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO credit_transactions
                 (user_id, amount, kind, balance_after, description, stripe_event_id)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(user_id)
        .bind(amount)
        .bind(kind)
        .bind(new_balance)
        .bind(description)
        .bind(stripe_event_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(new_balance)
    }

    /// Atomically record a Stripe event and credit the user — exactly once.
    /// The `stripe_events` insert (PK = event id) is the idempotency guard: a
    /// replayed webhook hits `ON CONFLICT DO NOTHING`, so `rows_affected == 0`
    /// means "already processed" and nothing is credited (returns `Ok(false)`).
    /// On first delivery the balance + ledger update commit in the same tx as
    /// the event row, so a crash can't credit without recording the event.
    pub async fn credit_from_stripe_event(
        &self,
        event_id: &str,
        event_type: &str,
        user_id: Uuid,
        amount: Decimal,
        description: &str,
    ) -> Result<bool, BillingError> {
        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO stripe_events (id, type) VALUES ($1, $2) ON CONFLICT (id) DO NOTHING",
        )
        .bind(event_id)
        .bind(event_type)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            tx.rollback().await?;
            return Ok(false); // duplicate delivery — already credited
        }
        let balance: Decimal =
            sqlx::query_scalar("SELECT balance FROM users WHERE id = $1 FOR UPDATE")
                .bind(user_id)
                .fetch_one(&mut *tx)
                .await?;
        let new_balance = (balance + amount).round_dp(6);
        sqlx::query("UPDATE users SET balance = $2, updated_at = now() WHERE id = $1")
            .bind(user_id)
            .bind(new_balance)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO credit_transactions
                 (user_id, amount, kind, balance_after, description, stripe_event_id)
             VALUES ($1, $2, 'purchase', $3, $4, $5)",
        )
        .bind(user_id)
        .bind(amount)
        .bind(new_balance)
        .bind(description)
        .bind(event_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Deduct usage cost. Locks the user row; if the balance can't cover `amount`
    /// the transaction rolls back and [`BillingError::InsufficientFunds`] is
    /// returned (balance untouched). When `session_id` is given, the matching
    /// `usage_session`'s `speaking_seconds` + `cost` are accumulated atomically.
    /// Returns the new balance.
    pub async fn deduct_usage(
        &self,
        user_id: Uuid,
        session_id: Option<Uuid>,
        seconds: i32,
        amount: Decimal,
    ) -> Result<Decimal, BillingError> {
        let mut tx = self.pool.begin().await?;
        let balance: Decimal =
            sqlx::query_scalar("SELECT balance FROM users WHERE id = $1 FOR UPDATE")
                .bind(user_id)
                .fetch_one(&mut *tx)
                .await?;
        if balance < amount {
            return Err(BillingError::InsufficientFunds);
        }
        let new_balance = (balance - amount).round_dp(6);
        sqlx::query("UPDATE users SET balance = $2, updated_at = now() WHERE id = $1")
            .bind(user_id)
            .bind(new_balance)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO credit_transactions (user_id, amount, kind, balance_after, description)
             VALUES ($1, $2, 'usage', $3, 'Speaking time')",
        )
        .bind(user_id)
        .bind(-amount)
        .bind(new_balance)
        .execute(&mut *tx)
        .await?;
        if let Some(sid) = session_id {
            sqlx::query(
                "UPDATE usage_sessions
                 SET speaking_seconds = speaking_seconds + $2, cost = cost + $3
                 WHERE id = $1",
            )
            .bind(sid)
            .bind(seconds)
            .bind(amount)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(new_balance)
    }

    /// Deduct the cost of an AI feature (report, sentiment, email draft, live
    /// suggestions). Same atomic shape as [`deduct_usage`](Self::deduct_usage):
    /// row lock → balance check → update → ledger. The ledger row carries the
    /// feature name as its `kind` plus the per-feature detail columns
    /// (`feature`, `session_id`, `metadata`). No usage_sessions accumulation —
    /// AI charges are not speaking time. Returns the new balance.
    pub async fn deduct_feature(
        &self,
        user_id: Uuid,
        session_id: Option<Uuid>,
        feature: &str,
        amount: Decimal,
        description: &str,
        metadata: serde_json::Value,
    ) -> Result<Decimal, BillingError> {
        let mut tx = self.pool.begin().await?;
        let balance: Decimal =
            sqlx::query_scalar("SELECT balance FROM users WHERE id = $1 FOR UPDATE")
                .bind(user_id)
                .fetch_one(&mut *tx)
                .await?;
        if balance < amount {
            return Err(BillingError::InsufficientFunds);
        }
        let new_balance = (balance - amount).round_dp(6);
        sqlx::query("UPDATE users SET balance = $2, updated_at = now() WHERE id = $1")
            .bind(user_id)
            .bind(new_balance)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO credit_transactions
                 (user_id, amount, kind, balance_after, description, feature, session_id, metadata)
             VALUES ($1, $2, $3, $4, $5, $3, $6, $7)",
        )
        .bind(user_id)
        .bind(-amount)
        .bind(feature)
        .bind(new_balance)
        .bind(description)
        .bind(session_id)
        .bind(metadata)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(new_balance)
    }

    /// Open a usage session for a call the user just joined.
    /// Open a usage session for a call, tagged with the translation `engine_id`
    /// the speaker chose (spec 0093), so cost is attributable per engine.
    pub async fn create_session(
        &self,
        user_id: Uuid,
        room: &str,
        engine_id: &str,
    ) -> Result<Uuid, sqlx::Error> {
        sqlx::query_scalar(
            "INSERT INTO usage_sessions (user_id, room, engine_id) VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(user_id)
        .bind(room)
        .bind(engine_id)
        .fetch_one(&self.pool)
        .await
    }

    /// Mark a usage session ended.
    pub async fn finalize_session(&self, session_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE usage_sessions SET ended_at = now() WHERE id = $1 AND ended_at IS NULL",
        )
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Recent ledger entries, newest first.
    pub async fn get_history(
        &self,
        user_id: Uuid,
        limit: i64,
    ) -> Result<Vec<TransactionRecord>, sqlx::Error> {
        let rows: Vec<TxRow> = sqlx::query_as(
            "SELECT amount, kind, balance_after, description, created_at
             FROM credit_transactions
             WHERE user_id = $1
             ORDER BY created_at DESC
             LIMIT $2",
        )
        .bind(user_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(TransactionRecord::from).collect())
    }

    /// Recent usage sessions for a user, newest first.
    pub async fn get_sessions(
        &self,
        user_id: Uuid,
        limit: i64,
    ) -> Result<Vec<UsageRecord>, sqlx::Error> {
        let rows: Vec<UsageRow> = sqlx::query_as(
            "SELECT room, speaking_seconds, cost, started_at, ended_at
             FROM usage_sessions
             WHERE user_id = $1
             ORDER BY started_at DESC
             LIMIT $2",
        )
        .bind(user_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(UsageRecord::from).collect())
    }
}

/// Internal row shape (decimals); mapped to the f64 DTO at the edge.
#[derive(Debug, FromRow)]
struct TxRow {
    amount: Decimal,
    kind: String,
    balance_after: Decimal,
    description: Option<String>,
    created_at: DateTime<Utc>,
}

/// A ledger entry as sent to the client (money as `f64`).
#[derive(Debug, Serialize)]
pub struct TransactionRecord {
    pub amount: f64,
    pub kind: String,
    pub balance_after: f64,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl From<TxRow> for TransactionRecord {
    fn from(r: TxRow) -> Self {
        Self {
            amount: r.amount.to_f64().unwrap_or(0.0),
            kind: r.kind,
            balance_after: r.balance_after.to_f64().unwrap_or(0.0),
            description: r.description,
            created_at: r.created_at,
        }
    }
}

/// Internal usage-session row (decimals); mapped to the f64 DTO at the edge.
#[derive(Debug, FromRow)]
struct UsageRow {
    room: String,
    speaking_seconds: i32,
    cost: Decimal,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
}

/// A usage session as sent to the client (cost as `f64`).
#[derive(Debug, Serialize)]
pub struct UsageRecord {
    pub room: String,
    pub speaking_seconds: i32,
    pub cost: f64,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
}

impl From<UsageRow> for UsageRecord {
    fn from(r: UsageRow) -> Self {
        Self {
            room: r.room,
            speaking_seconds: r.speaking_seconds,
            cost: r.cost.to_f64().unwrap_or(0.0),
            started_at: r.started_at,
            ended_at: r.ended_at,
        }
    }
}

/// Convert a USD `f64` (config value, package price) to a 6-dp [`Decimal`].
pub fn usd(value: f64) -> Decimal {
    Decimal::from_f64_retain(value)
        .unwrap_or(Decimal::ZERO)
        .round_dp(6)
}

/// Errors from credit mutations.
#[derive(Debug)]
pub enum BillingError {
    /// Balance can't cover the requested deduction.
    InsufficientFunds,
    Db(sqlx::Error),
}

impl std::fmt::Display for BillingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BillingError::InsufficientFunds => write!(f, "insufficient funds"),
            BillingError::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for BillingError {}

impl From<sqlx::Error> for BillingError {
    fn from(e: sqlx::Error) -> Self {
        BillingError::Db(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Connect + migrate the test DB, or `None` when `DATABASE_URL` is unset.
    async fn test_service() -> Option<BillingService> {
        let url = std::env::var("DATABASE_URL").ok()?;
        let pool = crate::db::connect(&url).await.ok()?;
        crate::db::migrate(&pool).await.ok()?;
        Some(BillingService::new(pool, Decimal::new(5, 2))) // min join 0.05
    }

    /// Insert a bare user with the given starting balance, returning its id.
    async fn make_user(svc: &BillingService, balance: Decimal) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO users (google_id, email, name, balance)
             VALUES ($1, $2, 'T', $3) RETURNING id",
        )
        .bind(format!("g-{}", Uuid::new_v4()))
        .bind(format!("{}@x.com", Uuid::new_v4()))
        .bind(balance)
        .fetch_one(&svc.pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn add_and_deduct_record_ledger() {
        let Some(svc) = test_service().await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        let uid = make_user(&svc, Decimal::ZERO).await;

        let after_add = svc
            .add_credits(uid, Decimal::new(500, 2), "purchase", Some("Starter"), None)
            .await
            .unwrap();
        assert_eq!(after_add, Decimal::new(500, 2)); // 5.00

        let after_deduct = svc
            .deduct_usage(uid, None, 30, Decimal::new(50, 2))
            .await
            .unwrap();
        assert_eq!(after_deduct, Decimal::new(450, 2)); // 4.50

        let history = svc.get_history(uid, 10).await.unwrap();
        assert_eq!(history.len(), 2);
        // Newest first: the usage deduction.
        assert_eq!(history[0].kind, "usage");
        assert!((history[0].amount + 0.50).abs() < 1e-6); // -0.50
        assert!((history[0].balance_after - 4.50).abs() < 1e-6);
        assert_eq!(history[1].kind, "purchase");
    }

    #[tokio::test]
    async fn deduct_rejects_insufficient_funds() {
        let Some(svc) = test_service().await else {
            return;
        };
        let uid = make_user(&svc, Decimal::new(5, 2)).await; // 0.05
        let err = svc
            .deduct_usage(uid, None, 60, Decimal::new(10, 2)) // wants 0.10
            .await
            .unwrap_err();
        assert!(matches!(err, BillingError::InsufficientFunds));
        // Balance is unchanged after the rejected deduction.
        assert_eq!(svc.get_balance(uid).await.unwrap(), Decimal::new(5, 2));
    }

    #[tokio::test]
    async fn can_join_respects_threshold() {
        let Some(svc) = test_service().await else {
            return;
        };
        let rich = make_user(&svc, Decimal::new(5, 2)).await; // 0.05 == threshold
        let poor = make_user(&svc, Decimal::new(4, 2)).await; // 0.04 < threshold
        assert!(svc.can_join(rich).await.unwrap());
        assert!(!svc.can_join(poor).await.unwrap());
    }

    #[tokio::test]
    async fn concurrent_deductions_never_overspend() {
        let Some(svc) = test_service().await else {
            return;
        };
        let uid = make_user(&svc, Decimal::new(100, 2)).await; // 1.00

        // 20 racing deductions of 0.10 — exactly 10 can succeed.
        let mut handles = Vec::new();
        for _ in 0..20 {
            let svc = svc.clone();
            handles.push(tokio::spawn(async move {
                svc.deduct_usage(uid, None, 1, Decimal::new(10, 2)).await
            }));
        }
        let mut ok = 0;
        for h in handles {
            if h.await.unwrap().is_ok() {
                ok += 1;
            }
        }
        assert_eq!(ok, 10);
        // Never negative, and fully drained.
        assert_eq!(svc.get_balance(uid).await.unwrap(), Decimal::ZERO);
    }

    #[tokio::test]
    async fn deduct_feature_writes_ledger_detail() {
        let Some(svc) = test_service().await else {
            return;
        };
        let uid = make_user(&svc, Decimal::new(100, 2)).await; // 1.00
        let sid = Uuid::new_v4();

        let after = svc
            .deduct_feature(
                uid,
                Some(sid),
                "ai_report",
                Decimal::new(7, 2), // 0.07
                "AI report",
                serde_json::json!({ "duration_min": 10 }),
            )
            .await
            .unwrap();
        assert_eq!(after, Decimal::new(93, 2)); // 0.93

        let (amount, kind, feature, tx_sid, metadata): (
            Decimal,
            String,
            Option<String>,
            Option<Uuid>,
            Option<serde_json::Value>,
        ) = sqlx::query_as(
            "SELECT amount, kind, feature, session_id, metadata
             FROM credit_transactions
             WHERE user_id = $1
             ORDER BY created_at DESC
             LIMIT 1",
        )
        .bind(uid)
        .fetch_one(&svc.pool)
        .await
        .unwrap();
        assert_eq!(amount, Decimal::new(-7, 2));
        assert_eq!(kind, "ai_report");
        assert_eq!(feature.as_deref(), Some("ai_report"));
        assert_eq!(tx_sid, Some(sid));
        assert_eq!(metadata.unwrap()["duration_min"], 10);
    }

    #[tokio::test]
    async fn deduct_feature_rolls_back_on_insufficient_funds() {
        let Some(svc) = test_service().await else {
            return;
        };
        let uid = make_user(&svc, Decimal::new(1, 2)).await; // 0.01
        let err = svc
            .deduct_feature(
                uid,
                None,
                "ai_sentiment",
                Decimal::new(10, 2), // wants 0.10
                "Sentiment analysis",
                serde_json::json!({}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, BillingError::InsufficientFunds));
        // Balance untouched, no ledger row written.
        assert_eq!(svc.get_balance(uid).await.unwrap(), Decimal::new(1, 2));
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM credit_transactions WHERE user_id = $1")
                .bind(uid)
                .fetch_one(&svc.pool)
                .await
                .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn session_lifecycle_accumulates_cost() {
        let Some(svc) = test_service().await else {
            return;
        };
        let uid = make_user(&svc, Decimal::new(100, 2)).await; // 1.00
        let sid = svc.create_session(uid, "room-x", "standard").await.unwrap();

        svc.deduct_usage(uid, Some(sid), 5, Decimal::new(5, 2))
            .await
            .unwrap();
        svc.deduct_usage(uid, Some(sid), 5, Decimal::new(5, 2))
            .await
            .unwrap();
        svc.finalize_session(sid).await.unwrap();

        let (secs, cost, ended): (i32, Decimal, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT speaking_seconds, cost, ended_at FROM usage_sessions WHERE id = $1",
        )
        .bind(sid)
        .fetch_one(&svc.pool)
        .await
        .unwrap();
        assert_eq!(secs, 10);
        assert_eq!(cost, Decimal::new(10, 2)); // 0.10
        assert!(ended.is_some());
    }
}
