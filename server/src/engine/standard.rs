//! The **Standard** engine: the original Deepgram (STT) + Groq (translation)
//! pipeline, wrapped as the first [`TranslationEngine`]. Behavior is unchanged —
//! this is the same code path as before spec 0093, just moved behind the trait so
//! routing, billing, and the UI stay engine-agnostic. The hot path (the bounded
//! audio channel and `process_transcripts`) is reused verbatim.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::config::Config;
use crate::deepgram::{self, SpeakerCtx};
use crate::moderation::Moderator;
use crate::protocol::ServerMessage;
use crate::rooms::RoomManager;
use crate::transcripts::TranscriptService;
use crate::translator::Translator;

use super::metadata::{EngineCapabilities, EngineMetadata};
use super::{SessionDeps, TranslationEngine, STANDARD_ID};

/// Capacity of a speaker's bounded audio→Deepgram channel (issue #123 / spec
/// 0065). 100 ms chunks of 32 kbps Opus are a few hundred bytes, so 256 chunks
/// (~25 s) is far above any real backlog while bounding a stalled session to
/// ~100 KiB. Dropping mid-stream chunks would corrupt the WebM container, so on
/// overflow the receive loop ends the session cleanly instead.
const AUDIO_CHANNEL_CAP: usize = 256;

/// Max bytes buffered while detecting (~3 s of 32 kbps Opus ≈ 12 KiB; the cap
/// guards memory against pathological encoders).
const MAX_DETECT_BUFFER: usize = 256 * 1024;

/// Default per-minute server cost + markup when billing isn't configured (guest
/// mode shows an informational rate). Mirrors `config::PricingConfig` defaults.
const DEFAULT_COST_PER_MINUTE: f64 = 0.008;
const DEFAULT_MARKUP: f64 = 0.25;

/// The languages VoxTranslate ships in the UI (see `client/src/scripts/i18n.ts`).
const STANDARD_LANGS: &[&str] = &["it", "en", "es", "fr", "de", "pt", "ja", "zh"];

/// Deepgram STT + Groq translation, behind the engine trait. `config`, `http`,
/// and `translator` are stable for the process lifetime, so they're held here;
/// the room/moderator/transcript services are passed per session ([`SessionDeps`])
/// because they can change after the registry is built.
pub struct StandardEngine {
    meta: EngineMetadata,
    config: Arc<Config>,
    http: reqwest::Client,
    translator: Translator,
}

impl StandardEngine {
    /// Build the Standard engine. Cost/markup come from the billing pricing when
    /// configured, else the shipped defaults.
    pub fn new(config: Arc<Config>, http: reqwest::Client, translator: Translator) -> Self {
        let (cost_per_minute, markup) = config
            .billing
            .as_ref()
            .map(|b| (b.pricing.cost_per_minute, b.pricing.markup_percentage))
            .unwrap_or((DEFAULT_COST_PER_MINUTE, DEFAULT_MARKUP));
        let langs: Vec<String> = STANDARD_LANGS.iter().map(|s| s.to_string()).collect();
        let meta = EngineMetadata {
            id: STANDARD_ID.to_string(),
            display_name: "Standard".to_string(),
            tier: "standard".to_string(),
            description: "Live subtitles via Deepgram speech-to-text and Groq translation. \
                          Translated voice is synthesized on your device."
                .to_string(),
            cost_per_minute,
            markup,
            input_languages: langs.clone(),
            output_languages: langs,
            capabilities: EngineCapabilities {
                translated_audio: false,
                cost_scales_per_language: false,
                max_room_size: 4,
            },
        };
        Self {
            meta,
            config,
            http,
            translator,
        }
    }

    /// Open Deepgram for a known language and spawn the forward + transcript
    /// tasks. (Formerly `lib::start_speaking_session`.)
    async fn start_known(
        &self,
        ctx: SpeakerCtx,
        rooms: Arc<RoomManager>,
        moderator: Arc<Moderator>,
        transcripts: Option<TranscriptService>,
    ) -> Option<mpsc::Sender<Vec<u8>>> {
        match deepgram::open_deepgram_ws(&ctx.speaker_lang, &self.config).await {
            Ok((dg_sink, dg_source)) => {
                let (audio_tx, audio_rx) = mpsc::channel::<Vec<u8>>(AUDIO_CHANNEL_CAP);
                tokio::spawn(deepgram::forward_audio(audio_rx, dg_sink));
                tokio::spawn(deepgram::process_transcripts(
                    dg_source,
                    rooms,
                    self.translator.clone(),
                    moderator,
                    ctx,
                    transcripts,
                ));
                Some(audio_tx)
            }
            Err(e) => {
                tracing::error!("deepgram open failed: {e}");
                rooms.relay_to_peer(
                    &ctx.room,
                    &ctx.speaker_id,
                    &ServerMessage::Error {
                        message: "speech service unavailable".to_string(),
                        code: None,
                    }
                    .to_json(),
                );
                None
            }
        }
    }

