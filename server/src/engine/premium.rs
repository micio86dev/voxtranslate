//! The **Premium** engine: OpenAI GPT-Realtime-Translate (spec 0093).
//!
//! End-to-end speech-to-speech. For one speaker we open **one OpenAI realtime
//! session per distinct target language** in the room (deduped, capped by a
//! process-wide semaphore), stream the speaker's PCM16 audio to all of them, and
//! map each session's transcript deltas to room subtitles. Because OpenAI emits
//! append-only deltas with no segment boundary, captions are segmented by an idle
//! debounce. Translated **audio** deltas are forwarded to listeners in S3.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use chrono::Utc;
use futures::{SinkExt as _, StreamExt as _};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio::time::{sleep, Instant};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use crate::config::OpenAiConfig;
use crate::deepgram::SpeakerCtx;
use crate::protocol::ServerMessage;
use crate::rooms::RoomManager;
use crate::transcripts::{EventKind, TranscriptEvent, TranscriptService};

use super::metadata::{EngineCapabilities, EngineMetadata};
use super::openai::{self, OaSink, OaSource, OpenAiEvent};
use super::{SessionDeps, TranslationEngine, PREMIUM_ID};

/// Capacity of the speaker's bounded audio channel (mirrors the Standard path).
const AUDIO_CHANNEL_CAP: usize = 256;

/// Per-session audio buffer. Each language session drains its own copy; if one
/// stalls (slow/reconnecting), its buffer fills and we drop its chunks rather than
/// back-pressuring the other sessions or the speaker.
const PER_SESSION_AUDIO_CAP: usize = 128;

/// Idle gap (ms) after which the accumulated transcript is flushed as a final
/// caption — OpenAI sends no segment/`done` boundary, so a pause delimits a
/// sentence (cf. Deepgram's `utterance_end_ms`).
const SEGMENT_IDLE_MS: u64 = 900;

/// Reconnect backoff bounds (ms) and the cap on *consecutive* failed re-opens
/// before a language session gives up (a persistent failure, e.g. a bad key).
const RECONNECT_BASE_MS: u64 = 500;
const RECONNECT_MAX_MS: u64 = 8000;
const MAX_OPEN_FAILURES: u32 = 6;

/// Why a single OpenAI connection ended.
enum ConnOutcome {
    /// The speaker stopped (audio channel closed) — the session is done.
    AudioClosed,
    /// The connection dropped unexpectedly — reconnect with backoff.
    Dropped,
}

/// Output languages exposed in the UI. OpenAI supports 13 output / 70+ input
/// languages; we surface the app's shipped set for now (all within OpenAI's set).
const PREMIUM_LANGS: &[&str] = &["it", "en", "es", "fr", "de", "pt", "ja", "zh"];

/// OpenAI GPT-Realtime-Translate, behind the engine trait.
pub struct PremiumEngine {
    meta: EngineMetadata,
    config: OpenAiConfig,
    /// Process-wide cap on concurrent OpenAI sessions (group-room backpressure).
    sessions: Arc<Semaphore>,
}

impl PremiumEngine {
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
        Self {
            meta,
            config: config.clone(),
            sessions: Arc::new(Semaphore::new(config.max_sessions.max(1))),
        }
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
        // Target languages = distinct OTHER languages in the room, excluding the
        // speaker's own (translating to the source language is a no-op). Computed
        // at Start; mid-call membership changes reconcile on the next Start (MVP).
        let mut targets = deps.rooms.get_room_languages(&ctx.room, &ctx.speaker_id);
        targets.retain(|l| l != &ctx.speaker_lang);

        let (audio_tx, audio_rx) = mpsc::channel::<Vec<u8>>(AUDIO_CHANNEL_CAP);
        let config = self.config.clone();
        let sem = self.sessions.clone();
        tokio::spawn(run_session(config, sem, ctx, deps, targets, audio_rx));
        Some(audio_tx)
    }
}

