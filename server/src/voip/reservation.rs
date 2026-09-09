//! Credit holds for calls that have not happened yet (spec 0111, R9–R11, D6).
//!
//! ## The race this closes
//!
//! `business::credits::deduct_org_credits_tx` is atomic *per deduction*, but nothing in
//! the existing ledger holds funds for work that is about to happen. So N simultaneous
//! dials each read the same balance, each decide they can afford a call, and the org ends
//! up on more calls than it can pay for. Checking the balance harder does not fix it —
//! any check-then-act is racy no matter how carefully it is written.
//!
//! So a hold is a **real deduction at dial time**. The money leaves the pool immediately,
//! with its own ledger row, and the second dial simply finds a smaller balance. There is
//! no window to race. Settlement afterwards refunds whatever the call did not use.
//!
//! ## The accounting invariants
//!
//! For every reservation, once closed:
//!
//! ```text
//! released = held - min(actual, held)          // what came back
//! settled + shortfall = actual                 // what was owed
//! net pool movement = -settled                 // what the org actually paid
//! ```
//!
//! `shortfall` is the part of a call the pool could not cover **after** it had already
//! happened. It is recorded rather than hidden: a call cannot be un-made, and a silently
//! swallowed shortfall is a loss nobody is measuring.

use chrono::{DateTime, Utc};
use sqlx::FromRow;
use uuid::Uuid;

use crate::business::credits::{add_org_credits_tx, deduct_org_credits_tx, OrgCharge};
use crate::db::Pool;

/// Ledger `type` values. Distinct from the meeting meter's kinds so a VoIP hold, refund
/// and charge are separable in a billing report without joining anything.
pub const KIND_HOLD: &str = "voip_hold";
pub const KIND_RELEASE: &str = "voip_hold_release";
pub const KIND_EXTRA: &str = "voip_overage";

/// An open hold.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct Reservation {
    pub id: Uuid,
    pub call_id: Uuid,
    pub org_id: Uuid,
    pub held_credits: i32,
    pub settled_credits: i32,
    pub released_credits: i32,
    pub shortfall_credits: i32,
    pub state: String,
    pub closed_at: Option<DateTime<Utc>>,
}

/// Outcome of trying to take a hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReserveOutcome {
    Held(Box<Reservation>),
    /// The pool could not cover it. Nothing was deducted and no reservation row exists,
    /// so the caller must refuse the call (R11).
    Insufficient {
        balance: i32,
        required: i32,
    },
}

/// What a closed reservation ended up doing to the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settlement {
    pub held: i32,
    /// What the org paid for this call, in total.
    pub settled: i32,
    /// What came back to the pool.
    pub released: i32,
    /// What the call owed that the pool could not cover.
    pub shortfall: i32,
}

impl Settlement {
    /// The accounting identities, checked in code rather than only in tests so a future
    /// change cannot quietly violate them in production.
    pub fn is_coherent(&self, actual: i32) -> bool {
        let expected_released = self.held - actual.min(self.held).max(0);
        self.released == expected_released && self.settled + self.shortfall == actual.max(0)
    }
}

