//! The **Premium** engine: Google Gemini 3.5 Live Translate (spec 0100).
//!
//! End-to-end speech-to-speech (the top, "Premium" tier). For one speaker we open
//! **one Gemini Live session per distinct target language** in the room (deduped,
//! capped by a process-wide semaphore), resample the speaker's 24 kHz PCM16 down to
//! the 16 kHz Gemini wants, fan it to every session, and map each session's
//! transcript/audio back to room subtitles + translated audio.
//!
//! This shares the OpenAI **Pro** coordinator's shape ([`super::pro`]: capacity
//! reservation, per-language reconnect loop, live target reconcile, the
//! engine-agnostic `SessionReader` routing) driving a different upstream — by design
//! (spec 0093) adding an engine reuses this machinery. The Gemini-specific protocol
//! lives in [`super::gemini`]. Captions segment on Gemini's explicit `turnComplete`,
//! with an idle debounce as a fallback.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use chrono::Utc;
use futures::{SinkExt as _, StreamExt as _};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio::time::{interval, sleep, Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use crate::config::GeminiConfig;
use crate::deepgram::SpeakerCtx;
use crate::protocol::ServerMessage;
use crate::rooms::RoomManager;
use crate::transcripts::{EventKind, TranscriptEvent, TranscriptService};

use super::gemini::{self, GemSink, GemSource, GeminiEvent};
use super::metadata::{EngineCapabilities, EngineMetadata};
use super::{reconcile_langs, SessionDeps, SessionOutcome, TranslationEngine, GEMINI_ID};

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

/// How often a live session re-checks the room's target languages. Targets are
/// fixed at the first `Start`, so a peer who joins (or whose `auto` language
/// resolves) *after* the speaker began talking would never be translated for — a
/// new language is added/removed within this interval instead. Cheap (a room-map
/// read per active speaker); 1 s keeps a new joiner's wait short.
const RECONCILE_MS: u64 = 1000;

/// Why a single Gemini connection ended.
enum ConnOutcome {
    /// The speaker stopped (audio channel closed) — the session is done.
    AudioClosed,
    /// The connection dropped (or the server sent `goAway`) — reconnect with backoff.
    Dropped,
}

/// Google Gemini 3.5 Live Translate, behind the engine trait.
pub struct PremiumEngine {
    meta: EngineMetadata,
    config: GeminiConfig,
    /// Process-wide cap on concurrent Gemini sessions (group-room backpressure;
    /// the preview tier limits concurrent sessions, so keep this conservative).
    sessions: Arc<Semaphore>,
}

impl PremiumEngine {
    pub fn new(config: &GeminiConfig) -> Self {
        // The full union from the shared map (spec 0102): Premium is the universal-fallback
        // tier — any language we offer must be producible here.
        let langs: Vec<String> = super::langmap::tier_output_langs("premium");
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
                client_direct: false,
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

    async fn start_session(&self, ctx: SpeakerCtx, deps: SessionDeps) -> SessionOutcome {
        // INITIAL target languages = distinct OTHER languages in the room, excluding
        // the speaker's own (Gemini auto-detects the source; translating into it is a
        // no-op). `run_session` then reconciles these live against room membership, so a
        // peer who joins (or whose `auto` resolves) after Start is picked up too.
        // Listener-pays (spec 0099): a paid Gemini session is opened for a language
        // only when a listener of that language chose THIS engine; otherwise every
        // distinct other language (legacy speaker-pays).
        let mut targets = if deps.listener_pays {
            deps.rooms
                .target_langs_for_engine(&ctx.room, &ctx.speaker_id, GEMINI_ID)
        } else {
            deps.rooms.get_room_languages(&ctx.room, &ctx.speaker_id)
        };
        targets.retain(|l| l != &ctx.speaker_lang);
        tracing::info!(
            speaker = %ctx.speaker_id,
            source = %ctx.speaker_lang,
            ?targets,
            "premium: start_session"
        );

        let (audio_tx, audio_rx) = mpsc::channel::<Vec<u8>>(AUDIO_CHANNEL_CAP);

        // Reserve one upstream-session permit per INITIAL target language —
        // all-or-nothing and WITHOUT blocking (spec 0094). If we can't get them all the
        // engine is at capacity, so the caller falls back to Standard rather than
        // queueing the speaker silently. Dropping `initial` on the early return releases
        // any taken. Empty targets (alone / all same language) reserve nothing — zero
        // cost — but we still spawn the session so it picks up later joiners by reconcile.
        let mut initial = Vec::with_capacity(targets.len());
        for lang in targets {
            match self.sessions.clone().try_acquire_owned() {
                Ok(permit) => initial.push((lang, permit)),
                Err(_) => return SessionOutcome::AtCapacity,
            }
        }

        tokio::spawn(run_session(
            self.config.clone(),
            ctx,
            deps,
            self.sessions.clone(),
            initial,
            audio_rx,
        ));
        SessionOutcome::Started(audio_tx)
    }
}

/// Coordinator for one speaker: keep one reconnecting task per *currently-present*
/// target language, resample the captured 24 kHz audio to 16 kHz once, and fan it to
/// all of them — while **reconciling** the language set against live room membership
/// so a late joiner (or a now-resolved `auto` language) starts being translated for
/// without restarting the existing sessions. `initial` carries the targets known at
/// `Start` (each with a pre-acquired permit); it may be empty.
async fn run_session(
    config: GeminiConfig,
    ctx: SpeakerCtx,
    deps: SessionDeps,
    sessions: Arc<Semaphore>,
    initial: Vec<(String, OwnedSemaphorePermit)>,
    mut audio_rx: mpsc::Receiver<Vec<u8>>,
) {
    // lang → its audio feed. A language session self-exits (flush, close the Gemini
    // stream, release its permit) when its feed sender is dropped, so removing a
    // language is just a map removal. `primary` names the one session that echoes the
    // speaker's own words back to them, so the echo isn't duplicated across languages.
    let mut active: HashMap<String, mpsc::Sender<Vec<u8>>> = HashMap::new();
    let mut primary: Option<String> = None;
    for (lang, permit) in initial {
        spawn_lang_session(
            &config,
            &deps,
            &ctx,
            lang,
            permit,
            &mut active,
            &mut primary,
        );
    }

    let mut reconcile = interval(Duration::from_millis(RECONCILE_MS));
    reconcile.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            chunk = audio_rx.recv() => match chunk {
                Some(chunk) => {
                    // Resample to 16 kHz ONCE (every session needs the same rate), then
                    // fan out. `try_send` so one stalled/reconnecting session never blocks
                    // the others (its buffer just drops). 100 ms @ 24 kHz → exactly 1600
                    // samples @ 16 kHz, so chunk boundaries stay sample-aligned.
                    let chunk16 = gemini::resample_pcm16_mono(
                        &chunk,
                        gemini::CAPTURE_HZ,
                        gemini::GEMINI_INPUT_HZ,
                    );
                    for feed in active.values() {
                        let _ = feed.try_send(chunk16.clone());
                    }
                }
                // Speaker stopped: dropping `active` closes every feed → each task
                // flushes, ends its Gemini stream, releases its permit, and exits.
                None => return,
            },
            _ = reconcile.tick() => {
                let mut want = if deps.listener_pays {
                    deps.rooms
                        .target_langs_for_engine(&ctx.room, &ctx.speaker_id, GEMINI_ID)
                } else {
                    deps.rooms.get_room_languages(&ctx.room, &ctx.speaker_id)
                };
                want.retain(|l| l != &ctx.speaker_lang);
                let active_keys: HashSet<String> = active.keys().cloned().collect();
                let (to_drop, to_add) = reconcile_langs(&active_keys, &want);
                // Drop languages whose listeners all left / switched away: removing the
                // feed sender ends that task and frees its permit.
                for lang in &to_drop {
                    active.remove(lang);
                    if primary.as_ref() == Some(lang) {
                        primary = None; // the primary's listeners left; re-elect on next add
                    }
                }
                // Add newly-present languages, best-effort: if the engine is at capacity
                // right now, skip and retry next tick when a permit frees (never tear
                // down the languages already streaming).
                for lang in to_add {
                    match sessions.clone().try_acquire_owned() {
                        Ok(permit) => {
                            spawn_lang_session(&config, &deps, &ctx, lang, permit, &mut active, &mut primary);
                        }
                        Err(_) => break,
                    }
                }
            }
        }
    }
}