/// Coordinator for one speaker: spawn a self-contained, reconnecting task per
/// target language, then fan the speaker's captured audio to all of them.
async fn run_session(
    config: OpenAiConfig,
    sem: Arc<Semaphore>,
    ctx: SpeakerCtx,
    deps: SessionDeps,
    targets: Vec<String>,
    mut audio_rx: mpsc::Receiver<Vec<u8>>,
) {
    let mut feeds: Vec<mpsc::Sender<Vec<u8>>> = Vec::new();
    for (i, lang) in targets.iter().enumerate() {
        // Hold a permit for the whole session lifetime (released when the task
        // ends). Cap reached → skip the remaining languages.
        let Ok(permit) = sem.clone().acquire_owned().await else {
            break;
        };
        let (feed_tx, feed_rx) = mpsc::channel::<Vec<u8>>(PER_SESSION_AUDIO_CAP);
        feeds.push(feed_tx);
        let reader = SessionReader {
            lang: lang.clone(),
            is_primary: i == 0, // one session relays the speaker's original words
            rooms: deps.rooms.clone(),
            transcripts: deps.transcripts.clone(),
            room: ctx.room.clone(),
            speaker_id: ctx.speaker_id.clone(),
            speaker_name: ctx.speaker_name.clone(),
            source_lang: ctx.speaker_lang.clone(),
            session_id: ctx.session_id,
            speaker_user_id: ctx.speaker_user_id,
        };
        tokio::spawn(session_task(config.clone(), reader, feed_rx, permit));
    }

    if feeds.is_empty() {
        // Nothing to translate for (alone / all same language): drain audio so the
        // bounded channel doesn't back-pressure the speaker.
        while audio_rx.recv().await.is_some() {}
        return;
    }

    // Fan each captured PCM16 chunk to every session. `try_send` so one stalled or
    // reconnecting session never blocks the others (its buffer just drops).
    while let Some(chunk) = audio_rx.recv().await {
        for feed in &feeds {
            let _ = feed.try_send(chunk.clone());
        }
    }
    // Speaker stopped: dropping `feeds` closes each session's channel → its task
    // flushes, closes the OpenAI session, and exits.
}

/// One target language: keep an OpenAI session alive across transient drops,
/// reconnecting with capped exponential backoff. Exits when the speaker stops or
/// after too many consecutive failed re-opens.
async fn session_task(
    config: OpenAiConfig,
    reader: SessionReader,
    mut feed_rx: mpsc::Receiver<Vec<u8>>,
    _permit: OwnedSemaphorePermit,
) {
    let mut failures: u32 = 0;
    loop {
        match openai::open_session(&config, &reader.lang).await {
            Ok((sink, source)) => {
                failures = 0; // a successful connect resets the failure budget
                match run_connection(sink, source, &mut feed_rx, &reader).await {
                    ConnOutcome::AudioClosed => return,
                    ConnOutcome::Dropped => {
                        tracing::warn!(lang = %reader.lang, "openai session dropped — reconnecting");
                    }
                }
            }
            Err(e) => {
                failures += 1;
                tracing::warn!(lang = %reader.lang, failures, "openai open failed: {e}");
                if failures >= MAX_OPEN_FAILURES {
                    tracing::error!(lang = %reader.lang, "openai session giving up");
                    return;
                }
            }
        }
        // Backoff before the next attempt. A speaker who stopped mid-backoff is
        // detected on the next connection (its first recv yields a closed channel),
        // or right here when the feed channel has closed.
        let backoff = (RECONNECT_BASE_MS << failures.min(4)).min(RECONNECT_MAX_MS);
        sleep(Duration::from_millis(backoff)).await;
        if feed_rx.is_closed() {
            return;
        }
    }
}

/// Drive one live OpenAI connection: forward audio, map transcript/audio deltas to
/// subtitles, and segment captions by idle debounce. Returns how it ended so the
/// caller can reconnect or finish.
async fn run_connection(
    mut sink: OaSink,
    mut source: OaSource,
    feed_rx: &mut mpsc::Receiver<Vec<u8>>,
    reader: &SessionReader,
) -> ConnOutcome {
    let mut original = String::new(); // speaker's words (input transcript)
    let mut translated = String::new(); // this session's output language
    let mut dirty = false;
    let mut audio_seq: u64 = 0; // orders translated-audio chunks for the client
    let idle = sleep(Duration::from_millis(SEGMENT_IDLE_MS));
    tokio::pin!(idle);

    loop {
        tokio::select! {
            chunk = feed_rx.recv() => match chunk {
                Some(c) => {
                    let _ = sink.send(Message::text(openai::audio_append_json(&c))).await;
                }
                None => {
                    // Speaker stopped: flush, ask the server to close, finish.
                    if dirty {
                        reader.flush_final(&original, &translated);
                    }
                    let _ = sink.send(Message::text(openai::session_close_json())).await;
                    let _ = sink.close().await;
                    return ConnOutcome::AudioClosed;
                }
            },
            msg = source.next() => {
                let text = match msg {
                    Some(Ok(Message::Text(t))) => t,
                    Some(Ok(Message::Close(_))) | None => {
                        if dirty { reader.flush_final(&original, &translated); }
                        return ConnOutcome::Dropped;
                    }
                    Some(Ok(_)) => continue, // ping/pong/binary
                    Some(Err(e)) => {
                        tracing::warn!("openai stream error: {e}");
                        if dirty { reader.flush_final(&original, &translated); }
                        return ConnOutcome::Dropped;
                    }
                };
                match openai::parse_openai_event(text.as_str()) {
                    OpenAiEvent::InputTranscriptDelta(d) => {
                        original.push_str(&d);
                        dirty = true;
                        if reader.is_primary {
                            reader.emit_interim_to_speaker(&original);
                        }
                        idle.as_mut().reset(Instant::now() + Duration::from_millis(SEGMENT_IDLE_MS));
                    }
                    OpenAiEvent::OutputTranscriptDelta(d) => {
                        translated.push_str(&d);
                        dirty = true;
                        reader.emit_interim_to_lang(&translated);
                        idle.as_mut().reset(Instant::now() + Duration::from_millis(SEGMENT_IDLE_MS));
                    }
                    OpenAiEvent::OutputAudioDelta(pcm) => {
                        reader.emit_audio(audio_seq, &pcm);
                        audio_seq += 1;
                    }
                    // A `closed` we didn't initiate (our own close path returns
                    // above without reading it) is an unexpected drop → reconnect.
                    OpenAiEvent::Closed => {
                        if dirty { reader.flush_final(&original, &translated); }
                        return ConnOutcome::Dropped;
                    }
                    // Errors are documented as recoverable (the session stays open),
                    // so log and keep going rather than tearing down.
                    OpenAiEvent::Error(e) => {
                        tracing::warn!(lang = %reader.lang, "openai session error: {e}");
                    }
                    OpenAiEvent::Other => {}
                }
            }
            _ = &mut idle => {
                if dirty {
                    reader.flush_final(&original, &translated);
                    original.clear();
                    translated.clear();
                    dirty = false;
                }
                idle.as_mut().reset(Instant::now() + Duration::from_secs(3600));
            }
        }
    }
}

