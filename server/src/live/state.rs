//! Shared Live Connect state, carried on `AppState`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use rust_decimal::Decimal;
use tokio::time::Instant;
use uuid::Uuid;

use super::config::LiveConfig;
use crate::business::credits::CreditAccumulator;
use crate::rooms::PeerTx;

/// Seats a conversation can have: the host plus up to three guests (`rooms::MAX_PEERS`).
pub const MAX_SEATS: usize = 4;

/// Speech-to-translated-audio latency probes and microphone levels for one conversation (spec
/// 0121, S18, and the echo filter).
///
/// One slot per **seat**, up to [`MAX_SEATS`]. A seat is a phone: [`seat_for`](Self::seat_for)
/// hands each peer id the first free slot when its socket connects, and the same slot again when
/// it reconnects. The speaker's socket *stamps* its slot when speech starts; whichever listener's
/// socket first delivers that speaker's translated audio *takes* the stamp, and the difference
/// is the number the product is judged on: how long after somebody starts talking somebody else
/// starts hearing them. One observation per utterance, however many listeners there are.
///
/// Lock-free on purpose. Stamping happens once per utterance and taking costs one atomic swap
/// per delivered audio chunk, so the hot path neither locks nor allocates. Stamps are nanoseconds
/// since `epoch` plus one, so `0` can mean "nothing pending".
#[derive(Debug)]
pub struct SessionLatency {
    epoch: tokio::time::Instant,
    /// Which peer id holds each seat. Set once, never changed: a seat is for the life of the
    /// probe set, which lives as long as any socket of the conversation does.
    seats: [OnceLock<Box<str>>; MAX_SEATS],
    stamps: [AtomicU64; MAX_SEATS],
    /// Per seat, smoothed microphone level of the audio it feeds the engine (mean absolute PCM16
    /// sample) and when it was last updated. Written only by that seat's socket.
    mic_level: [AtomicU32; MAX_SEATS],
    mic_level_at: [AtomicU64; MAX_SEATS],
    /// Per seat, when its microphone last carried speech (same nanosecond encoding as `stamps`).
    mic_voiced_at: [AtomicU64; MAX_SEATS],
    /// When the echo filter last wrote a diagnostics event for this conversation.
    echo_report_at: AtomicU64,
    /// The conversation runs on Enhanced (phones send text, no audio reaches the server). Set by
    /// the sockets when they connect and cleared on the whole-session fallback; the outbound pump
    /// reads it to skip the audio-level echo filter, which has no microphone levels to work with
    /// and could only delay captions, in favour of the text-level guard in `enhanced`.
    text_mode: std::sync::atomic::AtomicBool,
    /// What each seat recently sent as text (Enhanced), for the text-level echo guard.
    text_log: Mutex<super::enhanced::TextLog>,
    /// How many translation calls the conversation's Enhanced seats may still make this minute.
    translation_budget: Mutex<super::enhanced::TranslationBudget>,
}

/// A microphone level nobody refreshed for this long is silence, not "the last thing it heard".
const MIC_LEVEL_FRESH: Duration = Duration::from_secs(3);
/// The echo filter writes at most one diagnostics event per conversation in this interval.
const ECHO_REPORT_EVERY: Duration = Duration::from_secs(10);

/// An utterance older than this when its audio finally arrives was never going to be a latency
/// sample (the translation was skipped, or the speaker went on to something else): dropped, not
/// recorded, so a stale stamp cannot poison the histogram.
const MAX_PLAUSIBLE_LATENCY: Duration = Duration::from_secs(30);

impl SessionLatency {
    fn with_calls_per_min(calls_per_min: u32) -> Self {
        Self {
            epoch: tokio::time::Instant::now(),
            seats: std::array::from_fn(|_| OnceLock::new()),
            stamps: std::array::from_fn(|_| AtomicU64::new(0)),
            mic_level: std::array::from_fn(|_| AtomicU32::new(0)),
            mic_level_at: std::array::from_fn(|_| AtomicU64::new(0)),
            mic_voiced_at: std::array::from_fn(|_| AtomicU64::new(0)),
            echo_report_at: AtomicU64::new(0),
            text_mode: std::sync::atomic::AtomicBool::new(false),
            text_log: Mutex::new(super::enhanced::TextLog::default()),
            translation_budget: Mutex::new(super::enhanced::TranslationBudget::new(calls_per_min)),
        }
    }

