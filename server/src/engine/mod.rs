//! Translation-engine registry (spec 0093).
//!
//! VoxTranslate supports N translation engines behind one trait. Each engine is a
//! self-contained module that declares its [`EngineMetadata`] and opens a
//! per-speaker session from captured audio. Routing, billing, and the UI are
//! engine-agnostic: adding an engine = new module + `register`, nothing else
//! (Open/Closed — no `if engine == X` in the call sites).

pub mod cartesia;
pub mod gemini;
pub mod help_assistant;
pub mod help_assistant_client;
pub mod langmap;
pub mod metadata;
pub mod openai;
pub mod premium;
pub mod pro;
pub mod qwen;
pub mod qwen_catalogue;
pub mod standard;
pub mod voice_assistant;
pub mod voice_assistant_client;

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::deepgram::SpeakerCtx;
use crate::moderation::Moderator;
use crate::rooms::RoomManager;
use crate::transcripts::TranscriptService;

pub use cartesia::CartesiaEngine;
pub use metadata::{EngineCapabilities, EngineInfo, EngineMetadata};
pub use premium::PremiumEngine;
pub use pro::ProEngine;
pub use standard::StandardEngine;

/// Stable engine ids. Persisted in `usage_sessions.engine_id` and sent in the join
/// payload, so they must never change once shipped — which is why the string VALUES no
/// longer match the tier labels (they predate the Pro/Premium swap, #259). Named by
/// PROVIDER here to stay unambiguous: `OPENAI_ID == "premium"` is the **Pro** tier;
/// `GEMINI_ID` is the **Premium** tier.
pub const STANDARD_ID: &str = "standard";
/// OpenAI GPT-Realtime-Translate — the "Pro" tier (`engine::pro`). Its persisted id is
/// the literal `"premium"` (historical; kept frozen so billing/analytics stay valid).
pub const OPENAI_ID: &str = "premium";
/// Gemini 3.5 Live Translate — the "Premium" tier (`engine::premium`, spec 0100).
pub const GEMINI_ID: &str = "gemini_live_translate";
/// Cartesia real-time STT (Ink-2) + TTS (Sonic-3.5) — the "Enhanced" tier
/// (`engine::cartesia`, spec 0108, replacing the previous Enhanced engine in spec 0101).
/// The only **client-direct** engine: the browser connects straight to Cartesia with a
/// server-minted access token, so this id only tags `usage_sessions` / the listener-meter
/// rate and never opens a server session. (Pre-0108 Enhanced-tier `usage_sessions` rows
/// keep their old engine_id historically; `resolve()` falls unknown ids back to the default.)
pub const CARTESIA_ID: &str = "cartesia";

/// Live per-speaker dependencies the handler hands an engine when speech starts.
///
/// These come from the (optionally DB-backed) `AppState` and may differ from what
/// existed when the registry was built — e.g. the moderator gains DB blocklist
/// terms in `AppState::init` — so they are passed **per session** rather than
/// captured inside the engine.
pub struct SessionDeps {
    pub rooms: Arc<RoomManager>,
    pub moderator: Arc<Moderator>,
    /// Transcript persistence — `None` without a database.
    pub transcripts: Option<TranscriptService>,
    /// This speaker's transcript participant row, for auto-detect lang updates.
    pub participant_row: Option<Uuid>,
    /// Listener-pays routing (spec 0099). When `true`, an engine translates only
    /// into the languages of the listeners who chose IT (`target_langs_for_engine`)
    /// and delivers only to those listeners — instead of all room languages /
    /// everyone. `false` = the legacy speaker-pays behaviour (default; flag off).
    pub listener_pays: bool,
    /// Reliable text translator (Groq), the same one the Standard tier uses. The Pro
    /// engine falls back to it for the SUBTITLE text when OpenAI's gpt-realtime-translate
    /// ships an EMPTY output transcript for a segment (it does this intermittently — the
    /// translated audio still plays, but the caption would otherwise carry no translated
    /// text, so the client shows the untranslated original). Cheap to clone (Arc-backed).
    pub translator: crate::translator::Translator,
    /// Who persists this speaking turn's segments. Built ONCE per `Start` and cloned into
    /// every engine so the whole fan-out — all languages, all engines — writes each
    /// utterance exactly once. See [`TranscriptWriter`].
    pub transcript_writer: TranscriptWriter,
}

