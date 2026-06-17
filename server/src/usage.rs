//! Real-time usage metering for a single speaking session.
//!
//! While a user is actively speaking (a Deepgram session is open), the meter
//! deducts credits every `interval_secs`. It pushes a `balance_update` to the
//! speaker after each charge, warns once with `low_balance` when crossing the
//! threshold, and on exhaustion emits `balance_exhausted` and signals the caller
//! (via `exhaust_tx`) to drop the audio session — the WebRTC call stays up.
//!
//! Guests aren't billed; with `GUEST_MAX_MINUTES` set they get a time cap via
//! [`run_guest_meter`] instead (cumulative across speaking bursts).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rust_decimal::prelude::ToPrimitive;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::rooms::PeerTx;
use uuid::Uuid;

use crate::billing::{usd, BillingError, BillingService};
use crate::protocol::ServerMessage;

/// Per-session metering parameters.
#[derive(Clone)]
pub struct MeterConfig {
    pub interval_secs: u64,
    pub rate_per_second: f64,
    pub low_balance_threshold: f64,
    /// When `Some`, the meter skips billing ticks where no other participant
    /// speaks a different language (all-same-language room = no translations).
    pub rooms: Option<std::sync::Arc<crate::rooms::RoomManager>>,
    pub room: String,
    pub speaker_id: String,
    pub speaker_lang: String,
    /// Bill per distinct target language instead of a flat rate (spec 0093). The
    /// Premium engine opens one paid OpenAI session per language, so a group call
    /// genuinely costs the speaker more — the charge scales with the room.
    pub scale_by_target_count: bool,
}

/// How many translation streams to bill for this tick, or `None` to skip the tick
/// entirely (everyone shares the speaker's language → nothing is translated).
/// `scale_per_language` engines (Premium) bill one stream per distinct target
/// language; others bill a single flat stream regardless of fan-out. Pure — for
/// tests. `targets` is the distinct languages present (excluding the speaker by id;
/// `"auto"` is already filtered out by the room).
pub fn billable_streams(
    targets: &[String],
    speaker_lang: &str,
    scale_per_language: bool,
) -> Option<usize> {
    let distinct = targets
        .iter()
        .filter(|t| t.as_str() != speaker_lang)
        .count();
    if distinct == 0 {
        return None;
    }
    Some(if scale_per_language { distinct } else { 1 })
}

/// Charge a billed user for one speaking session. Runs until `cancel` resolves
/// (Stop / disconnect) or credits run out.
pub async fn run_usage_meter(
    billing: BillingService,
    user_id: Uuid,
    session_id: Uuid,
    cfg: MeterConfig,
    out_tx: PeerTx,
    exhaust_tx: UnboundedSender<()>,
    mut cancel: oneshot::Receiver<()>,
) {
    let interval = cfg.interval_secs.max(1);
    let mut ticker = tokio::time::interval(Duration::from_secs(interval));
    ticker.tick().await; // consume the immediate first tick (charge in arrears)
    let mut warned_low = false;

    loop {
        tokio::select! {
            _ = &mut cancel => break,
            _ = ticker.tick() => {
                // Decide how many translation streams to bill. When everyone shares
                // the speaker's language nothing is translated → skip the tick.
                // Premium scales the charge by the number of distinct target
                // languages (one paid OpenAI session each); others bill flat.
                let streams = match cfg.rooms.as_ref() {
                    Some(rooms) => {
                        let targets = rooms.get_room_languages(&cfg.room, &cfg.speaker_id);
                        match billable_streams(&targets, &cfg.speaker_lang, cfg.scale_by_target_count) {
                            Some(n) => n,
                            None => continue,
                        }
                    }
                    None => 1,
                };
                let amount = usd(cfg.rate_per_second * interval as f64 * streams as f64);
                match billing
                    .deduct_usage(user_id, Some(session_id), interval as i32, amount)
                    .await
                {
                    Ok(balance) => {
                        let bal = balance.to_f64().unwrap_or(0.0);
                        let _ = out_tx.send(ServerMessage::BalanceUpdate { balance: bal }.to_json());
                        if !warned_low && bal < cfg.low_balance_threshold {
                            warned_low = true;
                            let _ = out_tx
                                .send(ServerMessage::LowBalance { balance: bal }.to_json());
                        }
                    }
                    Err(BillingError::InsufficientFunds) => {
                        let _ = out_tx.send(ServerMessage::BalanceExhausted.to_json());
                        let _ = exhaust_tx.send(());
                        break;
                    }
                    Err(e) => {
                        tracing::error!("usage deduct failed: {e}");
                        break;
                    }
                }
            }
        }
    }
}

