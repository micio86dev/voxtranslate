//! Translation-engine registry (spec 0093).
//!
//! VoxTranslate supports N translation engines behind one trait. Each engine is a
//! self-contained module that declares its [`EngineMetadata`] and opens a
//! per-speaker session from captured audio. Routing, billing, and the UI are
//! engine-agnostic: adding an engine = new module + `register`, nothing else
//! (Open/Closed — no `if engine == X` in the call sites).

pub mod gemini;
pub mod metadata;
pub mod openai;
pub mod premium;
pub mod pro;
pub mod standard;

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::deepgram::SpeakerCtx;
use crate::moderation::Moderator;
use crate::rooms::RoomManager;
use crate::transcripts::TranscriptService;

pub use metadata::{EngineCapabilities, EngineInfo, EngineMetadata};
pub use premium::PremiumEngine;
pub use pro::ProEngine;
pub use standard::StandardEngine;

/// Stable engine ids. Persisted in `usage_sessions.engine_id` and sent in the
/// join payload, so they must never change once shipped.
pub const STANDARD_ID: &str = "standard";
pub const PREMIUM_ID: &str = "premium";
/// Gemini 3.5 Live Translate, the "Pro" tier (spec 0100).
pub const GEMINI_ID: &str = "gemini_live_translate";

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
    /// registry without the Standard engine's Deepgram/Groq dependencies.
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
                max_room_size: 4,
            },
        }
    }

    #[test]
    fn get_default_resolve_and_fallback() {
        let mut r = EngineRegistry::new(STANDARD_ID);
        r.register(Arc::new(Mock(meta(STANDARD_ID))));
        r.register(Arc::new(Mock(meta(PREMIUM_ID))));

        assert!(r.get(STANDARD_ID).is_some());
        assert!(r.get(PREMIUM_ID).is_some());
        assert!(r.get("nope").is_none());

        // The default is always the configured id.
        assert_eq!(r.default().metadata().id, STANDARD_ID);
        // A known id resolves to itself.
        assert_eq!(r.resolve(Some(PREMIUM_ID)).metadata().id, PREMIUM_ID);
        // An unknown / removed id and an absent id both fall back to the default.
        assert_eq!(r.resolve(Some("removed")).metadata().id, STANDARD_ID);
        assert_eq!(r.resolve(None).metadata().id, STANDARD_ID);

        assert_eq!(r.infos().len(), 2);
        assert_eq!(r.list().count(), 2);
    }
}
