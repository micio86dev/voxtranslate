//! Housekeeping for conversations nobody is going to end.
//!
//! A phone that loses signal does not say goodbye. Two things must still happen: a
//! conversation nobody joined in time stops being joinable, and a conversation whose phones
//! have both gone — and stayed gone past the reconnect grace — stops being billable and
//! stops holding the owner's concurrent-session slot.
//!
//! Presence is read from the live room registry (the one place that knows who is connected);
//! the verdict is written to the database, which is what everything else trusts.

use std::time::Duration;

use serde_json::json;
use uuid::Uuid;

use super::sessions;
use super::ws::room_name;
use crate::AppState;

/// How often the sweep runs. Finer than the reconnect grace, coarse enough to cost nothing.
const SWEEP_EVERY: Duration = Duration::from_secs(15);

/// Run forever. Spawned once at startup, only when the feature is enabled.
pub async fn run(state: AppState) {
    let mut tick = tokio::time::interval(SWEEP_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        if let Err(e) = sweep_once(&state).await {
            tracing::warn!(error = %e, "live: sweep failed");
        }
    }
}

/// One pass over every open conversation.
pub async fn sweep_once(state: &AppState) -> Result<u64, sqlx::Error> {
    sweep_sessions(state, None).await
}

/// One pass, optionally limited to `only` these sessions for the abandonment check.
///
/// Rooms live in THIS process's memory, so "nobody is in the room" is only meaningful for
/// sessions this process serves. That holds in production (one replica); it does not hold for
/// tests, where many servers share one database — there a pass must be scoped to the session
/// under test, or it would end its neighbours' conversations.
pub async fn sweep_sessions(state: &AppState, only: Option<&[Uuid]>) -> Result<u64, sqlx::Error> {
    let Some(pool) = state.pool.as_ref() else {
        return Ok(0);
    };
    let cfg = &state.live.config;
    let mut changed = sessions::expire_stale(pool, cfg).await?;

    let active: Vec<Uuid> = match only {
        None => {
            sqlx::query_scalar("SELECT id FROM live_sessions WHERE status = 'active'")
                .fetch_all(pool)
                .await?
        }
        Some(ids) => {
            sqlx::query_scalar(
                "SELECT id FROM live_sessions WHERE status = 'active' AND id = ANY($1)",
            )
            .bind(ids)
            .fetch_all(pool)
            .await?
        }
    };
    // Only a full pass sees every active session; a scoped one must not forget the others.
    if only.is_none() {
        state.live.retain_muted_for(&active);
    }
    let grace = Duration::from_secs(cfg.reconnect_grace_secs.max(0) as u64);
    let now = tokio::time::Instant::now();

    let mut abandoned = Vec::new();
    {
        // A poisoned lock only means another pass panicked mid-update; the map is a set of
        // timestamps and stays coherent, so keep sweeping rather than kill the one task that
        // expires invitations and frees owners' session slots.
        let mut vacant = state
            .live
            .vacant
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Forget sessions that are no longer active, so the map cannot grow without bound.
        vacant.retain(|id, _| active.contains(id));
        for id in &active {
            // `get_room_languages` lists the peers in the room; none means nobody is on it.
            let empty = state
                .rooms
                .get_room_languages(&room_name(*id), "")
                .is_empty();
            if !empty {
                vacant.remove(id);
                continue;
            }
            let since = *vacant.entry(*id).or_insert(now);
            if now.duration_since(since) >= grace {
                abandoned.push(*id);
                vacant.remove(id);
            }
        }
    }
    for id in abandoned {
        if sessions::end_session(pool, id, "abandoned").await? {
            sessions::record_event(pool, id, "abandoned", json!({})).await;
            changed += 1;
        }
    }
    Ok(changed)
}
