//! Billing for Live Connect: who pays, and how a charge can never happen twice.
//!
//! The **owner** pays for every seat — a user's personal balance (B2C) or the org wallet (B2B).
//! Guests are never charged and never see the owner's balance. Every phone is its own meter, and
//! each meter bills one stream per DISTINCT target language present in the room other than the
//! speaker's own, as a webinar does: host `it` with guests `es`, `es`, `en` pays two streams for
//! the host's speech (es, en), two for a Spanish guest (it, en), two for the English one (it, es);
//! two guests in one language never double a cost, and a room that all speaks one language is free.
//!
//! ## Why a tick can only be charged once
//!
//! The existing meter debits and moves on; a second meter for the same speaker (a phone that
//! reconnects before its old socket is torn down) would debit twice. Here every charge writes a
//! `live_usage_ticks` row keyed by `(session, peer, tick_seq)` **in the same transaction** as the
//! debit, `ON CONFLICT DO NOTHING`. The tick number is derived from the wall clock, so two meters
//! covering the same interval compute the same number and only the first one's transaction
//! commits a charge. A lost race costs nothing; a skipped tick after a stall costs the user
//! nothing either — the failure mode is under-charging, never double-charging.
//!
//! ## Never trust the client
//!
//! The meter runs on the server for as long as the engine session is open. Nothing a client
//! sends changes what it charges.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use super::state::LiveState;
use crate::billing::{deduct_usage_tx, usd, BillingError};
use crate::business::credits::{deduct_org_credits_tx, OrgCharge};
use crate::db::Pool;
use crate::protocol::ServerMessage;
use crate::rooms::RoomManager;
use crate::usage::billable_streams;

/// Ledger `type` for an org's Live Connect usage.
const ORG_LEDGER_TYPE: &str = "live_connect";
/// Consecutive failed charges before the conversation is closed rather than left free.
const MAX_CONSECUTIVE_ERRORS: u32 = 6;

/// Who is billed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivePayer {
    /// A consumer's personal balance, accumulating on this `usage_sessions` row.
    User { user_id: Uuid, usage_session: Uuid },
    /// A business wallet; the ledger row references the conversation's `call_sessions` row.
    Org { org_id: Uuid, call_session: Uuid },
}

/// What one attempted charge did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tick {
    /// Debited. `balance` is the consumer's new balance (an org's is not shown to anyone).
    Charged { balance: Option<Decimal> },
    /// This tick had already been charged: nothing was debited.
    Duplicate,
    /// The payer cannot cover it: nothing was debited.
    Insufficient,
}

/// Why a meter stopped the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndCause {
    BalanceExhausted,
    /// The database kept refusing charges; ending beats translating for free indefinitely.
    BillingError,
}

/// What one tick costs, in the unit each kind of payer is billed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Charge {
    /// Seconds of translation this tick covers (what a usage session accumulates).
    pub seconds: i32,
    /// What a consumer pays.
    pub usd: Decimal,
    /// What an org pays: a whole number of credits, 1 credit = 1 cent.
    pub credits: i32,
}

