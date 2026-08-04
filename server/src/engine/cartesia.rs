//! The **Enhanced** engine: Cartesia real-time STT (Ink-2) + TTS (Sonic-3.5) with
//! per-speaker voice cloning (spec 0108, replacing the previous Enhanced engine, spec 0101).
//!
//! Unlike every other engine, Enhanced is **client-direct**: the browser connects
//! straight to Cartesia using a scoped, short-lived access token minted by the server
//! (`POST /api/sessions/enhanced/session`), and the backend NEVER proxies the audio — that
//! is the whole point of the tier (it drops the server relay hop for low latency).
//!
//! Cartesia does STT and TTS but **not** translation, so the listener-side pipeline routes
//! each finalized transcript through the server's existing Groq translator (a text-only
//! `translate_text` WS round-trip) before speaking it back via Cartesia TTS in the
//! speaker's cloned voice. Audio still never touches the server.
//!
//! So this engine carries metadata ONLY: it appears in `GET /api/engines` (the picker and
//! the user-facing rate), tags `usage_sessions.engine_id`, and supplies the per-source rate
//! the listener-pays meter bills at. It runs no server session — the listener does the work
//! in the browser — so [`CartesiaEngine::start_session`] is never reached on the audio path
//! and returns [`SessionOutcome::Failed`] as a backstop. The `client_direct` capability is
//! what tells every speaking-session call site to skip it (see
//! [`super::metadata::EngineCapabilities::client_direct`]).

use async_trait::async_trait;

use crate::config::CartesiaConfig;
use crate::deepgram::SpeakerCtx;

use super::metadata::{EngineCapabilities, EngineMetadata};
use super::{SessionDeps, SessionOutcome, TranslationEngine, CARTESIA_ID};

/// Cartesia "Enhanced" — a metadata-only, client-direct engine (spec 0108).
pub struct CartesiaEngine {
    meta: EngineMetadata,
}

impl CartesiaEngine {
    pub fn new(config: &CartesiaConfig) -> Self {
        // The Enhanced tier's output languages, from the shared map (spec 0102). The STT +
        // TTS run client-direct (browser ↔ Cartesia), so this is purely the picker metadata.
        let langs: Vec<String> = super::langmap::tier_output_langs("enhanced");
        let meta = EngineMetadata {
            id: CARTESIA_ID.to_string(),
            display_name: "Enhanced".to_string(),
            tier: "enhanced".to_string(),
            // No latency figure here, deliberately. It was wrong (Enhanced is not the
            // fastest tier), and a number in shipped copy becomes a promise the product
            // has to keep on every network. No other tier quotes one either.
            description: "Real-time translation with Cartesia — natural Sonic voice and \
                          voice cloning so everyone is heard in their own voice, \
                          streamed straight to your browser."
                .to_string(),
            cost_per_minute: config.cost_per_minute,
            markup: config.markup,
            input_languages: langs.clone(),
            output_languages: langs,
            capabilities: EngineCapabilities {
                // The SERVER produces no audio for Enhanced (the browser plays Cartesia's
                // TTS locally), so it is excluded from the speaker's PCM-capture decision
                // and the premium fan-out — both filter on `translated_audio`.
                translated_audio: false,
                // Listener-pays bills per active source; surface the "per source you
                // listen to" picker note like the other per-source engines.
                cost_scales_per_language: true,
                // The defining trait: browser ↔ Cartesia directly; no server proxy.
                client_direct: true,
                max_room_size: 4,
            },
        };
        Self { meta }
    }
}

#[async_trait]
impl TranslationEngine for CartesiaEngine {
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
    use crate::config::CartesiaConfig;

    fn cfg() -> CartesiaConfig {
        CartesiaConfig {
            api_key: "sk_car_test".into(),
            stt_model: "ink-2".into(),
            stt_model_by_lang: Default::default(),
            tts_model: "sonic-3.5".into(),
            cost_per_minute: 0.036,
            markup: 0.85,
            voice_cloning_enabled: true,
            default_voice_id: None,
            api_base: "https://api.cartesia.ai".into(),
            stt_endpoint: "wss://api.cartesia.ai/stt/websocket".into(),
            tts_endpoint: "wss://api.cartesia.ai/tts/websocket".into(),
            version: "2026-03-01".into(),
        }
    }

    #[test]
    fn metadata_is_client_direct_enhanced_tier() {
        let e = CartesiaEngine::new(&cfg());
        let m = e.metadata();
        assert_eq!(m.id, CARTESIA_ID);
        assert_eq!(m.display_name, "Enhanced");
        assert_eq!(m.tier, "enhanced");
        assert!(m.capabilities.client_direct);
        // The server streams no audio for it and it doesn't force PCM capture.
        assert!(!m.capabilities.translated_audio);
        assert!(m.capabilities.cost_scales_per_language);
        // User rate = cost × (1 + markup) = 0.036 × 1.85 = 0.0666.
        assert!((m.user_rate_per_minute() - 0.0666).abs() < 1e-9);
        // Keeps the shipped common-language set (spec 0094) intact.
        assert!(m.output_languages.contains(&"en".to_string()));
    }

    #[tokio::test]
    async fn start_session_is_a_backstop_failure() {
        // The server never opens a session for a client-direct engine.
        let e = CartesiaEngine::new(&cfg());
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
            translator: crate::translator::Translator::new(crate::groq::Groq::new(
                "k".into(),
                "openai/gpt-oss-20b".into(),
            )),
        };
        assert!(matches!(
            e.start_session(ctx, deps).await,
            SessionOutcome::Failed
        ));
    }
}