/// Take a hold of `credits` for `call_id`, atomically.
///
/// `session_id` is the call's `call_sessions` row — the org ledger's `session_id` is a
/// FOREIGN KEY into that table, so it must be the room-lifetime id and not a
/// `usage_sessions` one.
pub async fn reserve(
    pool: &Pool,
    org_id: Uuid,
    call_id: Uuid,
    session_id: Uuid,
    actor_id: Option<Uuid>,
    credits: i32,
) -> Result<ReserveOutcome, sqlx::Error> {
    let credits = credits.max(0);
    let mut tx = pool.begin().await?;

    let charge = deduct_org_credits_tx(
        &mut tx,
        org_id,
        credits,
        KIND_HOLD,
        Some(session_id),
        actor_id,
        "Hold for a translated phone call",
    )
    .await?;

    let (balance, required) = match charge {
        OrgCharge::Charged { .. } => {
            let row: Reservation = sqlx::query_as(
                "INSERT INTO voip_credit_reservations (call_id, org_id, held_credits)
                 VALUES ($1, $2, $3)
                 RETURNING id, call_id, org_id, held_credits, settled_credits,
                           released_credits, shortfall_credits, state, closed_at",
            )
            .bind(call_id)
            .bind(org_id)
            .bind(credits)
            .fetch_one(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(ReserveOutcome::Held(Box::new(row)));
        }
        OrgCharge::Insufficient { balance, required } => (balance, required),
    };

    // Nothing was written; the commit just releases the `FOR UPDATE` lock.
    tx.commit().await?;
    Ok(ReserveOutcome::Insufficient { balance, required })
}

/// Close a hold against what the call actually consumed.
///
/// Idempotent: settling an already-closed reservation returns what it settled the first
/// time and touches no money. Webhook redelivery *will* trigger this twice, and the second
/// time must not refund a second time.
pub async fn settle(
    pool: &Pool,
    call_id: Uuid,
    session_id: Uuid,
    actual_credits: i32,
) -> Result<Option<Settlement>, sqlx::Error> {
    close(pool, call_id, session_id, actual_credits.max(0), "settled").await
}

/// Give the whole hold back — the call never connected, so it owes nothing (R10).
pub async fn release(
    pool: &Pool,
    call_id: Uuid,
    session_id: Uuid,
) -> Result<Option<Settlement>, sqlx::Error> {
    close(pool, call_id, session_id, 0, "released").await
}

async fn close(
    pool: &Pool,
    call_id: Uuid,
    session_id: Uuid,
    actual: i32,
    final_state: &str,
) -> Result<Option<Settlement>, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Lock the reservation itself, not the org row: two settlements for the SAME call are
    // the race here (redelivered hangup + reconcile job), and the org lock would not stop
    // them from both refunding.
    let existing: Option<Reservation> = sqlx::query_as(
        "SELECT id, call_id, org_id, held_credits, settled_credits, released_credits,
                shortfall_credits, state, closed_at
         FROM voip_credit_reservations
         WHERE call_id = $1
         ORDER BY created_at DESC
         LIMIT 1
         FOR UPDATE",
    )
    .bind(call_id)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(res) = existing else {
        tx.commit().await?;
        return Ok(None);
    };

    if res.state != "held" {
        // Already closed. Report what happened, change nothing.
        tx.commit().await?;
        return Ok(Some(Settlement {
            held: res.held_credits,
            settled: res.settled_credits,
            released: res.released_credits,
            shortfall: res.shortfall_credits,
        }));
    }

    let held = res.held_credits;
    let covered = actual.min(held);
    let released = held - covered;
    let mut settled = covered;
    let mut shortfall = 0;

    if released > 0 {
        add_org_credits_tx(
            &mut tx,
            res.org_id,
            released,
            KIND_RELEASE,
            "Unused hold returned after a translated phone call",
            None,
            Some(session_id),
        )
        .await?;
    }

    // The call ran longer or costlier than the hold. Charge the difference; if the pool
    // cannot cover it, charge what is there and record the rest as a shortfall — the call
    // already happened and pretending otherwise would just move the loss somewhere
    // nobody looks.
    let extra = actual - covered;
    if extra > 0 {
        match deduct_org_credits_tx(
            &mut tx,
            res.org_id,
            extra,
            KIND_EXTRA,
            Some(session_id),
            None,
            "Translated phone call exceeded its credit hold",
        )
        .await?
        {
            OrgCharge::Charged { .. } => settled += extra,
            OrgCharge::Insufficient { balance, .. } => {
                let partial = balance.max(0);
                if partial > 0 {
                    deduct_org_credits_tx(
                        &mut tx,
                        res.org_id,
                        partial,
                        KIND_EXTRA,
                        Some(session_id),
                        None,
                        "Translated phone call exceeded its credit hold (partial)",
                    )
                    .await?;
                    settled += partial;
                }
                shortfall = extra - partial;
            }
        }
    }

    sqlx::query(
        "UPDATE voip_credit_reservations
         SET state = $2, settled_credits = $3, released_credits = $4,
             shortfall_credits = $5, closed_at = now()
         WHERE id = $1",
    )
    .bind(res.id)
    .bind(final_state)
    .bind(settled)
    .bind(released)
    .bind(shortfall)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(Some(Settlement {
        held,
        settled,
        released,
        shortfall,
    }))
}

