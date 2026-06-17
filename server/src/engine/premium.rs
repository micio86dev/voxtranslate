//! The **Premium** engine: OpenAI GPT-Realtime-Translate (spec 0093).
//!
//! End-to-end speech-to-speech — the speaker's PCM16 audio streams to OpenAI,
//! which returns translated audio + transcript deltas (one realtime session per
//! output language). This slice (S1) registers the engine and its metadata so the
//! pre-join selector and `GET /api/engines` are real; the session implementation
//! (OpenAI WS client, transcript/audio deltas, capture/playback) lands in S2/S3.

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::config::OpenAiConfig;
use crate::deepgram::SpeakerCtx;
use crate::protocol::ServerMessage;

use super::metadata::{EngineCapabilities, EngineMetadata};
use super::{SessionDeps, TranslationEngine, PREMIUM_ID};

/// Output languages exposed in the UI. OpenAI supports 13 output / 70+ input
/// languages; we surface the app's shipped set for now (all within OpenAI's set)
/// and can widen this list without touching anything else (registry-driven UI).
const PREMIUM_LANGS: &[&str] = &["it", "en", "es", "fr", "de", "pt", "ja", "zh"];

/// OpenAI GPT-Realtime-Translate, behind the engine trait.
pub struct PremiumEngine {
    meta: EngineMetadata,
}

impl PremiumEngine {
    /// Build the Premium engine's metadata from the OpenAI config (cost + markup
    /// drive the displayed rate). The session-time fields (key, model, session cap)
    /// are read by the S2/S3 implementation.
    pub fn new(config: &OpenAiConfig) -> Self {
        let langs: Vec<String> = PREMIUM_LANGS.iter().map(|s| s.to_string()).collect();
        let meta = EngineMetadata {
            id: PREMIUM_ID.to_string(),
            display_name: "Premium".to_string(),
            tier: "premium".to_string(),
            description: "Natural speech-to-speech translation by OpenAI \
                          GPT-Realtime-Translate — translated voice and matching \
                          subtitles generated together."
                .to_string(),
            cost_per_minute: config.cost_per_minute,
            markup: config.markup,
            input_languages: langs.clone(),
            output_languages: langs,
            capabilities: EngineCapabilities {
                translated_audio: true,
                max_room_size: 4,
            },
        };
        Self { meta }
    }
}

#[async_trait]
impl TranslationEngine for PremiumEngine {
    fn metadata(&self) -> &EngineMetadata {
        &self.meta
    }

    async fn start_session(
        &self,
        ctx: SpeakerCtx,
        deps: SessionDeps,
    ) -> Option<mpsc::Sender<Vec<u8>>> {
        // S2/S3 will open OpenAI realtime sessions (one per target language), stream
        // PCM16 audio, and emit transcript + audio deltas. Until then the engine is
        // registered (so the selector and `/api/engines` are real) but not yet
        // functional — surface a clear error rather than silently dropping audio.
        deps.rooms.relay_to_peer(
            &ctx.room,
            &ctx.speaker_id,
            &ServerMessage::Error {
                message: "Premium translation is not yet available.".to_string(),
                code: Some("engine_unavailable".to_string()),
            }
            .to_json(),
        );
        None
    }
}