/// A role that exactly one of a speaking turn's sessions may hold at a time, and that
/// is **re-elected live** when its holder lets go.
///
/// A speaker's audio fans out to one upstream session per target language (and, under
/// listener-pays, one such set per engine). Some jobs must happen once per utterance no
/// matter how many sessions are running. Pinning such a job to a flag decided at spawn
/// looks equivalent and is not: the holder's language can leave the room mid-call, and a
/// frozen flag leaves the job stranded on a session that no longer exists while every
/// survivor sits there believing someone else has it.
///
/// So the claim is asked, not assigned: a session that finds it vacant takes it. `K` is
/// whatever identifies a session within the claim's scope — a language for a role scoped
/// to one engine, a process-unique id for one shared across engines.
pub struct SessionClaim<K>(Arc<Mutex<Option<K>>>);

// Hand-written: `derive` would demand `K: Clone`/`K: Default` from the *key*, which has
// nothing to do with cloning a handle or starting out vacant.
impl<K> Clone for SessionClaim<K> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<K> Default for SessionClaim<K> {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }
}

impl<K: PartialEq> SessionClaim<K> {
    /// Whether `key` holds the role — taking it when it is vacant, so the role heals
    /// itself the moment the previous holder lets go instead of waiting for a reconcile
    /// tick that may never come.
    pub fn owns(&self, key: &K) -> bool
    where
        K: Clone,
    {
        let mut held = self.0.lock().expect("session claim poisoned");
        match held.as_ref() {
            Some(h) => h == key,
            None => {
                *held = Some(key.clone());
                true
            }
        }
    }

    /// Give the role back if `key` held it. A no-op otherwise, so a session can call this
    /// unconditionally on the way out without knowing whether it was the holder.
    pub fn release(&self, key: &K) {
        let mut held = self.0.lock().expect("session claim poisoned");
        if held.as_ref() == Some(key) {
            *held = None;
        }
    }
}

impl SessionClaim<u64> {
    /// A process-unique session id, for claims whose scope spans more than one engine and
    /// so cannot key on something as local as a target language.
    pub fn next_id() -> u64 {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }
}

/// Who persists a speaking turn's segments. Scoped to the whole turn — **across engines**,
/// because listener-pays runs Standard alongside a premium engine on the same captured
/// stream and a per-engine claim would elect one writer each, storing every line twice.
/// Keyed by session id for exactly that reason: two engines can serve the same language.
pub type TranscriptWriter = SessionClaim<u64>;

/// Who echoes the speaker's own words back to them and captions the listeners who share
/// the speaker's language. Scoped to ONE engine and keyed by target language: delivery to
/// same-language listeners is engine-scoped under listener-pays, so each engine owes its
/// own listeners this caption and must elect its own holder.
pub type PrimaryClaim = SessionClaim<String>;

/// Outcome of opening a speaking session.
pub enum SessionOutcome {
    /// Session opened — feed captured audio into this sender (Standard expects
    /// WebM/Opus, premium engines PCM16). The engine owns the receiver and all
    /// processing; dropping the sender flushes and closes it.
    Started(mpsc::Sender<Vec<u8>>),
    /// The engine is at capacity right now (spec 0094) — the caller must fall back
    /// to the default engine so translation never stops. Only premium engines,
    /// which hold a bounded pool of upstream sessions, return this.
    AtCapacity,
    /// The session could not be opened (e.g. the upstream service is unavailable).
    Failed,
}