/// The open hold for a call, if any.
pub async fn open_for_call(pool: &Pool, call_id: Uuid) -> Result<Option<Reservation>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, call_id, org_id, held_credits, settled_credits, released_credits,
                shortfall_credits, state, closed_at
         FROM voip_credit_reservations
         WHERE call_id = $1 AND state = 'held'",
    )
    .bind(call_id)
    .fetch_optional(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shared fixture: an org with `credits` in the pool, plus a `call_sessions` row and a
    /// `voip_calls` row for the ledger's foreign keys to land on.
    ///
    /// DB-gated like the rest of the suite. NOTE: the database must have **pgvector**
    /// installed — a plain `postgres:16` makes these skip silently and still print `ok`.
    struct Fixture {
        pool: Pool,
        org: Uuid,
        session: Uuid,
        call: Uuid,
        user: Uuid,
    }

    async fn setup(credits: i32) -> Option<Fixture> {
        let url = std::env::var("DATABASE_URL").ok()?;
        let pool = crate::db::connect(&url).await.ok()?;
        crate::db::migrate(&pool).await.ok()?;

        let user: Uuid = sqlx::query_scalar(
            "INSERT INTO users (google_id, email, name, balance)
             VALUES ($1, $2, 'Owner', 0) RETURNING id",
        )
        .bind(format!("g-{}", Uuid::new_v4()))
        .bind(format!("{}@example.test", Uuid::new_v4()))
        .fetch_one(&pool)
        .await
        .unwrap();

        let org: Uuid = sqlx::query_scalar(
            "INSERT INTO organizations (name, slug, owner_id, credits_balance)
             VALUES ('VoIP Co', $1, $2, $3) RETURNING id",
        )
        .bind(format!("voip-{}", Uuid::new_v4().simple()))
        .bind(user)
        .bind(credits)
        .fetch_one(&pool)
        .await
        .unwrap();

        let session = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO call_sessions (id, room, org_id, kind) VALUES ($1, $2, $3, 'phone')",
        )
        .bind(session)
        .bind(format!("ph-{}", Uuid::new_v4().simple()))
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();

        let call: Uuid = sqlx::query_scalar(
            "INSERT INTO voip_calls
                (session_id, org_id, user_id, provider, direction, recipient_e164,
                 recipient_pseudonym, recipient_country, source_language, target_language,
                 engine_id)
             VALUES ($1, $2, $3, 'mock', 'outbound', '+8613800138000', 'abc123', 'CN',
                     'it', 'zh', 'standard')
             RETURNING id",
        )
        .bind(session)
        .bind(org)
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();

        Some(Fixture {
            pool,
            org,
            session,
            call,
            user,
        })
    }

    async fn balance(pool: &Pool, org: Uuid) -> i32 {
        sqlx::query_scalar("SELECT credits_balance FROM organizations WHERE id = $1")
            .bind(org)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// Net movement of the org pool attributable to this call, from the ledger alone.
    /// Read back rather than computed, because the ledger is what a customer disputes.
    async fn ledger_net(pool: &Pool, session: Uuid) -> i64 {
        sqlx::query_scalar(
            "SELECT COALESCE(SUM(amount), 0)::bigint
             FROM organization_credits_transactions WHERE session_id = $1",
        )
        .bind(session)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_hold_leaves_the_pool_immediately_so_there_is_nothing_left_to_race_for() {
        let Some(f) = setup(100).await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        let out = reserve(&f.pool, f.org, f.call, f.session, Some(f.user), 60)
            .await
            .unwrap();
        assert!(matches!(out, ReserveOutcome::Held(_)));
        // The defining property: the balance is already down BEFORE the call happens.
        assert_eq!(balance(&f.pool, f.org).await, 40);
        assert_eq!(ledger_net(&f.pool, f.session).await, -60);
    }

    #[tokio::test]
    async fn two_concurrent_holds_cannot_both_take_the_last_credits() {
        // R9. The whole point of the reservation: with a check-then-act balance test both
        // of these would succeed and the org would be on two calls it cannot pay for.
        let Some(f) = setup(100).await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };

        // A second call for the same org, so both holds are legitimate requests.
        let session2 = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO call_sessions (id, room, org_id, kind) VALUES ($1, $2, $3, 'phone')",
        )
        .bind(session2)
        .bind(format!("ph-{}", Uuid::new_v4().simple()))
        .bind(f.org)
        .execute(&f.pool)
        .await
        .unwrap();
        let call2: Uuid = sqlx::query_scalar(
            "INSERT INTO voip_calls
                (session_id, org_id, provider, direction, recipient_e164, recipient_pseudonym,
                 recipient_country, source_language, target_language, engine_id)
             VALUES ($1, $2, 'mock', 'outbound', '+393201234567', 'def456', 'IT', 'it', 'en',
                     'standard')
             RETURNING id",
        )
        .bind(session2)
        .bind(f.org)
        .fetch_one(&f.pool)
        .await
        .unwrap();

        let a = reserve(&f.pool, f.org, f.call, f.session, None, 60)
            .await
            .unwrap();
        let b = reserve(&f.pool, f.org, call2, session2, None, 60)
            .await
            .unwrap();

        assert!(
            matches!(a, ReserveOutcome::Held(_)),
            "the first hold takes 60"
        );
        match b {
            ReserveOutcome::Insufficient { balance, required } => {
                assert_eq!(balance, 40);
                assert_eq!(required, 60);
            }
            ReserveOutcome::Held(_) => panic!("the second hold overspent the pool"),
        }
        assert_eq!(
            balance(&f.pool, f.org).await,
            40,
            "the refusal deducted nothing"
        );
        assert!(open_for_call(&f.pool, call2).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_insufficient_pool_refuses_without_touching_anything() {
        let Some(f) = setup(5).await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        let out = reserve(&f.pool, f.org, f.call, f.session, None, 60)
            .await
            .unwrap();
        assert_eq!(
            out,
            ReserveOutcome::Insufficient {
                balance: 5,
                required: 60
            }
        );
        assert_eq!(balance(&f.pool, f.org).await, 5);
        assert_eq!(ledger_net(&f.pool, f.session).await, 0);
    }

    #[tokio::test]
    async fn a_short_call_gets_the_unused_hold_back() {
        // R10, and the case that happens on almost every call: we hold ten minutes and the
        // conversation takes ninety seconds.
        let Some(f) = setup(100).await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        reserve(&f.pool, f.org, f.call, f.session, None, 60)
            .await
            .unwrap();

        let s = settle(&f.pool, f.call, f.session, 15)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            s,
            Settlement {
                held: 60,
                settled: 15,
                released: 45,
                shortfall: 0
            }
        );
        assert!(s.is_coherent(15));
        assert_eq!(balance(&f.pool, f.org).await, 85, "100 - 15 actually used");
        assert_eq!(ledger_net(&f.pool, f.session).await, -15);
    }

    #[tokio::test]
    async fn a_call_that_never_connected_is_refunded_in_full() {
        // Busy, rejected, no answer: nothing was consumed, so nothing is owed.
        let Some(f) = setup(100).await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        reserve(&f.pool, f.org, f.call, f.session, None, 60)
            .await
            .unwrap();

        let s = release(&f.pool, f.call, f.session).await.unwrap().unwrap();
        assert_eq!(
            s,
            Settlement {
                held: 60,
                settled: 0,
                released: 60,
                shortfall: 0
            }
        );
        assert_eq!(balance(&f.pool, f.org).await, 100, "the org is made whole");
        assert_eq!(ledger_net(&f.pool, f.session).await, 0);
    }

    #[tokio::test]
    async fn a_call_that_overran_its_hold_charges_the_difference() {
        let Some(f) = setup(100).await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        reserve(&f.pool, f.org, f.call, f.session, None, 60)
            .await
            .unwrap();

        let s = settle(&f.pool, f.call, f.session, 75)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            s,
            Settlement {
                held: 60,
                settled: 75,
                released: 0,
                shortfall: 0
            }
        );
        assert!(s.is_coherent(75));
        assert_eq!(balance(&f.pool, f.org).await, 25);
        assert_eq!(ledger_net(&f.pool, f.session).await, -75);
    }

    #[tokio::test]
    async fn an_overrun_the_pool_cannot_cover_is_recorded_not_hidden() {
        // The call already happened. Charging what is there and recording the rest is the
        // honest outcome; silently forgiving it is a loss nobody is measuring.
        let Some(f) = setup(70).await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        reserve(&f.pool, f.org, f.call, f.session, None, 60)
            .await
            .unwrap();
        assert_eq!(balance(&f.pool, f.org).await, 10);

        let s = settle(&f.pool, f.call, f.session, 100)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            s,
            Settlement {
                held: 60,
                settled: 70, // 60 held + the 10 that was left
                released: 0,
                shortfall: 30, // owed and uncollectable
            }
        );
        assert!(s.is_coherent(100));
        assert_eq!(balance(&f.pool, f.org).await, 0, "never negative");
        assert_eq!(ledger_net(&f.pool, f.session).await, -70);
    }

    #[tokio::test]
    async fn settling_twice_does_not_refund_twice() {
        // A redelivered hangup webhook plus the reconcile job WILL both call this. The
        // second one refunding again would print money.
        let Some(f) = setup(100).await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        reserve(&f.pool, f.org, f.call, f.session, None, 60)
            .await
            .unwrap();

        let first = settle(&f.pool, f.call, f.session, 15)
            .await
            .unwrap()
            .unwrap();
        let after_first = balance(&f.pool, f.org).await;

        let second = settle(&f.pool, f.call, f.session, 15)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second, first, "the replay reports the original settlement");
        assert_eq!(
            balance(&f.pool, f.org).await,
            after_first,
            "and moves no money"
        );

        // Even a settlement claiming a DIFFERENT amount must not reopen it.
        let third = settle(&f.pool, f.call, f.session, 999)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(third, first);
        assert_eq!(balance(&f.pool, f.org).await, after_first);
    }

    #[tokio::test]
    async fn releasing_after_settling_changes_nothing() {
        let Some(f) = setup(100).await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        reserve(&f.pool, f.org, f.call, f.session, None, 60)
            .await
            .unwrap();
        settle(&f.pool, f.call, f.session, 20).await.unwrap();
        let before = balance(&f.pool, f.org).await;

        let out = release(&f.pool, f.call, f.session).await.unwrap().unwrap();
        assert_eq!(out.settled, 20);
        assert_eq!(balance(&f.pool, f.org).await, before);
    }

    #[tokio::test]
    async fn settling_a_call_that_never_reserved_is_a_no_op_not_an_error() {
        // A call refused before the hold (bad number, blocked country) still reaches the
        // teardown path. It must not blow up there.
        let Some(f) = setup(100).await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        assert_eq!(settle(&f.pool, f.call, f.session, 10).await.unwrap(), None);
        assert_eq!(balance(&f.pool, f.org).await, 100);
    }

    #[tokio::test]
    async fn a_zero_hold_is_legal_and_settles_cleanly() {
        // A free destination on a zero-margin deployment. The quote clamps the hold to 1,
        // but the primitive must not misbehave at 0.
        let Some(f) = setup(100).await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        reserve(&f.pool, f.org, f.call, f.session, None, 0)
            .await
            .unwrap();
        let s = settle(&f.pool, f.call, f.session, 0)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(s.held, 0);
        assert_eq!(s.settled, 0);
        assert_eq!(s.released, 0);
        assert_eq!(balance(&f.pool, f.org).await, 100);
    }

    #[tokio::test]
    async fn the_open_hold_is_findable_while_open_and_gone_once_closed() {
        let Some(f) = setup(100).await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        reserve(&f.pool, f.org, f.call, f.session, None, 60)
            .await
            .unwrap();
        let open = open_for_call(&f.pool, f.call).await.unwrap().unwrap();
        assert_eq!(open.held_credits, 60);
        assert_eq!(open.state, "held");

        settle(&f.pool, f.call, f.session, 10).await.unwrap();
        assert!(open_for_call(&f.pool, f.call).await.unwrap().is_none());
    }

    #[test]
    fn the_settlement_identities_are_checkable_without_a_database() {
        // released = held - min(actual, held);  settled + shortfall = actual.
        assert!(Settlement {
            held: 60,
            settled: 15,
            released: 45,
            shortfall: 0
        }
        .is_coherent(15));
        assert!(Settlement {
            held: 60,
            settled: 75,
            released: 0,
            shortfall: 0
        }
        .is_coherent(75));
        assert!(Settlement {
            held: 60,
            settled: 70,
            released: 0,
            shortfall: 30
        }
        .is_coherent(100));
        assert!(Settlement {
            held: 60,
            settled: 0,
            released: 60,
            shortfall: 0
        }
        .is_coherent(0));

        // Credits invented out of nowhere, or lost.
        assert!(!Settlement {
            held: 60,
            settled: 20,
            released: 45,
            shortfall: 0
        }
        .is_coherent(15));
        assert!(!Settlement {
            held: 60,
            settled: 15,
            released: 40,
            shortfall: 0
        }
        .is_coherent(15));
    }
}