    /// A probe set with no conversation behind it, for tests in other modules.
    #[cfg(test)]
    pub(crate) fn new_for_test() -> Self {
        Self::with_calls_per_min(600)
    }

    /// Mark the conversation as running (or no longer running) on Enhanced text.
    pub fn set_text_mode(&self, on: bool) {
        self.text_mode.store(on, Ordering::Relaxed);
    }

    /// Whether the conversation currently runs on Enhanced text.
    pub fn text_mode(&self) -> bool {
        self.text_mode.load(Ordering::Relaxed)
    }

    /// Judge one text frame from `seat` (declared language `lang`) against what the OTHER seats
    /// sent in the last few seconds and what was last spoken to its language, and, unless it is an
    /// echo, remember it. `true` means "echo: drop it".
    pub fn text_echo(
        &self,
        seat: usize,
        lang: &str,
        now: tokio::time::Instant,
        text: &str,
        sim: f64,
    ) -> bool {
        self.text_log
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .judge(seat % MAX_SEATS, lang, now, text, sim)
    }

    /// `text`, said by `from_seat`, was just sent to the phones of language `lang` to be spoken by
    /// their TTS.
    pub fn note_spoken(&self, from_seat: usize, lang: &str, now: tokio::time::Instant, text: &str) {
        self.text_log
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .note_spoken(from_seat % MAX_SEATS, lang, now, text);
    }

    /// May the conversation make one more translation call right now? Counts it when it does.
    /// Interim captions are only allowed a share of the minute, so a storm of them can never
    /// starve the finals.
    pub fn allow_translation(&self, now: tokio::time::Instant, is_final: bool) -> bool {
        self.translation_budget
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .allow(now, is_final)
    }

    /// The seat of `peer`: the slot it already holds, else the first free one. Called once per
    /// socket at connect, never per frame. A conversation never has more than [`MAX_SEATS`]
    /// peers, so running out is a bug elsewhere; the last slot is shared rather than panicking.
    pub fn seat_for(&self, peer: &str) -> usize {
        for (i, slot) in self.seats.iter().enumerate() {
            if slot.get().is_some_and(|p| &**p == peer) {
                return i;
            }
            // `set` can lose a race to another socket claiming the same free slot; then look at
            // what it holds (it may even be this very peer, reconnecting twice).
            if slot.set(peer.into()).is_ok() || slot.get().is_some_and(|p| &**p == peer) {
                return i;
            }
        }
        MAX_SEATS - 1
    }

    /// The seat `peer` holds, if its socket has connected. What the echo filter's level lookup
    /// uses to turn a speaker id on a frame into a microphone.
    pub fn seat_of(&self, peer: &str) -> Option<usize> {
        self.seats
            .iter()
            .position(|s| s.get().is_some_and(|p| &**p == peer))
    }

    fn encode(&self, at: tokio::time::Instant) -> u64 {
        (at.saturating_duration_since(self.epoch).as_nanos() as u64).saturating_add(1)
    }

    /// One frame the `seat`'s phone fed the engine: `mean_abs` is its mean absolute sample.
    ///
    /// Feeds the echo filter's one hard question: when both phones heard the same sentence, whose
    /// mouth was it? The phone whose microphone was clearly louder. A smoothed level (about 1.6 s
    /// time constant at 10 frames/s) rather than the last frame, so a pause between words does not
    /// flip the answer. Only the seat's own socket writes it, so a plain load/store is enough.
    pub fn mic_frame(&self, seat: usize, at: tokio::time::Instant, mean_abs: u32, voiced: bool) {
        let i = seat % MAX_SEATS;
        // A stale level reads as 0, so speech after a long silence starts from silence.
        let old = self.mic_level(i, at);
        // Fast attack, slow release: speech that has just started reads as most of its real level
        // within ~300 ms (a replier must not look quiet next to somebody who just stopped), while a
        // pause between words barely moves it (about 1.6 s time constant at 10 frames/s).
        let new = if mean_abs > old {
            old + (mean_abs - old) / 2
        } else {
            old - old / 16 + mean_abs / 16
        };
        self.mic_level[i].store(new, Ordering::Relaxed);
        let stamp = self.encode(at);
        self.mic_level_at[i].store(stamp, Ordering::Relaxed);
        if voiced {
            self.mic_voiced_at[i].store(stamp, Ordering::Relaxed);
        }
    }