/// Charge one tick, atomically and at most once. Only the part of `charge` that matches the
/// `payer` is used.
pub async fn charge_tick(
    pool: &Pool,
    payer: &LivePayer,
    session: Uuid,
    peer: Uuid,
    tick_seq: i64,
    charge: Charge,
) -> Result<Tick, BillingError> {
    let Charge {
        seconds,
        usd: usd_amount,
        credits,
    } = charge;
    let (payer_kind, recorded) = match payer {
        LivePayer::User { .. } => ("user", usd_amount),
        LivePayer::Org { .. } => ("org", Decimal::from(credits)),
    };
    let mut tx = pool.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO live_usage_ticks (session_id, peer_id, tick_seq, payer_kind, amount)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (session_id, peer_id, tick_seq) DO NOTHING",
    )
    .bind(session)
    .bind(peer)
    .bind(tick_seq)
    .bind(payer_kind)
    .bind(recorded)
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
        // Dropping `tx` rolls it back: nothing to keep.
        return Ok(Tick::Duplicate);
    }

    match payer {
        LivePayer::User {
            user_id,
            usage_session,
        } => match deduct_usage_tx(&mut tx, *user_id, Some(*usage_session), seconds, usd_amount)
            .await
        {
            Ok(balance) => {
                tx.commit().await?;
                Ok(Tick::Charged {
                    balance: Some(balance),
                })
            }
            // The tick row is rolled back with the failed debit, so a retry after a top-up is
            // not mistaken for a duplicate.
            Err(BillingError::InsufficientFunds) => Ok(Tick::Insufficient),
            Err(e) => Err(e),
        },
        LivePayer::Org {
            org_id,
            call_session,
        } => match deduct_org_credits_tx(
            &mut tx,
            *org_id,
            credits,
            ORG_LEDGER_TYPE,
            Some(*call_session),
            None,
            "Live Connect translation",
        )
        .await
        .map_err(BillingError::from)?
        {
            OrgCharge::Charged { .. } => {
                tx.commit().await?;
                Ok(Tick::Charged { balance: None })
            }
            OrgCharge::Insufficient { .. } => Ok(Tick::Insufficient),
        },
    }
}

/// What a conversation has cost its personal-wallet owner so far: the sum of its user-paid
/// ticks. (An organisation's whole-credit ticks are not a wallet amount and are not shown.)
pub async fn session_spent(pool: &Pool, session: Uuid) -> Result<f64, sqlx::Error> {
    let spent: Decimal = sqlx::query_scalar(
        "SELECT COALESCE(sum(amount), 0) FROM live_usage_ticks
         WHERE session_id = $1 AND payer_kind = 'user'",
    )
    .bind(session)
    .fetch_one(pool)
    .await?;
    Ok(spent.to_f64().unwrap_or(0.0))
}

/// Everything one phone's meter needs.
pub struct LiveMeter {
    pub pool: Pool,
    pub live: Arc<LiveState>,
    pub rooms: Arc<RoomManager>,
    pub payer: LivePayer,
    pub session: Uuid,
    pub peer: Uuid,
    pub room: String,
    pub speaker_lang: String,
    pub interval_secs: u64,
    pub rate_per_second: f64,
    pub low_balance_threshold: f64,
    /// Audio frames this socket has forwarded to the engine since the last tick. What is billed
    /// is audio that actually reached the engine, not a flag the client can flip: gating on mute
    /// state at the tick instant let a phone mute itself around each tick and translate for free.
    pub activity: Arc<AtomicU64>,
}

/// The tick number for "now": the same for every meter covering the same interval, which is
/// what makes a duplicated meter harmless.
pub fn tick_seq(now_unix: i64, interval_secs: u64) -> i64 {
    now_unix / interval_secs.max(1) as i64
}

