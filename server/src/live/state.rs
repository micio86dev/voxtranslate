//! Shared Live Connect state, carried on `AppState`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::time::Instant;
use uuid::Uuid;

use super::config::LiveConfig;
use crate::rooms::PeerTx;

/// Speech-to-translated-audio latency probes for one conversation (spec 0121, S18).
///
/// One slot per seat (`0` = owner side, `1` = guest side). The speaker's socket *stamps* its slot
/// when speech starts; the **other** phone's socket *takes* the stamp when the first translated
/// audio of that utterance is delivered to it, and the difference is the number the product is
/// judged on: how long after somebody starts talking the other person starts hearing them.
///
/// Lock-free on purpose. Stamping happens once per utterance and taking costs one atomic swap
/// per delivered audio chunk, so the hot path neither locks nor allocates. Stamps are nanoseconds
/// since `epoch` plus one, so `0` can mean "nothing pending".
#[derive(Debug)]
pub struct SessionLatency {
    epoch: tokio::time::Instant,
    stamps: [AtomicU64; 2],
}

/// An utterance older than this when its audio finally arrives was never going to be a latency
/// sample (the translation was skipped, or the speaker went on to something else): dropped, not
/// recorded, so a stale stamp cannot poison the histogram.
const MAX_PLAUSIBLE_LATENCY: Duration = Duration::from_secs(30);

impl SessionLatency {
    fn new() -> Self {
        Self {
            epoch: tokio::time::Instant::now(),
            stamps: [AtomicU64::new(0), AtomicU64::new(0)],
        }
    }

    /// The seat index of a role: the owner side (host, staff) is `0`, the guest `1`.
    pub fn seat(is_guest: bool) -> usize {
        usize::from(is_guest)
    }

    /// `seat` started speaking at `at`. Overwrites whatever was pending: a new utterance
    /// supersedes one whose audio never arrived.
    pub fn speech_started(&self, seat: usize, at: tokio::time::Instant) {
        let ns = at.saturating_duration_since(self.epoch).as_nanos() as u64;
        self.stamps[seat & 1].store(ns.saturating_add(1), Ordering::Relaxed);
    }

    /// Translated audio of the **other** seat's speech was delivered to `listener` at `at`.
    /// Returns the latency of the utterance it answers, exactly once per utterance: a second
    /// chunk of the same utterance finds nothing pending.
    pub fn audio_delivered(&self, listener: usize, at: tokio::time::Instant) -> Option<Duration> {
        let stamp = self.stamps[(listener & 1) ^ 1].swap(0, Ordering::Relaxed);
        if stamp == 0 {
            return None;
        }
        let now = at.saturating_duration_since(self.epoch);
        let started = Duration::from_nanos(stamp - 1);
        let elapsed = now.checked_sub(started)?;
        (elapsed <= MAX_PLAUSIBLE_LATENCY).then_some(elapsed)
    }
}

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
    /// Latency probes per session. Weak: a probe lives exactly as long as a socket of its
    /// session holds it, so nothing here needs the sweeper to clean up after a conversation.
    latency: Mutex<HashMap<Uuid, Weak<SessionLatency>>>,
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
            latency: Mutex::new(HashMap::new()),
        }
    }

    /// The latency probes of `session`, shared by both of its sockets. Taken once per socket at
    /// connect, never per frame. Dead entries are pruned here, when the map has grown past what
    /// a busy process has live, so the map stays proportional to open conversations.
    pub fn latency_for(&self, session: Uuid) -> Arc<SessionLatency> {
        let mut map = self.latency.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(live) = map.get(&session).and_then(Weak::upgrade) {
            return live;
        }
        if map.len() >= 64 {
            map.retain(|_, w| w.strong_count() > 0);
        }
        let fresh = Arc::new(SessionLatency::new());
        map.insert(session, Arc::downgrade(&fresh));
        fresh
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

    fn at(base: tokio::time::Instant, ms: u64) -> tokio::time::Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn latency_is_observed_once_per_utterance_and_goes_to_the_other_seat() {
        let l = SessionLatency::new();
        let t0 = l.epoch;
        // Nothing was said: nothing to measure.
        assert_eq!(l.audio_delivered(1, at(t0, 50)), None);

        // The owner side (seat 0) speaks; the guest (seat 1) hears the translation 240 ms later.
        l.speech_started(0, at(t0, 100));
        assert_eq!(
            l.audio_delivered(1, at(t0, 340)),
            Some(Duration::from_millis(240))
        );
        // The second chunk of the same utterance is not a second sample.
        assert_eq!(l.audio_delivered(1, at(t0, 360)), None);
    }

    #[test]
    fn a_seat_never_measures_its_own_speech() {
        let l = SessionLatency::new();
        let t0 = l.epoch;
        l.speech_started(0, at(t0, 10));
        // Audio delivered to the speaker itself (it would be an echo) is not a sample.
        assert_eq!(l.audio_delivered(0, at(t0, 200)), None);
        // And it did not consume the pending stamp of the real listener.
        assert!(l.audio_delivered(1, at(t0, 250)).is_some());
    }

    #[test]
    fn both_directions_are_measured_independently() {
        let l = SessionLatency::new();
        let t0 = l.epoch;
        l.speech_started(0, at(t0, 0));
        l.speech_started(1, at(t0, 50));
        assert_eq!(
            l.audio_delivered(0, at(t0, 400)),
            Some(Duration::from_millis(350))
        );
        assert_eq!(
            l.audio_delivered(1, at(t0, 420)),
            Some(Duration::from_millis(420))
        );
    }

    #[test]
    fn a_new_utterance_replaces_a_stale_stamp_and_an_old_one_is_dropped() {
        let l = SessionLatency::new();
        let t0 = l.epoch;
        l.speech_started(0, at(t0, 0));
        // Never translated; the speaker starts again later and that is what the audio answers.
        l.speech_started(0, at(t0, 5_000));
        assert_eq!(
            l.audio_delivered(1, at(t0, 5_300)),
            Some(Duration::from_millis(300))
        );
        // A stamp older than the plausibility window is discarded, not recorded.
        l.speech_started(0, at(t0, 6_000));
        assert_eq!(l.audio_delivered(1, at(t0, 6_000 + 31_000)), None);
    }

    #[test]
    fn probes_are_shared_per_session_and_forgotten_with_it() {
        let state = LiveState::new(LiveConfig::disabled());
        let id = Uuid::new_v4();
        let a = state.latency_for(id);
        let b = state.latency_for(id);
        assert!(
            Arc::ptr_eq(&a, &b),
            "both sockets of a session share one probe set"
        );
        assert!(!Arc::ptr_eq(&a, &state.latency_for(Uuid::new_v4())));
        // Once every socket let go, the next lookup starts clean and the map prunes itself.
        drop((a, b));
        for _ in 0..70 {
            drop(state.latency_for(Uuid::new_v4()));
        }
        let map = state.latency.lock().unwrap();
        assert!(map.len() < 70, "dead entries are pruned, got {}", map.len());
    }

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