/// Emit context for one language session — turns transcript/audio into subtitles.
struct SessionReader {
    lang: String,
    is_primary: bool,
    rooms: Arc<RoomManager>,
    transcripts: Option<TranscriptService>,
    room: String,
    speaker_id: String,
    speaker_name: String,
    source_lang: String,
    session_id: Uuid,
    speaker_user_id: Option<Uuid>,
}

impl SessionReader {
    /// Live translated caption to the listeners of this session's language.
    fn emit_interim_to_lang(&self, translated: &str) {
        self.rooms.broadcast_to_lang(
            &self.room,
            &self.lang,
            &ServerMessage::SubtitleInterim {
                speaker_id: self.speaker_id.clone(),
                speaker_name: self.speaker_name.clone(),
                text: translated.to_string(),
                lang: self.source_lang.clone(),
            }
            .to_json(),
        );
    }

    /// Forward one translated-audio chunk to the listeners of this language. The
    /// PCM was base64-decoded on parse (validating it); re-encode for the JSON
    /// frame the client plays via its AudioWorklet.
    fn emit_audio(&self, seq: u64, pcm16: &[u8]) {
        let b64 = base64::engine::general_purpose::STANDARD.encode(pcm16);
        self.rooms.broadcast_to_lang(
            &self.room,
            &self.lang,
            &ServerMessage::TranslatedAudio {
                speaker_id: self.speaker_id.clone(),
                lang: self.lang.clone(),
                seq,
                pcm16_b64: b64,
            }
            .to_json(),
        );
    }

    /// Live original caption back to the speaker (their own words).
    fn emit_interim_to_speaker(&self, original: &str) {
        self.rooms.relay_to_peer(
            &self.room,
            &self.speaker_id,
            &ServerMessage::SubtitleInterim {
                speaker_id: self.speaker_id.clone(),
                speaker_name: self.speaker_name.clone(),
                text: original.to_string(),
                lang: self.source_lang.clone(),
            }
            .to_json(),
        );
    }

    /// Finalize a segment: record it (once) and broadcast a `subtitle_final` to
    /// the listeners of this language. The translations map carries this one
    /// language; each language's listeners get their own targeted message — so a
    /// listener never sees text that differs from the audio they hear.
    fn flush_final(&self, original: &str, translated: &str) {
        if translated.trim().is_empty() && original.trim().is_empty() {
            return;
        }
        let mut translations = HashMap::new();
        translations.insert(self.lang.clone(), translated.trim().to_string());
        if let Some(svc) = self.transcripts.as_ref() {
            svc.record(TranscriptEvent {
                session_id: self.session_id,
                kind: EventKind::Speech,
                speaker_peer_id: self.speaker_id.clone(),
                speaker_user_id: self.speaker_user_id,
                speaker_name: self.speaker_name.clone(),
                original_text: original.trim().to_string(),
                original_lang: self.source_lang.clone(),
                translations: translations.clone(),
                ts: Utc::now(),
            });
        }
        self.rooms.broadcast_to_lang(
            &self.room,
            &self.lang,
            &ServerMessage::SubtitleFinal {
                speaker_id: self.speaker_id.clone(),
                speaker_name: self.speaker_name.clone(),
                original: original.trim().to_string(),
                lang: self.source_lang.clone(),
                translations,
            }
            .to_json(),
        );
    }
}
