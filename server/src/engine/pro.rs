//! The **Pro** engine: Google Gemini 3.5 Live Translate (spec 0100).
//!
//! End-to-end speech-to-speech, sitting between Standard and Premium. For one
//! speaker we open **one Gemini Live session per distinct target language** in the
//! room (deduped, capped by a process-wide semaphore), resample the speaker's
//! 24 kHz PCM16 down to the 16 kHz Gemini wants, fan it to every session, and map
//! each session's transcript/audio back to room subtitles + translated audio.
//!
//! This is the OpenAI **Premium** coordinator's shape (capacity reservation,
//! per-language reconnect loop, the engine-agnostic `SessionReader` routing) driving
//! a different upstream — by design (spec 0093) adding an engine reuses this
//! machinery. The Gemini-specific protocol lives in [`super::gemini`]. Captions
//! segment on Gemini's explicit `turnComplete`, with an idle debounce as a fallback.

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

use crate::config::GeminiConfig;
use crate::deepgram::SpeakerCtx;
use crate::protocol::ServerMessage;
use crate::rooms::RoomManager;
use crate::transcripts::{EventKind, TranscriptEvent, TranscriptService};

use super::gemini::{self, GemSink, GemSource, GeminiEvent};
use super::metadata::{EngineCapabilities, EngineMetadata};
use super::{SessionDeps, SessionOutcome, TranslationEngine, GEMINI_ID};

/// Capacity of the speaker's bounded audio channel (mirrors the other engines).
const AUDIO_CHANNEL_CAP: usize = 256;

/// Per-session audio buffer. Each language session drains its own copy; if one
/// stalls (slow/reconnecting), its buffer fills and we drop its chunks rather than
/// back-pressuring the other sessions or the speaker.
const PER_SESSION_AUDIO_CAP: usize = 128;

/// Idle gap (ms) after which the accumulated transcript is flushed as a final
/// caption — a fallback for when `turnComplete` is sparse (cf. premium's debounce).
const SEGMENT_IDLE_MS: u64 = 900;

/// Reconnect backoff bounds (ms) and the cap on *consecutive* failed re-opens
/// before a language session gives up (a persistent failure, e.g. a bad key).
const RECONNECT_BASE_MS: u64 = 500;
const RECONNECT_MAX_MS: u64 = 8000;
const MAX_OPEN_FAILURES: u32 = 6;

/// Why a single Gemini connection ended.
enum ConnOutcome {
    /// The speaker stopped (audio channel closed) — the session is done.
    AudioClosed,
    /// The connection dropped (or the server sent `goAway`) — reconnect with backoff.
    Dropped,
}

/// Output languages exposed in the UI. Gemini auto-detects 70+ input languages and
/// can translate into a large set; we surface the app's shipped UI languages for now
/// (every engine must produce these for mixed-room safety — spec 0094 `commonLangs`).
const PRO_LANGS: &[&str] = &["it", "en", "es", "fr", "de", "pt", "ja", "zh"];

/// Google Gemini 3.5 Live Translate, behind the engine trait.
pub struct ProEngine {
    meta: EngineMetadata,
    config: GeminiConfig,
    /// Process-wide cap on concurrent Gemini sessions (group-room backpressure;
    /// the preview tier limits concurrent sessions, so keep this conservative).
    sessions: Arc<Semaphore>,
}