    /// Auto-detect variant (spec 0012): buffer the first `auto_detect_buffer_ms`
    /// of audio, probe the language via Deepgram's REST endpoint, then open the
    /// real streaming connection and replay the buffer. The buffer is a valid clip
    /// AND a valid stream prefix because MediaRecorder's chunk #1 carries the WebM
    /// header. (Formerly `lib::start_detecting_session`.)
    fn start_detecting(
        &self,
        ctx: SpeakerCtx,
        rooms: Arc<RoomManager>,
        moderator: Arc<Moderator>,
        transcripts: Option<TranscriptService>,
        participant_row: Option<Uuid>,
    ) -> mpsc::Sender<Vec<u8>> {
        let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(AUDIO_CHANNEL_CAP);
        let config = self.config.clone();
        let http = self.http.clone();
        let translator = self.translator.clone();
        let buffer_ms = self.config.auto_detect_buffer_ms;
        tokio::spawn(async move {
            // Phase 1 — buffer chunks until the deadline, the size cap, or an early
            // stop (the speaker muted / disconnected → channel closed).
            let deadline = tokio::time::sleep(Duration::from_millis(buffer_ms));
            tokio::pin!(deadline);
            let mut buf: Vec<Vec<u8>> = Vec::new();
            let mut buffered = 0usize;
            let mut channel_open = true;
            loop {
                tokio::select! {
                    chunk = audio_rx.recv() => match chunk {
                        Some(c) => {
                            buffered += c.len();
                            buf.push(c);
                            if buffered >= MAX_DETECT_BUFFER {
                                break;
                            }
                        }
                        None => {
                            channel_open = false;
                            break;
                        }
                    },
                    _ = &mut deadline => break,
                }
            }
            if buf.is_empty() {
                return; // stopped before any audio arrived — nothing to detect
            }

            // Phase 2 — REST probe on the buffered clip (non-consuming: the chunks
            // are replayed into the streaming connection in phase 4).
            let clip = buf.concat();
            let (lang, confidence) = match deepgram::detect_language(&http, &config, clip).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("language detect failed: {e}");
                    rooms.relay_to_peer(
                        &ctx.room,
                        &ctx.speaker_id,
                        &ServerMessage::Error {
                            message: "language detection failed — using English".to_string(),
                            code: Some("detect_failed".to_string()),
                        }
                        .to_json(),
                    );
                    ("en".to_string(), None)
                }
            };

            // Phase 3 — apply, unless a manual `set_lang` already resolved it while
            // we were probing (the user's explicit correction always wins).
            let live = rooms.peer_lang(&ctx.room, &ctx.speaker_id);
            let final_lang = if live.as_deref() == Some("auto") {
                rooms.set_peer_lang(&ctx.room, &ctx.speaker_id, &lang);
                if let (Some(pid), Some(svc)) = (participant_row, transcripts.as_ref()) {
                    if let Err(e) = svc.update_participant_lang(pid, &lang).await {
                        tracing::error!("participant lang update failed: {e}");
                    }
                }
                rooms.broadcast(
                    &ctx.room,
                    &ServerMessage::LanguageDetected {
                        peer_id: ctx.speaker_id.clone(),
                        lang: lang.clone(),
                        confidence,
                    }
                    .to_json(),
                );
                lang
            } else {
                live.unwrap_or(lang)
            };
            if !channel_open {
                return; // already stopped — the lang is recorded for the next start
            }

            // Phase 4 — open the real streaming session and replay the buffer in
            // order, then bridge live chunks until the speaker stops.
            let ctx = SpeakerCtx {
                speaker_lang: final_lang.clone(),
                ..ctx
            };
            match deepgram::open_deepgram_ws(&final_lang, &config).await {
                Ok((dg_sink, dg_source)) => {
                    let (dg_tx, dg_rx) = mpsc::channel::<Vec<u8>>(AUDIO_CHANNEL_CAP);
                    tokio::spawn(deepgram::forward_audio(dg_rx, dg_sink));
                    tokio::spawn(deepgram::process_transcripts(
                        dg_source,
                        rooms.clone(),
                        translator.clone(),
                        moderator,
                        ctx,
                        transcripts.clone(),
                    ));
                    // Bounded send (#123): a dedicated task, so awaiting here
                    // backpressures cleanly — if Deepgram stalls, `audio_rx` fills
                    // and the receive loop ends the session. No chunk is dropped, so
                    // the WebM stream stays intact. `send` errs only if the
                    // forwarder is gone.
                    for chunk in buf {
                        if dg_tx.send(chunk).await.is_err() {
                            return;
                        }
                    }
                    while let Some(chunk) = audio_rx.recv().await {
                        if dg_tx.send(chunk).await.is_err() {
                            break;
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("deepgram open after detect failed: {e}");
                    rooms.relay_to_peer(
                        &ctx.room,
                        &ctx.speaker_id,
                        &ServerMessage::Error {
                            message: "speech service unavailable".to_string(),
                            code: None,
                        }
                        .to_json(),
                    );
                }
            }
        });
        audio_tx
    }
}

#[async_trait]
impl TranslationEngine for StandardEngine {
    fn metadata(&self) -> &EngineMetadata {
        &self.meta
    }

    async fn start_session(
        &self,
        ctx: SpeakerCtx,
        deps: SessionDeps,
    ) -> Option<mpsc::Sender<Vec<u8>>> {
        let SessionDeps {
            rooms,
            moderator,
            transcripts,
            participant_row,
        } = deps;
        if ctx.speaker_lang == "auto" {
            // Detection pending: buffer → REST probe → stream (spec 0012).
            Some(self.start_detecting(ctx, rooms, moderator, transcripts, participant_row))
        } else {
            self.start_known(ctx, rooms, moderator, transcripts).await
        }
    }
}
