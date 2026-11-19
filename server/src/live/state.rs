//! Shared Live Connect state, carried on `AppState`.

use std::collections::HashMap;
use std::sync::Mutex;

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
        }
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