/// Pure reconcile decision shared by the live speech-to-speech engines: given the
/// languages a speaker currently has a session for (`active`) and the languages the
/// room now wants (`want`, already speaker- and `auto`-filtered), return which to
/// `drop` (no longer present) and which to `add` (newly present, in `want` order).
///
/// Targets are otherwise fixed at the first `Start`, so without this a peer who joins
/// — or whose `auto` language resolves — *after* a speaker began talking would never be
/// translated for. Kept pure (no permits, no spawning) so it's unit-tested directly;
/// the caller owns the permit budget, the spawn, and primary re-election.
pub(crate) fn reconcile_langs(
    active: &HashSet<String>,
    want: &[String],
) -> (Vec<String>, Vec<String>) {
    let want_set: HashSet<&str> = want.iter().map(String::as_str).collect();
    let drop: Vec<String> = active
        .iter()
        .filter(|l| !want_set.contains(l.as_str()))
        .cloned()
        .collect();
    let add: Vec<String> = want
        .iter()
        .filter(|l| !active.contains(*l))
        .cloned()
        .collect();
    (drop, add)
}

/// The engines to open **alongside** Standard for one speaking turn: the distinct
/// speech-to-speech engines a cross-language listener actually chose, minus Standard.
///
/// Standard is excluded even when listeners chose it, because the caller starts it
/// unconditionally afterwards — it is the tier that must never be missing. Letting it in
/// here started it TWICE for the same speaker, which cost real money: the Standard tier
/// bills per target language, so a room with one English listener opened two upstream
/// sessions and paid for both. It also fed the same listeners two audio streams, and
/// wrote every utterance to the transcript twice.
///
/// The list was always described in the caller as the "premium engines a listener chose";
/// the filter said "anything that speaks", and Standard speaks (`translated_audio`).
/// Naming the exclusion here keeps the two from drifting apart again.
pub fn extra_engines_for_turn(chosen: &[String], speech_engines: &[String]) -> Vec<String> {
    let mut wanted: Vec<String> = chosen
        .iter()
        .filter(|id| id.as_str() != STANDARD_ID && speech_engines.contains(id))
        .cloned()
        .collect();
    wanted.sort();
    wanted.dedup();
    wanted
}

/// A translation engine: turns one speaker's captured audio into room subtitles
/// (and, for premium engines, translated audio).
#[async_trait]
pub trait TranslationEngine: Send + Sync {
    /// Static description (id, languages, cost, capabilities).
    fn metadata(&self) -> &EngineMetadata;

    /// Open a speaking session for `ctx`. See [`SessionOutcome`] — `AtCapacity`
    /// tells the caller to retry on the default engine (never block the speaker).
    async fn start_session(&self, ctx: SpeakerCtx, deps: SessionDeps) -> SessionOutcome;
}

/// Ordered set of available engines, keyed by id, with a guaranteed default.
pub struct EngineRegistry {
    engines: Vec<Arc<dyn TranslationEngine>>,
    default_id: String,
}

impl EngineRegistry {
    /// Create an empty registry whose [`resolve`](Self::resolve) /
    /// [`default`](Self::default) fall back to `default_id` (which must be
    /// registered before first use).
    pub fn new(default_id: impl Into<String>) -> Self {
        Self {
            engines: Vec::new(),
            default_id: default_id.into(),
        }
    }

    /// Add an engine. Later registrations with a duplicate id never shadow an
    /// earlier one ([`get`](Self::get) returns the first match), so register the
    /// canonical engine first.
    pub fn register(&mut self, engine: Arc<dyn TranslationEngine>) {
        self.engines.push(engine);
    }

    /// Look up an engine by id.
    pub fn get(&self, id: &str) -> Option<Arc<dyn TranslationEngine>> {
        self.engines.iter().find(|e| e.metadata().id == id).cloned()
    }

    /// The default engine. Panics only on a programming error — the configured
    /// default must have been registered at startup.
    pub fn default(&self) -> Arc<dyn TranslationEngine> {
        self.get(&self.default_id)
            .expect("default engine must be registered")
    }