impl ProEngine {
    pub fn new(config: &GeminiConfig) -> Self {
        let langs: Vec<String> = PRO_LANGS.iter().map(|s| s.to_string()).collect();
        let meta = EngineMetadata {
            id: GEMINI_ID.to_string(),
            display_name: "Premium".to_string(),
            tier: "premium".to_string(),
            description: "Natural speech-to-speech translation by Google Gemini 3.5 \
                          Live — auto-detects 70+ spoken languages and returns a \
                          translated voice with matching subtitles."
                .to_string(),
            cost_per_minute: config.cost_per_minute,
            markup: config.markup,
            input_languages: langs.clone(),
            output_languages: langs,
            capabilities: EngineCapabilities {
                translated_audio: true,
                cost_scales_per_language: true,
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
impl TranslationEngine for ProEngine {
    fn metadata(&self) -> &EngineMetadata {
        &self.meta
    }

    async fn start_session(&self, ctx: SpeakerCtx, deps: SessionDeps) -> SessionOutcome {
        // Target languages = distinct OTHER languages in the room, excluding the
        // speaker's own (Gemini auto-detects the source; translating into it is a
        // no-op). Computed at Start; membership changes reconcile on the next Start.
        let mut targets = deps.rooms.get_room_languages(&ctx.room, &ctx.speaker_id);
        targets.retain(|l| l != &ctx.speaker_lang);
        tracing::info!(
            speaker = %ctx.speaker_id,
            source = %ctx.speaker_lang,
            ?targets,
            "pro: start_session"
        );

        let (audio_tx, audio_rx) = mpsc::channel::<Vec<u8>>(AUDIO_CHANNEL_CAP);

        if targets.is_empty() {
            // Nobody to translate for (alone / all same language): drain the audio so
            // the bounded channel doesn't back-pressure the speaker. No upstream
            // session, no cost — but the call flows normally.
            let mut audio_rx = audio_rx;
            tokio::spawn(async move { while audio_rx.recv().await.is_some() {} });
            return SessionOutcome::Started(audio_tx);
        }

        // Reserve one upstream-session permit per target language — all-or-nothing
        // and WITHOUT blocking (spec 0094). If we can't get them all the engine is at
        // capacity, so the caller falls back to Standard rather than queueing the
        // speaker silently. Dropping `permits` on the early return releases any taken.
        let mut permits = Vec::with_capacity(targets.len());
        for _ in &targets {
            match self.sessions.clone().try_acquire_owned() {
                Ok(p) => permits.push(p),
                Err(_) => return SessionOutcome::AtCapacity,
            }
        }

        tokio::spawn(run_session(
            self.config.clone(),
            ctx,
            deps,
            targets,
            permits,
            audio_rx,
        ));
        SessionOutcome::Started(audio_tx)
    }
}

/// Coordinator for one speaker: spawn a reconnecting task per target language (each
/// holding a pre-acquired permit), resample the captured 24 kHz audio to 16 kHz
/// once, then fan it to all of them. `targets` is non-empty and `permits` has one
/// entry per target (guaranteed by `start_session`).
async fn run_session(
    config: GeminiConfig,
    ctx: SpeakerCtx,
    deps: SessionDeps,
    targets: Vec<String>,
    permits: Vec<OwnedSemaphorePermit>,
    mut audio_rx: mpsc::Receiver<Vec<u8>>,
) {
    let mut feeds: Vec<mpsc::Sender<Vec<u8>>> = Vec::new();
    for ((i, lang), permit) in targets.iter().enumerate().zip(permits) {
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

    // Resample each captured chunk to 16 kHz ONCE (every session needs the same
    // rate), then fan it out. `try_send` so one stalled/reconnecting session never
    // blocks the others (its buffer just drops). 100 ms @ 24 kHz → exactly 1600
    // samples @ 16 kHz, so chunk boundaries stay sample-aligned.
    while let Some(chunk) = audio_rx.recv().await {
        let chunk16 =
            gemini::resample_pcm16_mono(&chunk, gemini::CAPTURE_HZ, gemini::GEMINI_INPUT_HZ);
        for feed in &feeds {
            let _ = feed.try_send(chunk16.clone());
        }
    }
    // Speaker stopped: dropping `feeds` closes each session's channel → its task
    // flushes, ends the Gemini stream, and exits.
}

/// One target language: keep a Gemini session alive across transient drops,
/// reconnecting with capped exponential backoff. Exits when the speaker stops or
/// after too many consecutive failed re-opens.
async fn session_task(
    config: GeminiConfig,
    reader: SessionReader,
    mut feed_rx: mpsc::Receiver<Vec<u8>>,
    _permit: OwnedSemaphorePermit,
) {
    let mut failures: u32 = 0;
    loop {
        match gemini::open_session(&config, &reader.lang).await {
            Ok((sink, source)) => {
                failures = 0; // a successful connect resets the failure budget
                match run_connection(sink, source, &mut feed_rx, &reader).await {
                    ConnOutcome::AudioClosed => return,
                    ConnOutcome::Dropped => {
                        tracing::warn!(lang = %reader.lang, "gemini session dropped — reconnecting");
                    }
                }
            }
            Err(e) => {
                failures += 1;
                tracing::warn!(lang = %reader.lang, failures, "gemini open failed: {e}");
                if failures >= MAX_OPEN_FAILURES {
                    tracing::error!(lang = %reader.lang, "gemini session giving up");
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

/// Drive one live Gemini connection: forward (already-16 kHz) audio, map transcript/
/// audio events to subtitles, and segment captions on `turnComplete` (idle debounce
/// as a fallback). Returns how it ended so the caller can reconnect or finish.
async fn run_connection(
    mut sink: GemSink,
    mut source: GemSource,
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
                    let _ = sink.send(Message::text(gemini::audio_input_json(&c))).await;
                }
                None => {
                    // Speaker stopped: flush, signal end-of-audio, close, finish.
                    if dirty {
                        reader.flush_final(&original, &translated);
                    }
                    let _ = sink.send(Message::text(gemini::audio_stream_end_json())).await;
                    let _ = sink.close().await;
                    return ConnOutcome::AudioClosed;
                }
            },
            msg = source.next() => {
                // Gemini Live sends EVERY server frame as BINARY (UTF-8 JSON) — including
                // setupComplete, transcripts, and the translated audio. Handling only
                // Text would silently drop all output (no voice, no subtitles), so decode
                // Binary too.
                let text = match msg {
                    Some(Ok(Message::Text(t))) => t.to_string(),
                    Some(Ok(Message::Binary(b))) => match std::str::from_utf8(&b) {
                        Ok(s) => s.to_string(),
                        Err(_) => continue,
                    },
                    Some(Ok(Message::Close(_))) | None => {
                        if dirty { reader.flush_final(&original, &translated); }
                        return ConnOutcome::Dropped;
                    }
                    Some(Ok(_)) => continue, // ping/pong
                    Some(Err(e)) => {
                        tracing::warn!("gemini stream error: {e}");
                        if dirty { reader.flush_final(&original, &translated); }
                        return ConnOutcome::Dropped;
                    }
                };
                for event in gemini::parse_server_message(&text) {
                    match event {
                        GeminiEvent::SetupComplete => {
                            tracing::debug!(lang = %reader.lang, "gemini: setup complete");
                        }
                        GeminiEvent::InputTranscript(d) => {
                            original.push_str(&d);
                            dirty = true;
                            if reader.is_primary {
                                reader.emit_interim_to_speaker(&original);
                            }
                            idle.as_mut().reset(Instant::now() + Duration::from_millis(SEGMENT_IDLE_MS));
                        }
                        GeminiEvent::OutputTranscript(d) => {
                            translated.push_str(&d);
                            dirty = true;
                            reader.emit_interim_to_lang(&translated);
                            idle.as_mut().reset(Instant::now() + Duration::from_millis(SEGMENT_IDLE_MS));
                        }
                        GeminiEvent::OutputAudio(pcm) => {
                            reader.emit_audio(audio_seq, &pcm);
                            audio_seq += 1;
                        }
                        // An explicit turn boundary: finalize the segment now.
                        GeminiEvent::TurnComplete => {
                            if dirty {
                                reader.flush_final(&original, &translated);
                                original.clear();
                                translated.clear();
                                dirty = false;
                            }
                            idle.as_mut().reset(Instant::now() + Duration::from_secs(3600));
                        }
                        // The server is about to disconnect — flush and reconnect
                        // proactively rather than losing the tail to a hard cut.
                        GeminiEvent::GoAway => {
                            if dirty { reader.flush_final(&original, &translated); }
                            let _ = sink.close().await;
                            return ConnOutcome::Dropped;
                        }
                        // Errors are logged; if they're fatal the socket closes next
                        // and we reconnect via the Close/None arm above.
                        GeminiEvent::Error(e) => {
                            tracing::warn!(lang = %reader.lang, "gemini session error: {e}");
                        }
                    }
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
/// Routing is engine-agnostic (same broadcast surface the Premium engine uses).
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

    /// Forward one translated-audio chunk (PCM16 @ 24 kHz) to the listeners of this
    /// language. The PCM was base64-decoded on parse (validating it); re-encode for
    /// the JSON frame the client plays via its AudioWorklet.
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

    /// Finalize a segment: record it (once) and broadcast a `subtitle_final` to the
    /// listeners of this language. Each language's listeners get their own targeted
    /// message — so a listener never sees text that differs from the audio they hear.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::moderation::Moderator;
    use crate::rooms::{Peer, PeerTx, RoomManager, Visibility};

    fn cfg(max_sessions: usize) -> GeminiConfig {
        GeminiConfig {
            api_key: "k".into(),
            model: "gemini-3.5-live-translate-preview".into(),
            cost_per_minute: 0.023,
            markup: 0.5,
            max_sessions,
        }
    }

    fn join_peer(rm: &RoomManager, room: &str, id: &str, lang: &str) {
        let (tx, rx, _ovf) = PeerTx::channel(8);
        std::mem::forget(rx); // keep the sender alive so the peer isn't pruned
        let peer = Peer {
            id: id.into(),
            conn: Uuid::new_v4(),
            name: id.into(),
            lang: lang.into(),
            avatar_url: None,
            tx,
        };
        rm.join(room, peer, Visibility::Private).unwrap();
    }

    fn deps(rm: RoomManager) -> SessionDeps {
        SessionDeps {
            rooms: Arc::new(rm),
            moderator: Arc::new(Moderator::from_env()),
            transcripts: None,
            participant_row: None,
        }
    }

    fn speaker_ctx(room: &str, id: &str, lang: &str) -> SpeakerCtx {
        SpeakerCtx {
            room: room.into(),
            speaker_id: id.into(),
            speaker_name: id.into(),
            speaker_lang: lang.into(),
            session_id: Uuid::new_v4(),
            speaker_user_id: None,
            glossary: None,
        }
    }

    #[test]
    fn metadata_labels_gemini_as_premium() {
        let engine = ProEngine::new(&cfg(4));
        let m = engine.metadata();
        assert_eq!(m.id, GEMINI_ID); // stable id unchanged
        assert_eq!(m.display_name, "Premium");
        assert_eq!(m.tier, "premium");
        // Speech-to-speech, per-language billing — like Premium.
        assert!(m.capabilities.translated_audio);
        assert!(m.capabilities.cost_scales_per_language);
        // User rate = cost × (1 + markup) = 0.023 × 1.5.
        assert!((m.user_rate_per_minute() - 0.0345).abs() < 1e-9);
    }

    #[tokio::test]
    async fn at_capacity_returns_atcapacity() {
        // One permit, already taken → no free upstream session for the one target
        // language, so the caller is told to fall back to Standard (spec 0094).
        let engine = ProEngine::new(&cfg(1));
        let _held = engine.sessions.clone().acquire_owned().await.unwrap();

        let rm = RoomManager::new();
        join_peer(&rm, "r", "spk", "it");
        join_peer(&rm, "r", "lis", "en"); // one distinct target language

        let out = engine
            .start_session(speaker_ctx("r", "spk", "it"), deps(rm))
            .await;
        assert!(matches!(out, SessionOutcome::AtCapacity));
    }

    #[tokio::test]
    async fn no_targets_returns_started() {
        // Speaker alone → nothing to translate → Started (no permit, no upstream
        // session, no cost), so the call flows normally.
        let engine = ProEngine::new(&cfg(4));
        let rm = RoomManager::new();
        join_peer(&rm, "r", "spk", "it");

        let out = engine
            .start_session(speaker_ctx("r", "spk", "it"), deps(rm))
            .await;
        assert!(matches!(out, SessionOutcome::Started(_)));
    }
}