/// Spawn the reconnecting upstream task for one target `lang`, register its audio
/// feed in `active`, and make it `primary` (the session that echoes the speaker's own
/// words) when no active session currently is.
fn spawn_lang_session(
    config: &GeminiConfig,
    deps: &SessionDeps,
    ctx: &SpeakerCtx,
    lang: String,
    permit: OwnedSemaphorePermit,
    active: &mut HashMap<String, mpsc::Sender<Vec<u8>>>,
    primary: &mut Option<String>,
) {
    let is_primary = primary.is_none();
    let (feed_tx, feed_rx) = mpsc::channel::<Vec<u8>>(PER_SESSION_AUDIO_CAP);
    let reader = SessionReader {
        lang: lang.clone(),
        is_primary,
        rooms: deps.rooms.clone(),
        transcripts: deps.transcripts.clone(),
        room: ctx.room.clone(),
        speaker_id: ctx.speaker_id.clone(),
        speaker_name: ctx.speaker_name.clone(),
        source_lang: ctx.speaker_lang.clone(),
        session_id: ctx.session_id,
        speaker_user_id: ctx.speaker_user_id,
        listener_pays: deps.listener_pays,
    };
    tokio::spawn(session_task(config.clone(), reader, feed_rx, permit));
    if is_primary {
        *primary = Some(lang.clone());
    }
    active.insert(lang, feed_tx);
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
        let connect_started = Instant::now();
        match gemini::open_session(&config, &reader.lang).await {
            Ok((sink, source)) => {
                failures = 0; // a successful connect resets the failure budget
                // latency: the WS connect + setup-send cost. Audio buffered during this
                // window waits, so it lands on the FIRST utterance's critical path unless
                // the session was pre-warmed (opened before the speaker talks). Aggregated
                // into the `voxtranslate_gemini_connect_ms` histogram (/metrics).
                let connect_ms = connect_started.elapsed().as_millis() as u64;
                crate::metrics::record_gemini_connect(connect_ms);
                tracing::debug!(lang = %reader.lang, connect_ms, "gemini.latency: session connected");
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
    // latency: per-segment time-to-first-audio. `seg_first_in` marks when this
    // segment's first audio chunk was forwarded to Gemini; on the first translated
    // audio back we record the gap (the model's ear-voice span) once, then reset both
    // at the next turn boundary so every segment is measured.
    let mut seg_first_in: Option<Instant> = None;
    let mut seg_ttfa_logged = false;
    let idle = sleep(Duration::from_millis(SEGMENT_IDLE_MS));
    tokio::pin!(idle);

    loop {
        tokio::select! {
            chunk = feed_rx.recv() => match chunk {
                Some(c) => {
                    if seg_first_in.is_none() {
                        seg_first_in = Some(Instant::now());
                    }
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
                            if !seg_ttfa_logged {
                                if let Some(t0) = seg_first_in {
                                    // Per-segment ear-voice span → the
                                    // `voxtranslate_gemini_ttfa_ms` histogram (/metrics),
                                    // not a per-segment log line (too chatty at scale).
                                    let ttfa_ms = t0.elapsed().as_millis() as u64;
                                    crate::metrics::record_gemini_ttfa(ttfa_ms);
                                    tracing::debug!(lang = %reader.lang, ttfa_ms, "gemini.latency: first translated audio (ear-voice span)");
                                    seg_ttfa_logged = true;
                                }
                            }
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
                            // New segment starts on the next audio: re-arm the TTFA probe.
                            seg_first_in = None;
                            seg_ttfa_logged = false;
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
                seg_first_in = None;
                seg_ttfa_logged = false;
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
    /// Listener-pays (spec 0099): deliver this Gemini output only to the listeners who
    /// chose Gemini ("Premium"), not every listener of the language.
    listener_pays: bool,
}

impl SessionReader {
    /// Deliver one output frame to this language's listeners. In listener-pays mode
    /// that's the `(lang, Gemini)` subset; otherwise every listener of the language.
    fn deliver(&self, message: &str) {
        if self.listener_pays {
            self.rooms
                .broadcast_to_lang_engine(&self.room, &self.lang, GEMINI_ID, message);
        } else {
            self.rooms
                .broadcast_to_lang(&self.room, &self.lang, message);
        }
    }

    /// Live translated caption to the listeners of this session's language.
    fn emit_interim_to_lang(&self, translated: &str) {
        self.deliver(
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
        self.deliver(
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
        self.deliver(
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
            voice: None,
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
            engine: "standard".to_string(),
            avatar_url: None,
            tx,
            speaking: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        rm.join(room, peer, Visibility::Private).unwrap();
    }

    fn deps(rm: RoomManager) -> SessionDeps {
        SessionDeps {
            rooms: Arc::new(rm),
            moderator: Arc::new(Moderator::from_env()),
            transcripts: None,
            participant_row: None,
            listener_pays: false,
            pcm_input: false,
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
        let engine = PremiumEngine::new(&cfg(4));
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
        let engine = PremiumEngine::new(&cfg(1));
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
        let engine = PremiumEngine::new(&cfg(4));
        let rm = RoomManager::new();
        join_peer(&rm, "r", "spk", "it");

        let out = engine
            .start_session(speaker_ctx("r", "spk", "it"), deps(rm))
            .await;
        assert!(matches!(out, SessionOutcome::Started(_)));
    }

    /// Real end-to-end AUDIO-latency probe against the LIVE Gemini Live API: drive
    /// the actual engine (`start_session` → resample → Gemini → broadcast) and time
    /// when the listener peer receives the first `translated_audio` frame, measured
    /// from when the first audio chunk was fed in. This is the server-internal
    /// ear-voice span — what local users feel, minus only the browser/network legs.
    ///
    /// `#[ignore]` because it needs a real key + network and bills Gemini minutes.
    /// Run it explicitly (it prints with --nocapture):
    /// ```text
    /// GOOGLE_AI_API_KEY=... GEMINI_LATENCY_PCM=/tmp/gemtest/utt_24k.pcm \
    ///   cargo test -p voxtranslate-server --lib \
    ///   engine::premium::tests::premium_audio_latency_e2e -- --ignored --nocapture
    /// ```
    /// `GEMINI_LATENCY_PCM` must be mono PCM16 little-endian at 24 kHz (the capture
    /// rate the engine resamples from).
    #[tokio::test]
    #[ignore]
    async fn premium_audio_latency_e2e() {
        use std::sync::atomic::AtomicBool;

        let _ = dotenvy::dotenv();
        let key = match std::env::var("GOOGLE_AI_API_KEY") {
            Ok(k) if !k.trim().is_empty() => k,
            _ => {
                eprintln!("skip: set GOOGLE_AI_API_KEY");
                return;
            }
        };
        let pcm_path = match std::env::var("GEMINI_LATENCY_PCM") {
            Ok(p) if !p.trim().is_empty() => p,
            _ => {
                eprintln!("skip: set GEMINI_LATENCY_PCM to a 24kHz mono PCM16 file");
                return;
            }
        };
        let pcm = std::fs::read(&pcm_path).expect("read PCM sample");
        let in_dur = pcm.len() as f64 / 2.0 / 24000.0;
        let model = std::env::var("GEMINI_LIVE_TRANSLATE_MODEL")
            .unwrap_or_else(|_| "gemini-3.5-live-translate-preview".into());

        let mut config = cfg(4);
        config.api_key = key;
        config.model = model;

        // Room: speaker (it) + a listener (en) whose receiver we keep so we can read
        // the translated audio the engine broadcasts.
        let rm = RoomManager::new();
        join_peer(&rm, "lat", "spk", "it");
        let (tx, mut rx, _ovf) = PeerTx::channel(2048);
        let listener = Peer {
            id: "lis".into(),
            conn: Uuid::new_v4(),
            name: "lis".into(),
            lang: "en".into(),
            engine: GEMINI_ID.to_string(),
            avatar_url: None,
            tx,
            speaking: Arc::new(AtomicBool::new(false)),
        };
        rm.join("lat", listener, Visibility::Private).unwrap();

        let engine = PremiumEngine::new(&config);
        let audio_tx = match engine
            .start_session(speaker_ctx("lat", "spk", "it"), deps(rm))
            .await
        {
            SessionOutcome::Started(tx) => tx,
            _ => panic!("expected Started session outcome (engine at capacity or failed)"),
        };

        // Feed 24kHz PCM16 in 100ms chunks, in real time (simulate a live mic).
        const BYTES_PER_CHUNK: usize = 24000 * 2 * 100 / 1000; // 4800
        let t0 = Instant::now();
        let feeder = {
            let audio_tx = audio_tx.clone();
            let pcm = pcm.clone();
            tokio::spawn(async move {
                for ch in pcm.chunks(BYTES_PER_CHUNK) {
                    if audio_tx.send(ch.to_vec()).await.is_err() {
                        break;
                    }
                    sleep(Duration::from_millis(100)).await;
                }
            })
        };

        let mut first_subtitle: Option<u128> = None;
        let mut first_audio: Option<u128> = None;
        let mut audio_frames = 0u32;
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match tokio::time::timeout(left, rx.recv()).await {
                Ok(Some(msg)) => {
                    let v: serde_json::Value = serde_json::from_str(&msg).unwrap_or_default();
                    match v["type"].as_str() {
                        Some("subtitle_interim") | Some("subtitle_final")
                            if first_subtitle.is_none() =>
                        {
                            first_subtitle = Some(t0.elapsed().as_millis());
                        }
                        Some("translated_audio") => {
                            if first_audio.is_none() {
                                first_audio = Some(t0.elapsed().as_millis());
                            }
                            audio_frames += 1;
                        }
                        _ => {}
                    }
                    if first_audio.is_some() && audio_frames >= 25 {
                        break; // enough to confirm a steady stream
                    }
                }
                _ => break,
            }
        }
        feeder.abort();
        drop(audio_tx);

        println!("\n=== LOCAL E2E Premium (engine→Gemini→listener), from first audio fed ===");
        println!("  input audio duration   : {in_dur:.2}s");
        println!("  first translated CAPTION: {first_subtitle:?} ms");
        println!("  first translated AUDIO  : {first_audio:?} ms  <- server-internal ear-voice span");
        println!("  translated audio frames : {audio_frames}");
        assert!(
            first_audio.is_some(),
            "expected at least one translated_audio frame from Gemini"
        );
    }
}
