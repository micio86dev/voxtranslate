//! The **Enhanced** engine: Soniox real-time STT + translation (spec 0101).
//!
//! Unlike every other engine, Enhanced is **client-direct**: the browser connects
//! straight to Soniox using a scoped, single-use temp key minted by the server
//! (`POST /api/soniox/session`), and the backend NEVER proxies the audio — that's the
//! whole point of the tier (it drops the server relay hop for ~sub-250 ms end to end).
//!
//! So this engine carries metadata ONLY: it appears in `GET /api/engines` (the picker
//! and the user-facing rate), tags `usage_sessions.engine_id`, and supplies the
//! per-source rate the listener-pays meter bills at. It runs no server session — the
//! listener does the work in the browser — so [`SonioxEngine::start_session`] is never
//! reached on the audio path and returns [`SessionOutcome::Failed`] as a backstop. The
//! `client_direct` capability is what tells every speaking-session call site to skip it
//! (see [`super::metadata::EngineCapabilities::client_direct`]).

use async_trait::async_trait;

use crate::config::SonioxConfig;
use crate::deepgram::SpeakerCtx;

use super::metadata::{EngineCapabilities, EngineMetadata};
use super::{SessionDeps, SessionOutcome, TranslationEngine, SONIOX_ID};

/// Soniox "Enhanced" — a metadata-only, client-direct engine (spec 0101).
pub struct SonioxEngine {
    meta: EngineMetadata,
}

impl SonioxEngine {
    pub fn new(config: &SonioxConfig) -> Self {
        // Soniox's 60+ output languages, from the shared map (spec 0102). The translation
        // runs client-direct (browser ↔ Soniox), so this is purely the picker metadata.
        let langs: Vec<String> = super::langmap::tier_output_langs("enhanced");
        let meta = EngineMetadata {
            id: SONIOX_ID.to_string(),
            display_name: "Enhanced".to_string(),
            tier: "enhanced".to_string(),
            description: "Real-time translation by Soniox — 60+ languages with \
                          auto-detection, streamed straight to your browser for the \
                          lowest latency."
                .to_string(),
            cost_per_minute: config.cost_per_minute,
            markup: config.markup,
            input_languages: langs.clone(),
            output_languages: langs,
            capabilities: EngineCapabilities {
                // The SERVER produces no audio for Enhanced (the browser plays Soniox's
                // TTS locally), so it is excluded from the speaker's PCM-capture decision
                // and the premium fan-out — both filter on `translated_audio`.
                translated_audio: false,
                // Listener-pays bills per active source; surface the "per source you
                // listen to" picker note like the other per-source engines.
                cost_scales_per_language: true,
                // The defining trait: browser ↔ Soniox directly; no server proxy.
                client_direct: true,
                max_room_size: 4,
            },
        };
        Self { meta }
    }
}

#[async_trait]
impl TranslationEngine for SonioxEngine {
    fn metadata(&self) -> &EngineMetadata {
        &self.meta
    }

    /// Never reached on the server audio path: a client-direct engine is filtered out of
    /// every speaking-session call site by its `client_direct` capability (the listener
    /// translates in-browser). `Failed` is a defensive backstop only.
    async fn start_session(&self, _ctx: SpeakerCtx, _deps: SessionDeps) -> SessionOutcome {
        SessionOutcome::Failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SonioxConfig, SonioxRegionConfig};

    fn cfg() -> SonioxConfig {
        let region = || SonioxRegionConfig {
            api_key: "k".into(),
            stt_endpoint: "wss://stt".into(),
            tts_endpoint: "wss://tts".into(),
        };
        SonioxConfig {
            stt_model: "stt-rt-v5".into(),
            cost_per_minute: 0.015,
            markup: 0.85,
            us: region(),
            eu: region(),
            jp: region(),
        }
    }

    #[test]
    fn metadata_is_client_direct_enhanced_tier() {
        let e = SonioxEngine::new(&cfg());
        let m = e.metadata();
        assert_eq!(m.id, SONIOX_ID);
        assert_eq!(m.display_name, "Enhanced");
        assert_eq!(m.tier, "enhanced");
        assert!(m.capabilities.client_direct);
        // The server streams no audio for it and it doesn't force PCM capture.
        assert!(!m.capabilities.translated_audio);
        assert!(m.capabilities.cost_scales_per_language);
        // User rate = cost × (1 + markup) = 0.015 × 1.85 = 0.02775.
        assert!((m.user_rate_per_minute() - 0.02775).abs() < 1e-9);
        // Keeps the shipped common-language set (spec 0094) intact.
        assert!(m.output_languages.contains(&"en".to_string()));
    }

    #[tokio::test]
    async fn start_session_is_a_backstop_failure() {
        // The server never opens a session for a client-direct engine.
        let e = SonioxEngine::new(&cfg());
        let ctx = SpeakerCtx {
            room: "r".into(),
            speaker_id: "s".into(),
            speaker_name: "S".into(),
            speaker_lang: "en".into(),
            session_id: uuid::Uuid::new_v4(),
            speaker_user_id: None,
            glossary: None,
        };
        let deps = SessionDeps {
            rooms: std::sync::Arc::new(crate::rooms::RoomManager::new()),
            moderator: std::sync::Arc::new(crate::moderation::Moderator::from_terms(
                std::iter::empty::<&str>(),
            )),
            transcripts: None,
            participant_row: None,
            listener_pays: true,
            pcm_input: false,
        };
        assert!(matches!(
            e.start_session(ctx, deps).await,
            SessionOutcome::Failed
        ));
    }
}