    /// Resolve a (possibly absent or stale) engine id to a concrete engine,
    /// falling back to the default when the id is unknown or has been removed —
    /// graceful degradation for a persisted preference that no longer exists.
    pub fn resolve(&self, id: Option<&str>) -> Arc<dyn TranslationEngine> {
        id.and_then(|i| self.get(i))
            .unwrap_or_else(|| self.default())
    }

    /// All registered engines.
    pub fn list(&self) -> impl Iterator<Item = &Arc<dyn TranslationEngine>> {
        self.engines.iter()
    }

    /// Public, client-safe DTOs for every engine (no raw cost/markup), for
    /// `GET /api/engines`.
    pub fn infos(&self) -> Vec<EngineInfo> {
        self.engines
            .iter()
            .map(|e| EngineInfo::from(e.metadata()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- TranscriptWriter -----------------------------------------------------
    //
    // The regression these guard: a speaker's audio fans out to one session per target
    // language (and per engine under listener-pays), each of which finalizes the SAME
    // utterance. Every one of them used to persist it, so the dashboard showed each line
    // repeated once per session.

    #[test]
    fn only_one_session_of_a_turn_may_write() {
        let w = TranscriptWriter::default();
        let (a, b, c) = (
            TranscriptWriter::next_id(),
            TranscriptWriter::next_id(),
            TranscriptWriter::next_id(),
        );
        assert!(
            w.owns(&a),
            "the first session to ask takes the vacant claim"
        );
        assert!(!w.owns(&b));
        assert!(!w.owns(&c));
        // Asking twice must not flip the answer — `owns` is a question, not a toggle.
        assert!(w.owns(&a));
        assert!(!w.owns(&b));
    }

    #[test]
    fn the_claim_is_shared_across_engines_not_just_languages() {
        // Listener-pays runs Standard AND a premium engine on the same captured stream.
        // A per-engine claim would elect one writer each and still write two rows.
        let deps_wide = TranscriptWriter::default();
        let standard_en = TranscriptWriter::next_id();
        let premium_es = TranscriptWriter::next_id();
        assert!(deps_wide.owns(&standard_en));
        assert!(!deps_wide.owns(&premium_es));
    }

    #[test]
    fn a_departing_writer_hands_the_claim_on() {
        // `is_primary` is frozen at spawn, so when its language left the room nobody was
        // primary any more and the transcript would simply stop. Releasing re-opens the
        // election instead.
        let w = TranscriptWriter::default();
        let first = TranscriptWriter::next_id();
        let second = TranscriptWriter::next_id();
        assert!(w.owns(&first));
        assert!(!w.owns(&second));
        w.release(&first);
        assert!(w.owns(&second), "the next session re-elects itself");
        assert!(
            !w.owns(&first),
            "the departed session does not take it back"
        );
    }

    #[test]
    fn releasing_a_claim_you_never_held_changes_nothing() {
        // Every session calls `release` on the way out, holder or not.
        let w = TranscriptWriter::default();
        let holder = TranscriptWriter::next_id();
        let other = TranscriptWriter::next_id();
        assert!(w.owns(&holder));
        w.release(&other);
        assert!(w.owns(&holder), "the real holder keeps writing");
    }

    // ---- PrimaryClaim ---------------------------------------------------------
    //
    // The regression these guard: `is_primary` used to be decided at spawn and frozen
    // there, so when the holder's language left the room nobody was primary any more —
    // the speaker stopped seeing their own words for the rest of the call.

    #[test]
    fn the_speaker_echo_moves_on_when_its_language_leaves_the_room() {
        let primary: PrimaryClaim = Default::default();
        let (en, es) = ("en".to_string(), "es".to_string());
        assert!(primary.owns(&en));
        assert!(!primary.owns(&es));
        // Every English listener leaves: that session ends and hands the role back.
        primary.release(&en);
        assert!(primary.owns(&es), "the surviving session re-elects itself");
    }

    #[test]
    fn only_one_language_echoes_the_speaker_at_a_time() {
        // Two sessions both echoing would show the speaker their own words twice.
        let primary: PrimaryClaim = Default::default();
        let (en, es, de) = ("en".to_string(), "es".to_string(), "de".to_string());
        assert!(primary.owns(&en));
        assert!(!primary.owns(&es));
        assert!(!primary.owns(&de));
        assert!(primary.owns(&en), "asking again does not rotate the role");
    }

    #[test]
    fn a_language_that_never_held_the_echo_cannot_release_it() {
        let primary: PrimaryClaim = Default::default();
        let (en, es) = ("en".to_string(), "es".to_string());
        assert!(primary.owns(&en));
        primary.release(&es);
        assert!(primary.owns(&en), "the real holder keeps the echo");
    }

    #[test]
    fn a_reconnecting_session_hands_the_echo_to_a_healthy_one() {
        // On an upstream drop the holder releases, so the echo follows a session that is
        // actually delivering rather than waiting out someone else's backoff.
        let primary: PrimaryClaim = Default::default();
        let (en, es) = ("en".to_string(), "es".to_string());
        assert!(primary.owns(&en));
        primary.release(&en); // en's socket dropped
        assert!(primary.owns(&es));
        assert!(!primary.owns(&en), "en does not take it back on reconnect");
    }

    // ---- extra_engines_for_turn ----------------------------------------------

    fn speech() -> Vec<String> {
        vec![
            STANDARD_ID.to_string(),
            OPENAI_ID.to_string(),
            GEMINI_ID.to_string(),
        ]
    }

    #[test]
    fn standard_is_never_started_twice_for_one_speaker() {
        // The regression: Standard has `translated_audio`, so a listener choosing it put
        // it in this list — and the caller starts Standard unconditionally anyway. Two
        // upstream sessions per target language, on the tier billed per target language.
        let chosen = vec![STANDARD_ID.to_string()];
        assert!(extra_engines_for_turn(&chosen, &speech()).is_empty());
    }

    #[test]
    fn premium_engines_a_listener_chose_are_kept() {
        let chosen = vec![GEMINI_ID.to_string(), OPENAI_ID.to_string()];
        let got = extra_engines_for_turn(&chosen, &speech());
        assert_eq!(got.len(), 2);
        assert!(got.contains(&GEMINI_ID.to_string()));
        assert!(got.contains(&OPENAI_ID.to_string()));
    }

    #[test]
    fn a_mixed_room_opens_the_premium_engine_and_leaves_standard_to_the_caller() {
        let chosen = vec![STANDARD_ID.to_string(), GEMINI_ID.to_string()];
        assert_eq!(extra_engines_for_turn(&chosen, &speech()), vec![GEMINI_ID]);
    }

    #[test]
    fn two_listeners_on_one_engine_open_one_session() {
        let chosen = vec![GEMINI_ID.to_string(), GEMINI_ID.to_string()];
        assert_eq!(extra_engines_for_turn(&chosen, &speech()), vec![GEMINI_ID]);
    }

    #[test]
    fn an_engine_that_cannot_speak_is_not_opened_here() {
        // A client-direct tier (Cartesia) never opens a server session; the browser talks
        // to the provider itself.
        let chosen = vec![CARTESIA_ID.to_string(), GEMINI_ID.to_string()];
        assert_eq!(extra_engines_for_turn(&chosen, &speech()), vec![GEMINI_ID]);
    }

    #[test]
    fn nobody_cross_language_means_nothing_extra_to_open() {
        assert!(extra_engines_for_turn(&[], &speech()).is_empty());
    }

    #[test]
    fn ids_are_unique_so_two_sessions_never_share_a_claim() {
        let ids: HashSet<u64> = (0..64).map(|_| TranscriptWriter::next_id()).collect();
        assert_eq!(ids.len(), 64);
    }

    /// Minimal engine that only carries metadata — enough to exercise the
    /// registry without the Standard engine's Qwen dependency.
    struct Mock(EngineMetadata);

    #[async_trait]
    impl TranslationEngine for Mock {
        fn metadata(&self) -> &EngineMetadata {
            &self.0
        }
        async fn start_session(&self, _ctx: SpeakerCtx, _deps: SessionDeps) -> SessionOutcome {
            SessionOutcome::Failed
        }
    }

    fn meta(id: &str) -> EngineMetadata {
        EngineMetadata {
            id: id.into(),
            display_name: id.into(),
            tier: "t".into(),
            description: String::new(),
            cost_per_minute: 0.0,
            markup: 0.0,
            input_languages: vec![],
            output_languages: vec![],
            capabilities: EngineCapabilities {
                translated_audio: false,
                cost_scales_per_language: false,
                client_direct: false,
                max_room_size: 4,
            },
        }
    }

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reconcile_adds_new_and_drops_gone_langs() {
        // Speaker started alone (no active sessions); two listeners now present → both
        // are added, in `want` order. This is the core late-joiner fix.
        let (drop, add) = reconcile_langs(&set(&[]), &["es".into(), "fr".into()]);
        assert!(drop.is_empty());
        assert_eq!(add, vec!["es".to_string(), "fr".to_string()]);

        // A listener left (de gone) and one joined (ja) while es stays: drop de, add ja,
        // leave es untouched (gap-free for its listeners).
        let (drop, add) = reconcile_langs(&set(&["es", "de"]), &["es".into(), "ja".into()]);
        assert_eq!(drop, vec!["de".to_string()]);
        assert_eq!(add, vec!["ja".to_string()]);

        // Steady state: nothing to do.
        let (drop, add) = reconcile_langs(&set(&["es", "fr"]), &["fr".into(), "es".into()]);
        assert!(drop.is_empty() && add.is_empty());

        // Everyone left → drop all, add none (the session then drains audio, no cost).
        let (drop, add) = reconcile_langs(&set(&["es"]), &[]);
        assert_eq!(drop, vec!["es".to_string()]);
        assert!(add.is_empty());
    }

    #[test]
    fn get_default_resolve_and_fallback() {
        let mut r = EngineRegistry::new(STANDARD_ID);
        r.register(Arc::new(Mock(meta(STANDARD_ID))));
        r.register(Arc::new(Mock(meta(OPENAI_ID))));

        assert!(r.get(STANDARD_ID).is_some());
        assert!(r.get(OPENAI_ID).is_some());
        assert!(r.get("nope").is_none());

        // The default is always the configured id.
        assert_eq!(r.default().metadata().id, STANDARD_ID);
        // A known id resolves to itself.
        assert_eq!(r.resolve(Some(OPENAI_ID)).metadata().id, OPENAI_ID);
        // An unknown / removed id and an absent id both fall back to the default.
        assert_eq!(r.resolve(Some("removed")).metadata().id, STANDARD_ID);
        assert_eq!(r.resolve(None).metadata().id, STANDARD_ID);

        assert_eq!(r.infos().len(), 2);
        assert_eq!(r.list().count(), 2);
    }

    #[test]
    fn an_unregistered_tier_is_neither_listed_nor_selectable() {
        // How the Pro kill switch (`PRO_TIER_ENABLED`) enforces itself: the tier is not
        // REGISTERED, so it is absent from `/api/engines` AND a request naming it — a
        // stale saved preference, or a hand-crafted one — resolves to the default instead
        // of starting it. Filtering only the listing would hide it from the picker while
        // leaving it reachable.
        let mut r = EngineRegistry::new(STANDARD_ID);
        r.register(Arc::new(Mock(meta(STANDARD_ID))));

        assert!(!r.infos().iter().any(|i| i.id == OPENAI_ID));
        assert_eq!(r.resolve(Some(OPENAI_ID)).metadata().id, STANDARD_ID);
        assert!(r.get(OPENAI_ID).is_none());
        assert_eq!(r.infos().len(), 1);
        assert_eq!(r.list().count(), 1);
    }
}