    /// The smoothed microphone level of `seat` at `now`; `0` when it has been quiet for a while.
    pub fn mic_level(&self, seat: usize, now: tokio::time::Instant) -> u32 {
        let i = seat % MAX_SEATS;
        let stamp = self.mic_level_at[i].load(Ordering::Relaxed);
        if stamp == 0 {
            return 0;
        }
        let age = now
            .saturating_duration_since(self.epoch)
            .saturating_sub(Duration::from_nanos(stamp - 1));
        if age > MIC_LEVEL_FRESH {
            return 0;
        }
        self.mic_level[i].load(Ordering::Relaxed)
    }

    /// When `seat`'s microphone last carried speech, if ever.
    pub fn mic_last_voiced(&self, seat: usize) -> Option<tokio::time::Instant> {
        let stamp = self.mic_voiced_at[seat % MAX_SEATS].load(Ordering::Relaxed);
        (stamp != 0).then(|| self.epoch + Duration::from_nanos(stamp - 1))
    }

    /// Whether the echo filter may write a diagnostics event now. At most one per conversation per
    /// [`ECHO_REPORT_EVERY`], whichever seat asks first: two seats must not double the rate.
    pub fn allow_echo_report(&self, now: tokio::time::Instant) -> bool {
        let stamp = self.encode(now);
        let last = self.echo_report_at.load(Ordering::Relaxed);
        if last != 0 && Duration::from_nanos(stamp.saturating_sub(last)) < ECHO_REPORT_EVERY {
            return false;
        }
        self.echo_report_at
            .compare_exchange(last, stamp, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }

    /// `seat` started speaking at `at`. Overwrites whatever was pending: a new utterance
    /// supersedes one whose audio never arrived.
    pub fn speech_started(&self, seat: usize, at: tokio::time::Instant) {
        let ns = at.saturating_duration_since(self.epoch).as_nanos() as u64;
        self.stamps[seat % MAX_SEATS].store(ns.saturating_add(1), Ordering::Relaxed);
    }

    /// Translated audio of `speaker`'s speech was delivered to `listener` at `at`. Returns the
    /// latency of the utterance it answers, exactly once per utterance: a second chunk of the
    /// same utterance (or the same chunk reaching another listener) finds nothing pending. A seat
    /// never measures its own speech.
    pub fn audio_delivered(
        &self,
        listener: usize,
        speaker: usize,
        at: tokio::time::Instant,
    ) -> Option<Duration> {
        if listener % MAX_SEATS == speaker % MAX_SEATS {
            return None;
        }
        let stamp = self.stamps[speaker % MAX_SEATS].swap(0, Ordering::Relaxed);
        if stamp == 0 {
            return None;
        }
        let now = at.saturating_duration_since(self.epoch);
        let started = Duration::from_nanos(stamp - 1);
        let elapsed = now.checked_sub(started)?;
        (elapsed <= MAX_PLAUSIBLE_LATENCY).then_some(elapsed)
    }
}

/// What an org seat owes but has not been charged yet, for the lifetime of its session.
///
/// Org credits are whole numbers (one cent), so a tick is usually a fraction of one and the
/// fraction is carried to the next. That carry used to live in each Start's meter and died with
/// it: a phone that Starts, speaks four seconds and Stops, over and over, never reached a whole
/// credit in any one Start and so was never charged at all. Kept here, per `(session, peer)`,
/// ten four-second Starts cost what one forty-second run costs.
#[derive(Debug)]
struct Carry {
    owed: CreditAccumulator,
    /// The wall-clock bucket whose cost was last added, so two meters of one seat covering the
    /// same interval (a reconnect overlapping its predecessor) add it once, not twice.
    bucket: Option<i64>,
    /// When this seat last owed anything, so a carry nobody is adding to can be dropped.
    touched: Instant,
}

/// A desk session an address opened, and until when it is worth counting.
type DeskScan = (Uuid, Instant);

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
    /// Uncharged org cost per seat; see [`Carry`].
    owed: Mutex<HashMap<(Uuid, Uuid), Carry>>,
    /// When each seat last took an opening charge, as `(session, peer)`; see
    /// [`LiveState::claim_opening`].
    opened: Mutex<HashMap<(Uuid, Uuid), Instant>>,
    /// Desk-scan sessions per `(desk, client address)`, with when each stops being worth
    /// counting. See [`LiveState::desk_scans_from`].
    desk_scans: Mutex<HashMap<(Uuid, String), Vec<DeskScan>>>,
    /// When the hourly housekeeping (finalise stragglers, retention) last started.
    maintenance: Mutex<Option<Instant>>,
    /// Latency probes per session. Weak: a probe lives exactly as long as a socket of its
    /// session holds it, so nothing here needs the sweeper to clean up after a conversation.
    latency: Mutex<HashMap<Uuid, Weak<SessionLatency>>>,
    /// Conversations whose tier ran out of capacity and moved, as a whole, to the default
    /// engine (see [`LiveState::downgrade_session`]), with when it happened.
    downgraded: Mutex<HashMap<Uuid, Instant>>,
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
            owed: Mutex::new(HashMap::new()),
            opened: Mutex::new(HashMap::new()),
            desk_scans: Mutex::new(HashMap::new()),
            maintenance: Mutex::new(None),
            latency: Mutex::new(HashMap::new()),
            downgraded: Mutex::new(HashMap::new()),
        }
    }

    /// Record that `session` fell back to the default engine because its tier was at capacity.
    /// Returns `true` for the call that made the change, so exactly one seat announces it even
    /// when two of them hit the limit in the same instant.
    ///
    /// Shared by every seat of the conversation (they all hold this `Arc`), which is the point:
    /// a seat that starts speaking later must not retry the full tier and fail the same way.
    /// In-memory is enough because a conversation's room lives in this process; the stored
    /// `engine_id` is updated alongside it for anyone who reconnects after a restart. Entries
    /// older than a conversation can last are dropped when the map has grown, so it needs no
    /// help from the sweeper.
    pub fn downgrade_session(&self, session: Uuid) -> bool {
        let now = Instant::now();
        let mut map = self.downgraded.lock().unwrap_or_else(|p| p.into_inner());
        if map.len() >= 256 {
            let ttl = Duration::from_secs(
                u64::try_from(
                    self.config
                        .max_session_min
                        .saturating_add(10)
                        .saturating_mul(60),
                )
                .unwrap_or(u64::MAX),
            );
            map.retain(|_, at| now.saturating_duration_since(*at) < ttl);
        }
        map.insert(session, now).is_none()
    }

    /// Whether `session` has fallen back to the default engine.
    pub fn is_downgraded(&self, session: Uuid) -> bool {
        self.downgraded
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(&session)
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
        let fresh = Arc::new(SessionLatency::with_calls_per_min(
            self.config.enhanced_calls_per_min,
        ));
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

    /// May this seat take an opening charge now? At most ONE per seat per `interval`.
    ///
    /// Every Start used to pay an opening charge under a key of its own, so a phone that Starts and
    /// Stops in a loop paid for each pair (about 40 a second) while translating nothing: a guest
    /// could drain the host's balance. Now a Start within `interval` of that seat's previous
    /// opening charge opens its meter WITHOUT one; the ordinary per-interval ticks still bill the
    /// audio or text it really sends. A Start/Stop shorter than an interval is still not free: the
    /// first one pays. The map holds only seats that opened within the last interval (older
    /// entries are dropped as the map grows, and with their session), so it cannot grow without
    /// bound.
    pub fn claim_opening(&self, session: Uuid, peer: Uuid, interval: Duration) -> bool {
        self.claim_opening_at(Instant::now(), session, peer, interval)
    }

    fn claim_opening_at(
        &self,
        now: Instant,
        session: Uuid,
        peer: Uuid,
        interval: Duration,
    ) -> bool {
        let mut map = self.opened.lock().unwrap_or_else(|p| p.into_inner());
        if map.len() >= 64 {
            map.retain(|_, at| now.saturating_duration_since(*at) < interval);
        }
        match map.get(&(session, peer)) {
            Some(at) if now.saturating_duration_since(*at) < interval => false,
            _ => {
                map.insert((session, peer), now);
                true
            }
        }
    }

    /// Forget the opening-charge stamps of sessions that are no longer active.
    pub fn retain_openings_for(&self, active: &[Uuid]) {
        self.opened
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|(session, _), _| active.contains(session));
    }

    /// Add `amount` USD of cost to this seat's carry and return the whole credits now due (0
    /// while it is still under one). `bucket` is the wall-clock interval the cost belongs to;
    /// pass `None` for a charge that has its own key (a Start's opening charge).
    pub fn take_owed(
        &self,
        session: Uuid,
        peer: Uuid,
        amount: Decimal,
        bucket: Option<i64>,
    ) -> i32 {
        self.take_owed_at(Instant::now(), session, peer, amount, bucket)
    }

    fn take_owed_at(
        &self,
        now: Instant,
        session: Uuid,
        peer: Uuid,
        amount: Decimal,
        bucket: Option<i64>,
    ) -> i32 {
        let mut map = self.owed.lock().unwrap_or_else(|p| p.into_inner());
        // A carry that nothing has touched for longer than a conversation can last belongs to a
        // conversation that is over. Pruned here, when the map has grown past what a busy
        // process has open, so it needs no help from the sweeper and cannot grow without bound.
        if map.len() >= 256 {
            let ttl = Duration::from_secs(
                u64::try_from(
                    self.config
                        .max_session_min
                        .saturating_add(10)
                        .saturating_mul(60),
                )
                .unwrap_or(u64::MAX),
            );
            map.retain(|_, c| now.saturating_duration_since(c.touched) < ttl);
        }
        let carry = map.entry((session, peer)).or_insert_with(|| Carry {
            owed: CreditAccumulator::default(),
            bucket: None,
            touched: now,
        });
        carry.touched = now;
        if let Some(b) = bucket {
            if carry.bucket == Some(b) {
                return 0;
            }
            carry.bucket = Some(b);
        }
        carry.owed.take(amount)
    }

    /// Return credits [`take_owed`](Self::take_owed) handed out but that were not charged.
    pub fn put_back_owed(&self, session: Uuid, peer: Uuid, credits: i32) {
        let mut map = self.owed.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(carry) = map.get_mut(&(session, peer)) {
            carry.owed.put_back(credits);
        }
    }

    /// The desk sessions `ip` opened at `desk` that are still inside their wait window. The
    /// caller asks the database which of them are still waiting: this only remembers who made
    /// them, so the answer stays right when staff answer or a customer gives up.
    pub fn desk_scans_from(&self, desk: Uuid, ip: &str) -> Vec<Uuid> {
        let now = Instant::now();
        let mut map = self.desk_scans.lock().unwrap_or_else(|p| p.into_inner());
        let key = (desk, ip.to_string());
        let Some(list) = map.get_mut(&key) else {
            return Vec::new();
        };
        list.retain(|(_, until)| *until > now);
        let ids = list.iter().map(|(id, _)| *id).collect();
        if list.is_empty() {
            map.remove(&key);
        }
        ids
    }

    /// Remember that `ip` opened `session` at `desk`, until `ttl` from now (the wait window).
    pub fn note_desk_scan(&self, desk: Uuid, ip: &str, session: Uuid, ttl: Duration) {
        let now = Instant::now();
        let mut map = self.desk_scans.lock().unwrap_or_else(|p| p.into_inner());
        if map.len() >= 256 {
            map.retain(|_, list| {
                list.retain(|(_, until)| *until > now);
                !list.is_empty()
            });
        }
        map.entry((desk, ip.to_string()))
            .or_default()
            .push((session, now + ttl));
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
    fn a_level_rises_fast_when_speech_starts_and_falls_slowly() {
        let l = SessionLatency::new_for_test();
        let t0 = l.epoch;
        // Three 100 ms frames of speech after silence already read as most of the real level:
        // a replier who has just started must not look quiet next to a speaker who stopped.
        for i in 0..3u64 {
            l.mic_frame(0, at(t0, i * 100), 1000, true);
        }
        assert!(
            l.mic_level(0, at(t0, 300)) >= 800,
            "{}",
            l.mic_level(0, at(t0, 300))
        );
        // ...while a pause after speech decays slowly (one quiet frame barely moves it).
        let before = l.mic_level(0, at(t0, 300));
        l.mic_frame(0, at(t0, 300), 0, false);
        assert!(l.mic_level(0, at(t0, 300)) > before * 9 / 10);
    }

    #[test]
    fn a_microphone_level_smooths_decays_and_belongs_to_one_seat() {
        let l = SessionLatency::new_for_test();
        let t0 = l.epoch;
        assert_eq!((l.mic_level(0, t0), l.mic_last_voiced(0)), (0, None));
        for i in 0..60u64 {
            l.mic_frame(0, at(t0, i * 100), 1000, true);
        }
        let loud = l.mic_level(0, at(t0, 6_000));
        assert!(
            (900..=1000).contains(&loud),
            "settles near the input: {loud}"
        );
        assert_eq!(
            l.mic_level(1, at(t0, 6_000)),
            0,
            "the other seat is untouched"
        );
        assert_eq!(l.mic_last_voiced(0), Some(at(t0, 5_900)));
        // One quiet frame does not flip it...
        l.mic_frame(0, at(t0, 6_000), 0, false);
        assert!(l.mic_level(0, at(t0, 6_000)) > loud * 9 / 10);
        assert_eq!(
            l.mic_last_voiced(0),
            Some(at(t0, 5_900)),
            "quiet is not speech"
        );
        // ...but silence for long enough is silence, and speech restarts from it.
        assert_eq!(l.mic_level(0, at(t0, 9_100)), 0);
        l.mic_frame(0, at(t0, 9_200), 1000, true);
        // (fast attack: half of the input after one frame, not the old slow ramp)
        assert_eq!(l.mic_level(0, at(t0, 9_200)), 500);
    }

    #[test]
    fn echo_reports_are_limited_per_conversation_not_per_seat() {
        let l = SessionLatency::new_for_test();
        let t0 = l.epoch;
        assert!(l.allow_echo_report(at(t0, 100)));
        assert!(
            !l.allow_echo_report(at(t0, 5_000)),
            "the other seat shares the limit"
        );
        assert!(!l.allow_echo_report(at(t0, 10_000)));
        assert!(l.allow_echo_report(at(t0, 10_200)));
    }

    #[test]
    fn latency_is_observed_once_per_utterance_and_goes_to_the_other_seat() {
        let l = SessionLatency::new_for_test();
        let t0 = l.epoch;
        // Nothing was said: nothing to measure.
        assert_eq!(l.audio_delivered(1, 0, at(t0, 50)), None);

        // The owner side (seat 0) speaks; the guest (seat 1) hears the translation 240 ms later.
        l.speech_started(0, at(t0, 100));
        assert_eq!(
            l.audio_delivered(1, 0, at(t0, 340)),
            Some(Duration::from_millis(240))
        );
        // The second chunk of the same utterance is not a second sample.
        assert_eq!(l.audio_delivered(1, 0, at(t0, 360)), None);
    }

    #[test]
    fn a_seat_never_measures_its_own_speech() {
        let l = SessionLatency::new_for_test();
        let t0 = l.epoch;
        l.speech_started(0, at(t0, 10));
        // Audio delivered to the speaker itself (it would be an echo) is not a sample.
        assert_eq!(l.audio_delivered(0, 0, at(t0, 200)), None);
        // And it did not consume the pending stamp of the real listener.
        assert!(l.audio_delivered(1, 0, at(t0, 250)).is_some());
    }

    #[test]
    fn both_directions_are_measured_independently() {
        let l = SessionLatency::new_for_test();
        let t0 = l.epoch;
        l.speech_started(0, at(t0, 0));
        l.speech_started(1, at(t0, 50));
        assert_eq!(
            l.audio_delivered(0, 1, at(t0, 400)),
            Some(Duration::from_millis(350))
        );
        assert_eq!(
            l.audio_delivered(1, 0, at(t0, 420)),
            Some(Duration::from_millis(420))
        );
    }

    #[test]
    fn a_new_utterance_replaces_a_stale_stamp_and_an_old_one_is_dropped() {
        let l = SessionLatency::new_for_test();
        let t0 = l.epoch;
        l.speech_started(0, at(t0, 0));
        // Never translated; the speaker starts again later and that is what the audio answers.
        l.speech_started(0, at(t0, 5_000));
        assert_eq!(
            l.audio_delivered(1, 0, at(t0, 5_300)),
            Some(Duration::from_millis(300))
        );
        // A stamp older than the plausibility window is discarded, not recorded.
        l.speech_started(0, at(t0, 6_000));
        assert_eq!(l.audio_delivered(1, 0, at(t0, 6_000 + 31_000)), None);
    }

    #[test]
    fn each_peer_gets_a_seat_of_its_own_and_keeps_it_across_a_reconnect() {
        let l = SessionLatency::new_for_test();
        let ids = ["host", "g1", "g2", "g3"];
        let seats: Vec<usize> = ids.iter().map(|p| l.seat_for(p)).collect();
        assert_eq!(seats, vec![0, 1, 2, 3], "four peers, four distinct seats");
        // The same peer asking again (a reconnect) gets the seat it had, not a new one.
        assert_eq!(l.seat_for("g2"), 2);
        assert_eq!(l.seat_of("g3"), Some(3));
        assert_eq!(l.seat_of("stranger"), None);
    }

    #[test]
    fn a_microphone_level_belongs_to_one_of_four_seats() {
        let l = SessionLatency::new_for_test();
        let t0 = l.epoch;
        for seat in 0..MAX_SEATS {
            l.mic_frame(seat, at(t0, 0), 100 * (seat as u32 + 1), true);
        }
        // Each seat reads back its own level, never a neighbour's (the old code folded seats
        // onto two slots, so seat 2 read seat 0).
        let levels: Vec<u32> = (0..MAX_SEATS)
            .map(|seat| l.mic_level(seat, at(t0, 0)))
            .collect();
        assert_eq!(levels, vec![50, 100, 150, 200]);
        assert_eq!(l.mic_last_voiced(2), Some(at(t0, 0)));
    }

    #[test]
    fn with_three_listeners_one_utterance_is_observed_once_and_for_its_own_speaker() {
        let l = SessionLatency::new_for_test();
        let t0 = l.epoch;
        l.speech_started(1, at(t0, 100));
        // Seat 2's utterance is a different stream: its audio must not consume seat 1's stamp.
        assert_eq!(l.audio_delivered(0, 2, at(t0, 200)), None);
        // Seat 1's audio reaches three phones; only the first delivery is a sample.
        assert_eq!(
            l.audio_delivered(0, 1, at(t0, 340)),
            Some(Duration::from_millis(240))
        );
        assert_eq!(l.audio_delivered(2, 1, at(t0, 345)), None);
        assert_eq!(l.audio_delivered(3, 1, at(t0, 350)), None);
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
    fn a_seat_takes_one_opening_charge_per_interval() {
        let state = LiveState::new(LiveConfig::disabled());
        let t0 = Instant::now();
        let (s, p) = (Uuid::new_v4(), Uuid::new_v4());
        let iv = Duration::from_secs(5);
        assert!(state.claim_opening_at(t0, s, p, iv));
        // A Start/Stop loop: every further Start inside the interval opens for free.
        for ms in [1u64, 6, 50, 1000, 4999] {
            assert!(
                !state.claim_opening_at(t0 + Duration::from_millis(ms), s, p, iv),
                "{ms}"
            );
        }
        // The next honest Start, a whole interval later, pays again, and the interval restarts.
        assert!(state.claim_opening_at(t0 + iv, s, p, iv));
        assert!(!state.claim_opening_at(t0 + iv + Duration::from_secs(1), s, p, iv));
        // Another seat, and another conversation, have their own.
        assert!(state.claim_opening_at(t0 + iv, s, Uuid::new_v4(), iv));
        assert!(state.claim_opening_at(t0 + iv, Uuid::new_v4(), p, iv));
    }

    #[test]
    fn opening_stamps_are_pruned_and_forgotten_with_their_session() {
        let state = LiveState::new(LiveConfig::disabled());
        let t0 = Instant::now();
        let iv = Duration::from_secs(5);
        let sessions: Vec<Uuid> = (0..200).map(|_| Uuid::new_v4()).collect();
        for s in &sessions {
            state.claim_opening_at(t0, *s, Uuid::new_v4(), iv);
        }
        // Once the map is big, a later claim drops every stamp older than the interval.
        state.claim_opening_at(
            t0 + Duration::from_secs(60),
            Uuid::new_v4(),
            Uuid::new_v4(),
            iv,
        );
        assert_eq!(state.opened.lock().unwrap().len(), 1);
        // Ended sessions are forgotten by the sweeper's pass.
        let keep = Uuid::new_v4();
        state.claim_opening_at(t0, keep, Uuid::new_v4(), iv);
        state.retain_openings_for(&[keep]);
        assert_eq!(state.opened.lock().unwrap().len(), 1);
    }

    fn cents(n: i64, scale: u32) -> Decimal {
        Decimal::new(n, scale)
    }

    #[test]
    fn the_fraction_of_a_credit_survives_between_meters() {
        let state = LiveState::new(LiveConfig::disabled());
        let (s, p) = (Uuid::new_v4(), Uuid::new_v4());
        // Ten Starts of 0.4 of a credit each (a credit is one cent): every one alone is free, all
        // together are four.
        let due: i32 = (0..10)
            .map(|_| state.take_owed(s, p, cents(4, 3), None))
            .sum();
        assert_eq!(due, 4);
        // Another seat's carry is its own.
        let other = state.take_owed(s, Uuid::new_v4(), cents(4, 3), None);
        assert_eq!(other, 0);
    }

    #[test]
    fn two_meters_covering_one_interval_add_its_cost_once() {
        let state = LiveState::new(LiveConfig::disabled());
        let (s, p) = (Uuid::new_v4(), Uuid::new_v4());
        // 0.6 of a credit per interval. The second meter of the same interval adds nothing...
        assert_eq!(state.take_owed(s, p, cents(6, 3), Some(100)), 0);
        assert_eq!(state.take_owed(s, p, cents(6, 3), Some(100)), 0);
        // ...so the next interval is the one that crosses a whole credit (1.2), not this one (1.8).
        assert_eq!(state.take_owed(s, p, cents(6, 3), Some(101)), 1);
        // Charges that carry their own key are never merged.
        assert_eq!(state.take_owed(s, p, cents(6, 3), None), 0);
        assert_eq!(state.take_owed(s, p, cents(6, 3), None), 1);
    }

    #[test]
    fn credits_that_were_not_charged_go_back_to_the_carry() {
        let state = LiveState::new(LiveConfig::disabled());
        let (s, p) = (Uuid::new_v4(), Uuid::new_v4());
        assert_eq!(state.take_owed(s, p, cents(25, 3), None), 2);
        state.put_back_owed(s, p, 2);
        assert_eq!(state.take_owed(s, p, cents(0, 0), None), 2);
    }

    #[test]
    fn carries_nobody_adds_to_any_more_are_pruned_so_the_map_cannot_grow_forever() {
        let state = LiveState::new(LiveConfig::disabled());
        let t0 = Instant::now();
        let p = Uuid::new_v4();
        let old: Vec<Uuid> = (0..256).map(|_| Uuid::new_v4()).collect();
        for s in &old {
            state.take_owed_at(t0, *s, p, cents(5, 3), None);
        }
        // A seat of a conversation still running keeps its fraction...
        let live = old[0];
        let later = t0 + Duration::from_secs(60 * 60);
        state.take_owed_at(later, live, p, cents(1, 3), None);
        // ...and once the map is big, a much later arrival drops every carry that has been quiet
        // for longer than a conversation can last (the ceiling, 120 min by default, plus ten),
        // but not one that was added to an hour earlier.
        let much_later = t0 + Duration::from_secs(60 * 60 * 3);
        let fresh = Uuid::new_v4();
        state.take_owed_at(much_later, fresh, p, cents(5, 3), None);
        let map = state.owed.lock().unwrap();
        assert_eq!(
            map.len(),
            2,
            "the arrival and the seat that was still active"
        );
        assert!(map.contains_key(&(fresh, p)) && map.contains_key(&(live, p)));
    }

    #[test]
    fn desk_scans_are_remembered_per_desk_and_address_until_their_window_closes() {
        let state = LiveState::new(LiveConfig::disabled());
        let (desk, other_desk, a, b) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        assert!(state.desk_scans_from(desk, "1.1.1.1").is_empty());
        state.note_desk_scan(desk, "1.1.1.1", a, Duration::from_secs(120));
        state.note_desk_scan(desk, "1.1.1.1", b, Duration::from_millis(0));
        // The one whose window is already over is not counted; the address and the desk matter.
        assert_eq!(state.desk_scans_from(desk, "1.1.1.1"), vec![a]);
        assert!(state.desk_scans_from(desk, "2.2.2.2").is_empty());
        assert!(state.desk_scans_from(other_desk, "1.1.1.1").is_empty());
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