/// Cap a guest's cumulative speaking time. `spent` accumulates across speaking
/// bursts; once it reaches `cap_secs` the audio is stopped (no billing).
pub async fn run_guest_meter(
    spent: Arc<AtomicU64>,
    cap_secs: u64,
    interval_secs: u64,
    out_tx: PeerTx,
    exhaust_tx: UnboundedSender<()>,
    mut cancel: oneshot::Receiver<()>,
) {
    let interval = interval_secs.max(1);
    let mut ticker = tokio::time::interval(Duration::from_secs(interval));
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = &mut cancel => break,
            _ = ticker.tick() => {
                let total = spent.fetch_add(interval, Ordering::SeqCst) + interval;
                if total >= cap_secs {
                    let _ = out_tx.send(ServerMessage::BalanceExhausted.to_json());
                    let _ = exhaust_tx.send(());
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn billable_streams_flat_vs_per_language() {
        let two = vec!["en".to_string(), "fr".to_string()];
        // All same language as the speaker → nothing to translate → skip.
        assert_eq!(billable_streams(&["it".to_string()], "it", true), None);
        assert_eq!(billable_streams(&[], "it", true), None);
        // Flat engines (Standard) bill one stream regardless of how many targets.
        assert_eq!(billable_streams(&two, "it", false), Some(1));
        // Per-language engines (Premium) bill one stream per distinct target — a
        // group call with two other languages costs 2× a 1:1 call.
        assert_eq!(billable_streams(&two, "it", true), Some(2));
        // The speaker's own language among the targets isn't a billable stream.
        assert_eq!(
            billable_streams(&["it".to_string(), "en".to_string()], "it", true),
            Some(1)
        );
    }

    #[tokio::test]
    async fn guest_meter_stops_at_cap() {
        let spent = Arc::new(AtomicU64::new(0));
        let (out_tx, mut out_rx, _out_overflow) = PeerTx::channel(crate::rooms::OUT_CHANNEL_CAP);
        let (exhaust_tx, mut exhaust_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = oneshot::channel();

        // cap 2s, tick 1s -> exhausts after the second tick.
        tokio::time::timeout(
            Duration::from_secs(5),
            run_guest_meter(spent.clone(), 2, 1, out_tx, exhaust_tx, cancel_rx),
        )
        .await
        .expect("meter should finish at cap");

        assert!(exhaust_rx.try_recv().is_ok(), "exhaust signalled");
        let msg = out_rx.try_recv().expect("a message was sent");
        assert!(msg.contains("balance_exhausted"));
        assert!(spent.load(Ordering::SeqCst) >= 2);
    }

    /// DB-gated: a billed meter deducts each interval, pushes `balance_update`,
    /// warns once with `low_balance`, and finally `balance_exhausted` + signals.
    /// Uses real 1s intervals (≈3s) with an aggressive rate so the balance drains
    /// in three ticks. Skipped without `DATABASE_URL`.
    #[tokio::test]
    async fn billed_meter_update_low_then_exhaust() {
        use rust_decimal::Decimal;
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        let pool = crate::db::connect(&url).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let svc = BillingService::new(pool.clone(), Decimal::ZERO);

        let uid: Uuid = sqlx::query_scalar(
            "INSERT INTO users (google_id, email, name, balance)
             VALUES ($1, $2, 'M', $3) RETURNING id",
        )
        .bind(format!("g-{}", Uuid::new_v4()))
        .bind(format!("{}@x.com", Uuid::new_v4()))
        .bind(Decimal::new(30, 2)) // 0.30
        .fetch_one(&pool)
        .await
        .unwrap();
        // Tag this session with the Premium engine (spec 0093) — asserted below.
        let sid = svc.create_session(uid, "room-m", "premium").await.unwrap();

        let (out_tx, mut out_rx, _out_overflow) = PeerTx::channel(crate::rooms::OUT_CHANNEL_CAP);
        let (exhaust_tx, mut exhaust_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        let cfg = MeterConfig {
            interval_secs: 1,
            rate_per_second: 0.125, // 0.125 per 1s tick: 0.30 -> 0.175 -> 0.05 -> exhaust
            low_balance_threshold: 1.0,
            rooms: None,
            room: String::new(),
            speaker_id: String::new(),
            speaker_lang: String::new(),
            scale_by_target_count: false,
        };

        tokio::time::timeout(
            Duration::from_secs(10),
            run_usage_meter(svc, uid, sid, cfg, out_tx, exhaust_tx, cancel_rx),
        )
        .await
        .expect("meter finishes on exhaust");

        let mut msgs = Vec::new();
        while let Ok(m) = out_rx.try_recv() {
            msgs.push(m);
        }
        assert!(msgs.iter().any(|m| m.contains("balance_update")), "update");
        let lows = msgs.iter().filter(|m| m.contains("low_balance")).count();
        assert_eq!(lows, 1, "low_balance warns exactly once");
        assert!(
            msgs.iter().any(|m| m.contains("balance_exhausted")),
            "exhaust"
        );
        assert!(exhaust_rx.try_recv().is_ok(), "exhaust signalled");

        // Two successful 1s deductions were recorded against the session, and the
        // engine tag (spec 0093) persisted.
        let (secs, cost, balance, engine_id): (i32, Decimal, Decimal, String) = sqlx::query_as(
            "SELECT s.speaking_seconds, s.cost, u.balance, s.engine_id
             FROM usage_sessions s JOIN users u ON u.id = s.user_id
             WHERE s.id = $1",
        )
        .bind(sid)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(secs, 2);
        assert_eq!(cost, Decimal::new(25, 2)); // 0.25
        assert_eq!(balance, Decimal::new(5, 2)); // 0.05, never negative
        assert_eq!(engine_id, "premium"); // tagged at create_session
    }

    #[tokio::test]
    async fn guest_meter_cancels_cleanly() {
        let spent = Arc::new(AtomicU64::new(0));
        let (out_tx, _out_rx, _out_overflow) = PeerTx::channel(crate::rooms::OUT_CHANNEL_CAP);
        let (exhaust_tx, mut exhaust_rx) = tokio::sync::mpsc::unbounded_channel();
        let (cancel_tx, cancel_rx) = oneshot::channel();

        let handle = tokio::spawn(run_guest_meter(
            spent, 3600, 1, out_tx, exhaust_tx, cancel_rx,
        ));
        // Cancel before the cap is ever reached.
        drop(cancel_tx);
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("meter exits on cancel")
            .unwrap();
        assert!(exhaust_rx.try_recv().is_err(), "no exhaust on cancel");
    }
}