impl LiveMeter {
    /// Run until cancelled, the payer cannot cover a tick, or charging keeps failing.
    pub async fn run(
        self,
        end_tx: mpsc::UnboundedSender<EndCause>,
        mut cancel: oneshot::Receiver<()>,
    ) {
        let interval = self.interval_secs.max(1);
        let mut ticker = tokio::time::interval(Duration::from_secs(interval));
        // After a stall, skip the missed ticks instead of firing them back to back: a skipped
        // tick under-charges, a burst of them could not be told apart from a duplicate.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // The first interval is charged the moment the meter starts (see `first` below), then
        // one more every `interval`. Charging only in arrears made a Start/Stop shorter than one
        // interval free: the meter was cancelled before its first charge, while the engine had
        // already been translating the whole time.
        let mut first = true;
        let mut warned_low = false;
        let mut errors = 0u32;

        loop {
            tokio::select! {
                _ = &mut cancel => break,
                _ = ticker.tick() => {
                    let is_first = std::mem::take(&mut first);
                    // After the opening charge, an interval in which no audio reached the engine
                    // costs nothing. The opening charge is unconditional: an engine session was
                    // opened for this Start whether or not any audio followed.
                    let forwarded = self.activity.swap(0, Ordering::Relaxed);
                    if !is_first && forwarded == 0 {
                        continue;
                    }
                    // Nothing to translate while everyone shares this speaker's language. Otherwise
                    // one stream per DISTINCT target language present, whatever the engine's
                    // metadata says about its own pricing: the upstream cost of a Live Connect
                    // phone is per speaker per target language, and the host pays for every seat
                    // (two guests in the same language cost one stream, not two).
                    let targets = self.rooms.get_room_languages(&self.room, &self.peer.to_string());
                    let Some(streams) = billable_streams(&targets, &self.speaker_lang, true)
                    else {
                        continue;
                    };
                    let amount = usd(self.rate_per_second * interval as f64 * streams as f64);
                    // The wall-clock bucket makes two meters covering the same interval agree, so
                    // one of them is harmless. The opening charge of a run cannot use it: a phone
                    // that starts and stops several times inside one bucket would land every
                    // start on the same number and pay for only the first. Each start gets a key
                    // of its own (negative, so it can never collide with a bucket).
                    let seq = if is_first {
                        -chrono::Utc::now().timestamp_micros()
                    } else {
                        tick_seq(chrono::Utc::now().timestamp(), interval)
                    };
                    let credits = match self.payer {
                        LivePayer::User { .. } => 0,
                        // The fraction of a credit is carried per seat for the whole session,
                        // not per Start: a Start/Stop shorter than a credit is not free.
                        LivePayer::Org { .. } => match self.live.take_owed(
                            self.session,
                            self.peer,
                            amount,
                            (!is_first).then_some(seq),
                        ) {
                            0 => continue, // still under a cent; carried to the next tick
                            c => c,
                        },
                    };

                    match charge_tick(
                        &self.pool,
                        &self.payer,
                        self.session,
                        self.peer,
                        seq,
                        Charge { seconds: interval as i32, usd: amount, credits },
                    ).await
                    {
                        Ok(Tick::Charged { balance }) => {
                            errors = 0;
                            if let Some(b) = balance {
                                let bal = b.to_f64().unwrap_or(0.0);
                                self.live.notify_owner(
                                    self.session,
                                    ServerMessage::BalanceUpdate { balance: bal }.to_json(),
                                );
                                // What the conversation has cost so far, from the tick ledger
                                // (the source of truth, restart-proof). Informational: a failed
                                // read only means the figure waits for the next tick.
                                if let Ok(spent) = session_spent(&self.pool, self.session).await {
                                    self.live.notify_owner(
                                        self.session,
                                        ServerMessage::SessionSpent { spent }.to_json(),
                                    );
                                }
                                if !warned_low && bal < self.low_balance_threshold {
                                    warned_low = true;
                                    self.live.notify_owner(
                                        self.session,
                                        ServerMessage::LowBalance { balance: bal }.to_json(),
                                    );
                                }
                            }
                        }
                        Ok(Tick::Duplicate) => {
                            // Not charged, so the credits taken from the carry were not spent.
                            self.live.put_back_owed(self.session, self.peer, credits);
                        }
                        Ok(Tick::Insufficient) => {
                            tracing::info!(session = %self.session, "live: payer cannot cover usage");
                            let _ = end_tx.send(EndCause::BalanceExhausted);
                            break;
                        }
                        Err(e) => {
                            self.live.put_back_owed(self.session, self.peer, credits);
                            errors += 1;
                            tracing::error!(session = %self.session, error = %e, errors, "live: charge failed");
                            if errors >= MAX_CONSECUTIVE_ERRORS {
                                let _ = end_tx.send(EndCause::BillingError);
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meters_covering_the_same_interval_agree_on_the_tick() {
        // Two meters that start a second apart still land on one number per interval.
        assert_eq!(tick_seq(1_000, 5), tick_seq(1_004, 5));
        assert_ne!(tick_seq(1_004, 5), tick_seq(1_005, 5));
        // Consecutive intervals of one meter are always different ticks.
        assert_eq!(tick_seq(1_005, 5), tick_seq(1_000, 5) + 1);
    }

    #[test]
    fn a_zero_interval_is_clamped_not_a_division_by_zero() {
        assert_eq!(tick_seq(100, 0), 100);
    }
}
