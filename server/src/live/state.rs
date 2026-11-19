//! Shared Live Connect state, carried on `AppState`.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;
use uuid::Uuid;

use super::config::LiveConfig;
use crate::rooms::PeerTx;

/// Configuration plus the little in-memory state the sweeper needs.
///
/// Rooms themselves live in `RoomManager`; this only remembers, per active session, since
/// when nobody has been connected to it.
pub struct LiveState {
    pub config: LiveConfig,
    pub(crate) vacant: Mutex<HashMap<Uuid, Instant>>,
    /// The owner side's outbound channel per session, so a meter running for the GUEST's
    /// speech can still tell the owner their balance. Keyed by session, valued by the socket's
    /// connection id so a stale socket cannot unregister its replacement.
    owners: Mutex<HashMap<Uuid, (Uuid, PeerTx)>>,
    /// Seats that are muted, as `(session, peer)`. Kept here and not per socket, so a phone that
    /// reconnects is still muted: the server enforces mute, it does not trust the client to
    /// remember it.
    muted: Mutex<HashSet<(Uuid, Uuid)>>,
    /// When the hourly housekeeping (finalise stragglers, retention) last started.
    maintenance: Mutex<Option<Instant>>,
}

// Hand-written: `PeerTx` is a channel handle, and a `{:?}` of the state should show the
// configuration, never the live sockets.
impl std::fmt::Debug for LiveState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveState")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl LiveState {
    pub fn new(config: LiveConfig) -> Self {
        Self {
            config,
            vacant: Mutex::new(HashMap::new()),
            owners: Mutex::new(HashMap::new()),
            muted: Mutex::new(HashSet::new()),
            maintenance: Mutex::new(None),
        }
    }

    /// Whether the periodic housekeeping is due, and if so record that it starts now. The
    /// check and the stamp are one step, so two sweeps cannot both start it.
    pub fn maintenance_due(&self, every: Duration) -> bool {
        self.maintenance_due_at(Instant::now(), every)
    }

    fn maintenance_due_at(&self, now: Instant, every: Duration) -> bool {
        let mut last = self.maintenance.lock().unwrap_or_else(|p| p.into_inner());
        match *last {
            Some(at) if now.duration_since(at) < every => false,
            _ => {
                *last = Some(now);
                true
            }
        }
    }

    pub fn set_muted(&self, session: Uuid, peer: Uuid, muted: bool) {
        let mut set = self.muted.lock().unwrap_or_else(|p| p.into_inner());
        if muted {
            set.insert((session, peer));
        } else {
            set.remove(&(session, peer));
        }
    }

    pub fn is_muted(&self, session: Uuid, peer: Uuid) -> bool {
        self.muted
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(&(session, peer))
    }

    /// Forget mute flags of sessions that are no longer active, so the set cannot grow forever.
    pub fn retain_muted_for(&self, active: &[Uuid]) {
        self.muted
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|(session, _)| active.contains(session));
    }

    /// Register the owner side's channel for `session` (the newest socket wins).
    pub fn set_owner(&self, session: Uuid, conn: Uuid, tx: PeerTx) {
        self.owners
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(session, (conn, tx));
    }

    /// Unregister, but only if `conn` is still the registered socket.
    pub fn clear_owner(&self, session: Uuid, conn: Uuid) {
        let mut owners = self.owners.lock().unwrap_or_else(|p| p.into_inner());
        if owners.get(&session).is_some_and(|(c, _)| *c == conn) {
            owners.remove(&session);
        }
    }

    /// Send a frame to the owner's phone, if it is connected. Best effort: a balance push is
    /// information, never something a charge waits on.
    pub fn notify_owner(&self, session: Uuid, frame: String) {
        let tx = self
            .owners
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&session)
            .map(|(_, tx)| tx.clone());
        if let Some(tx) = tx {
            tx.send(frame);
        }
    }

    /// Read the process environment. A bad configuration leaves the feature **off** and says
    /// why at ERROR level: failing the whole server's boot over an optional feature would be
    /// the worse trade, but silently running it with a weakened key would be worse still.
    pub fn from_env() -> Self {
        match LiveConfig::load(|k| std::env::var(k).ok()) {
            Ok(config) => {
                if config.enabled {
                    tracing::info!("live connect enabled");
                }
                Self::new(config)
            }
            Err(e) => {
                tracing::error!(error = %e, "live connect misconfigured — feature stays off");
                Self::new(LiveConfig::disabled())
            }
        }
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled && self.config.keys.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maintenance_runs_at_most_once_per_interval() {
        let state = LiveState::new(LiveConfig::disabled());
        let hour = Duration::from_secs(3600);
        let t0 = Instant::now();
        assert!(
            state.maintenance_due_at(t0, hour),
            "the first sweep runs it"
        );
        assert!(!state.maintenance_due_at(t0 + Duration::from_secs(15), hour));
        assert!(!state.maintenance_due_at(t0 + Duration::from_secs(3599), hour));
        assert!(
            state.maintenance_due_at(t0 + hour, hour),
            "due again once the interval has passed"
        );
        assert!(
            !state.maintenance_due_at(t0 + hour + Duration::from_secs(1), hour),
            "and the new stamp restarts the clock"
        );
    }
}
