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
use std::sync::Arc;

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
}

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
