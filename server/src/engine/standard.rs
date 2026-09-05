//! The **Standard** engine: Qwen realtime (Alibaba Cloud Model Studio).
//!
//! End-to-end speech-to-speech, and the tier every room falls back to. For one speaker
//! we open **one Qwen realtime session per distinct target language** in the room
//! (deduped, capped by a process-wide semaphore), resample the speaker's 24 kHz PCM16
//! down to the 16 kHz Qwen wants, fan it to every session, and map each session's
//! transcript/audio back to room subtitles + translated audio.
//!
//! This shares the shape of the premium coordinators ([`super::premium`] / [`super::pro`]:
//! capacity reservation, per-language reconnect loop, live target reconcile, the
//! engine-agnostic `SessionReader` routing) driving a different upstream — by design
//! (spec 0093) adding an engine reuses this machinery. The Qwen-specific protocol lives
//! in [`super::qwen`]. Captions segment on `response.done`, with an idle debounce as a
//! fallback.
//!
//! Two deltas from the premium tiers, both consequences of Standard being the
//! **default and capacity-fallback** engine:
//!
//! * It NEVER returns [`SessionOutcome::AtCapacity`]. There is nothing below Standard to
//!   fall back to, so when the semaphore is exhausted it starts the languages it *can*
//!   get permits for and picks up the rest on the next reconcile tick. Translation
//!   degrades; it never stops.
//! * `speaker_lang == "auto"` needs no detection pass. The model identifies the spoken
//!   language itself, so — as on the premium tiers — `"auto"` simply never matches a
//!   target code and the speaker is translated into every language in the room. The old
//!   Deepgram REST probe (spec 0012) is gone with the Deepgram pipeline. (`"auto"` is
//!   also what makes [`super::qwen::session_update_json`] OMIT the source-language field,
//!   whose server-side default would otherwise assume English.)

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

use crate::config::QwenConfig;
use crate::deepgram::SpeakerCtx;
use crate::protocol::ServerMessage;
use crate::rooms::RoomManager;
use crate::transcripts::{EventKind, TranscriptEvent, TranscriptService};

use super::metadata::{EngineCapabilities, EngineMetadata};
use super::qwen::{self, QwenEvent, QwenSink, QwenSource};
use super::{reconcile_langs, SessionDeps, SessionOutcome, TranslationEngine, STANDARD_ID};

/// Capacity of the speaker's bounded audio channel (mirrors the other engines).
const AUDIO_CHANNEL_CAP: usize = 256;

/// Per-session audio buffer. Each language session drains its own copy; if one stalls
/// (slow/reconnecting), its buffer fills and we drop its chunks rather than
/// back-pressuring the other sessions or the speaker.
const PER_SESSION_AUDIO_CAP: usize = 128;

/// Reconnect backoff bounds (ms) and the cap on *consecutive* failed re-opens before a
/// language session gives up (a persistent failure, e.g. a bad key).
const RECONNECT_BASE_MS: u64 = 500;
const RECONNECT_MAX_MS: u64 = 8000;
const MAX_OPEN_FAILURES: u32 = 6;

/// How often a live session re-checks the room's target languages. Targets are fixed at
/// the first `Start`, so a peer who joins (or whose `auto` language resolves) *after* the
/// speaker began talking would never be translated for — a new language is added/removed
/// within this interval instead. This tick is also what recovers languages that were
/// skipped at start because the engine was momentarily at capacity.
const RECONCILE_MS: u64 = 1000;

/// Why a single Qwen connection ended.
enum ConnOutcome {
    /// The speaker stopped (audio channel closed) — the session is done.
    AudioClosed,
    /// The connection dropped — reconnect with backoff.
    Dropped,
}

/// Qwen realtime translation, behind the engine trait.
pub struct StandardEngine {
    meta: EngineMetadata,
    config: QwenConfig,
    /// Process-wide cap on concurrent Qwen sessions (group-room backpressure). Unlike
    /// the premium engines, exhausting this degrades the language set rather than
    /// rejecting the session — see the module docs.
    sessions: Arc<Semaphore>,
}

impl StandardEngine {
    /// Build the Standard engine from the Qwen credentials. Cost/markup come from
    /// [`QwenConfig`] (env-tunable), not the billing pricing block: Standard now bills
    /// per target-language session like the other speech-to-speech tiers.
    pub fn new(config: &QwenConfig) -> Self {
        // Output (and input) languages from the shared map (spec 0102).
        let langs: Vec<String> = super::langmap::tier_output_langs("standard");
        let meta = EngineMetadata {
            id: STANDARD_ID.to_string(),
            display_name: "Standard".to_string(),
            tier: "standard".to_string(),
            description: "Natural speech-to-speech translation by Qwen LiveTranslate — \
                          auto-detects the spoken language and returns a translated \
                          voice with matching subtitles."
                .to_string(),
            cost_per_minute: config.cost_per_minute,
            markup: config.markup,
            input_languages: langs.clone(),
            output_languages: langs,
            capabilities: EngineCapabilities {
                // Standard is speech-to-speech now: the browser plays translated audio
                // from the server instead of synthesizing it with SpeechSynthesis.
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
impl TranslationEngine for StandardEngine {
    fn metadata(&self) -> &EngineMetadata {
        &self.meta
    }

    async fn start_session(&self, ctx: SpeakerCtx, deps: SessionDeps) -> SessionOutcome {
        // INITIAL target languages = distinct OTHER languages in the room, excluding the
        // speaker's own (Qwen auto-detects the source; translating into it is a no-op).
        // `run_session` then reconciles these live against room membership.
        // Listener-pays (spec 0099): a paid session is opened for a language only when a
        // listener of that language chose THIS engine; otherwise every distinct other
        // language (legacy speaker-pays).
        let mut targets = if deps.listener_pays {
            deps.rooms
                .target_langs_for_engine(&ctx.room, &ctx.speaker_id, STANDARD_ID)
        } else {
            deps.rooms.get_room_languages(&ctx.room, &ctx.speaker_id)
        };
        targets.retain(|l| l != &ctx.speaker_lang);
        tracing::info!(
            speaker = %ctx.speaker_id,
            source = %ctx.speaker_lang,
            ?targets,
            "standard: start_session"
        );

        let (audio_tx, audio_rx) = mpsc::channel::<Vec<u8>>(AUDIO_CHANNEL_CAP);

        // Reserve one upstream-session permit per INITIAL target language — BEST EFFORT,
        // never all-or-nothing. Standard is the fallback of last resort (spec 0094): a
        // language we can't get a permit for right now is simply skipped and retried by
        // the reconcile tick, so a busy server still translates what it can instead of
        // dropping the speaker into silence.
        let wanted = targets.len();
        let mut initial = Vec::with_capacity(wanted);
        for lang in targets {
            match self.sessions.clone().try_acquire_owned() {
                Ok(permit) => initial.push((lang, permit)),
                Err(_) => break,
            }
        }
        if initial.len() < wanted {
            tracing::warn!(
                speaker = %ctx.speaker_id,
                got = initial.len(),
                wanted,
                "standard: at capacity — starting a partial language set, rest on reconcile"
            );
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

/// Coordinator for one speaker: keep one reconnecting task per *currently-present* target
/// language, resample the captured 24 kHz audio to 16 kHz once, and fan it to all of them
/// — while **reconciling** the language set against live room membership so a late joiner
/// (or a now-resolved `auto` language) starts being translated for without restarting the
/// existing sessions. `initial` carries the targets that got a permit at `Start`; it may
/// be empty or shorter than the room's language set.
async fn run_session(
    config: QwenConfig,
    ctx: SpeakerCtx,
    deps: SessionDeps,
    sessions: Arc<Semaphore>,
    initial: Vec<(String, OwnedSemaphorePermit)>,
    mut audio_rx: mpsc::Receiver<Vec<u8>>,
) {
    // lang → its audio feed. A language session self-exits (flush, close the Qwen stream,
    // release its permit) when its feed sender is dropped, so removing a language is just
    // a map removal. `primary` names the one session that echoes the speaker's own words
    // back to them, so the echo isn't duplicated across languages.
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
                    // Resample to 16 kHz ONCE (every session needs the same rate), then fan
                    // out. `try_send` so one stalled/reconnecting session never blocks the
                    // others (its buffer just drops). 100 ms @ 24 kHz → exactly 1600 samples
                    // @ 16 kHz, so chunk boundaries stay sample-aligned.
                    let chunk16 = super::gemini::resample_pcm16_mono(
                        &chunk,
                        qwen::CAPTURE_HZ,
                        qwen::QWEN_INPUT_HZ,
                    );
                    for feed in active.values() {
                        let _ = feed.try_send(chunk16.clone());
                    }
                }
                // Speaker stopped: dropping `active` closes every feed → each task flushes,
                // ends its Qwen stream, releases its permit, and exits.
                None => return,
            },
            _ = reconcile.tick() => {
                let mut want = if deps.listener_pays {
                    deps.rooms
                        .target_langs_for_engine(&ctx.room, &ctx.speaker_id, STANDARD_ID)
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
                // Add newly-present languages (and retry any skipped at start),
                // best-effort: if the engine is at capacity right now, stop and retry next
                // tick when a permit frees (never tear down the languages already streaming).
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

/// Spawn the reconnecting upstream task for one target `lang`, register its audio feed in
/// `active`, and make it `primary` (the session that echoes the speaker's own words) when
/// no active session currently is.
/// A copy of `config` with the caller's segmentation override applied, or the original
/// when there is none.
///
/// Kept as a pure function so the override is testable without opening a socket, and so
/// there is exactly ONE place where "which numbers does this session use" is decided.
fn apply_segmentation(
    config: &QwenConfig,
    over: Option<crate::deepgram::Segmentation>,
) -> QwenConfig {
    let Some(over) = over else {
        return config.clone();
    };
    QwenConfig {
        silence_duration_ms: over.silence_duration_ms,
        segment_idle_ms: over.segment_idle_ms,
        ..config.clone()
    }
}

fn spawn_lang_session(
    config: &QwenConfig,
    deps: &SessionDeps,
    ctx: &SpeakerCtx,
    lang: String,
    permit: OwnedSemaphorePermit,
    active: &mut HashMap<String, mpsc::Sender<Vec<u8>>>,
    primary: &mut Option<String>,
) {
    let is_primary = primary.is_none();
    // Apply the caller's segmentation override once, here, so everything downstream —
    // the idle timer AND the provider's own VAD in `session_update_json` — sees one
    // consistent pair of numbers instead of each reaching for the global default.
    let config = &apply_segmentation(config, ctx.segmentation);
    let (feed_tx, feed_rx) = mpsc::channel::<Vec<u8>>(PER_SESSION_AUDIO_CAP);
    let reader = SessionReader {
        lang: lang.clone(),
        is_primary,
        segment_idle_ms: config.segment_idle_ms,
        dialect: qwen::QwenDialect::from_model(&config.model),
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

/// One target language: keep a Qwen session alive across transient drops, reconnecting
/// with capped exponential backoff. Exits when the speaker stops or after too many
/// consecutive failed re-opens.
async fn session_task(
    config: QwenConfig,
    reader: SessionReader,
    mut feed_rx: mpsc::Receiver<Vec<u8>>,
    _permit: OwnedSemaphorePermit,
) {
    let mut failures: u32 = 0;
    loop {
        let connect_started = Instant::now();
        match qwen::open_session(&config, &reader.source_lang, &reader.lang).await {
            Ok((sink, source)) => {
                failures = 0; // a successful connect resets the failure budget
                              // latency: the WS connect + session.update cost. Audio buffered during
                              // this window waits, so it lands on the FIRST utterance's critical path.
                let connect_ms = connect_started.elapsed().as_millis() as u64;
                crate::metrics::record_qwen_connect(connect_ms);
                tracing::debug!(lang = %reader.lang, connect_ms, "qwen.latency: session connected");
                match run_connection(sink, source, &mut feed_rx, &reader).await {
                    ConnOutcome::AudioClosed => return,
                    ConnOutcome::Dropped => {
                        tracing::warn!(lang = %reader.lang, "qwen session dropped — reconnecting");
                    }
                }
            }
            Err(e) => {
                failures += 1;
                tracing::warn!(lang = %reader.lang, failures, "qwen open failed: {e}");
                if failures >= MAX_OPEN_FAILURES {
                    tracing::error!(lang = %reader.lang, "qwen session giving up");
                    reader.report_unavailable();
                    return;
                }
            }
        }
        // Backoff before the next attempt. A speaker who stopped mid-backoff is detected
        // on the next connection (its first recv yields a closed channel), or right here
        // when the feed channel has closed.
        let backoff = (RECONNECT_BASE_MS << failures.min(4)).min(RECONNECT_MAX_MS);
        sleep(Duration::from_millis(backoff)).await;
        if feed_rx.is_closed() {
            return;
        }
    }
}

/// Drive one live Qwen connection: forward (already-16 kHz) audio, map transcript/audio
/// events to subtitles, and segment captions on `response.done` (idle debounce as a
/// fallback). Returns how it ended so the caller can reconnect or finish.
async fn run_connection(
    mut sink: QwenSink,
    mut source: QwenSource,
    feed_rx: &mut mpsc::Receiver<Vec<u8>>,
    reader: &SessionReader,
) -> ConnOutcome {
    let mut original = String::new(); // speaker's words (input transcript)
    let mut translated = String::new(); // this session's output language
    let mut dirty = false;
    let mut audio_seq: u64 = 0; // orders translated-audio chunks for the client
                                // latency: per-segment time-to-first-audio. `seg_first_in` marks when this
                                // segment's first audio chunk was forwarded; on the first translated audio back
                                // we record the gap (the model's ear-voice span) once, then reset both at the
                                // next turn boundary so every segment is measured.
    let mut seg_first_in: Option<Instant> = None;
    let mut seg_ttfa_logged = false;
    let idle = sleep(Duration::from_millis(reader.segment_idle_ms));
    tokio::pin!(idle);

    loop {
        tokio::select! {
            chunk = feed_rx.recv() => match chunk {
                Some(c) => {
                    if seg_first_in.is_none() {
                        seg_first_in = Some(Instant::now());
                    }
                    let _ = sink.send(Message::text(qwen::audio_append_json(&c))).await;
                }
                None => {
                    // Speaker stopped. On the omni family, commit the buffered tail and ask
                    // for its response explicitly — server VAD would otherwise wait for a
                    // silence that never arrives on a closed stream, losing the last
                    // utterance. The livetranslate family owns its own turn boundaries and
                    // REJECTS both frames, so sending them there only logs errors.
                    if reader.dialect.accepts_manual_turn_control() {
                        let _ = sink.send(Message::text(qwen::audio_commit_json())).await;
                        let _ = sink.send(Message::text(qwen::response_create_json())).await;
                    }
                    if dirty {
                        reader.flush_final(&original, &translated);
                    }
                    let _ = sink.close().await;
                    return ConnOutcome::AudioClosed;
                }
            },
            msg = source.next() => {
                // Qwen sends JSON as Text frames, but tolerate Binary UTF-8 too — a
                // gateway that switches framing must not silently mute the tier.
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
                        tracing::warn!("qwen stream error: {e}");
                        if dirty { reader.flush_final(&original, &translated); }
                        return ConnOutcome::Dropped;
                    }
                };
                for event in qwen::parse_server_message(&text) {
                    match event {
                        QwenEvent::SessionReady => {
                            tracing::debug!(lang = %reader.lang, "qwen: session ready");
                        }
                        QwenEvent::InputTranscript(u) => {
                            // Delta appends, Snapshot replaces — conflating them repeats
                            // every caption (`"CiaoCiao aCiao a tutti…"`).
                            u.apply(&mut original);
                            dirty = true;
                            if reader.is_primary {
                                reader.emit_interim_to_speaker(&original);
                                reader.emit_interim_to_source_listeners(&original);
                            }
                            idle.as_mut().reset(Instant::now() + Duration::from_millis(reader.segment_idle_ms));
                        }
                        // Ignored on purpose: it repeats the words the deltas above already
                        // appended to `original`, so consuming it too would double every
                        // caption. This path segments on `TurnComplete` instead.
                        QwenEvent::InputTranscriptDone(_) => {}
                        QwenEvent::OutputTranscript(u) => {
                            u.apply(&mut translated);
                            dirty = true;
                            reader.emit_interim_to_lang(&translated, &original);
                            idle.as_mut().reset(Instant::now() + Duration::from_millis(reader.segment_idle_ms));
                        }
                        QwenEvent::OutputAudio(pcm) => {
                            if !seg_ttfa_logged {
                                if let Some(t0) = seg_first_in {
                                    // Per-segment ear-voice span → the
                                    // `voxtranslate_qwen_ttfa_ms` histogram (/metrics),
                                    // not a per-segment log line (too chatty at scale).
                                    let ttfa_ms = t0.elapsed().as_millis() as u64;
                                    crate::metrics::record_qwen_ttfa(ttfa_ms);
                                    tracing::debug!(lang = %reader.lang, ttfa_ms, "qwen.latency: first translated audio (ear-voice span)");
                                    seg_ttfa_logged = true;
                                }
                            }
                            reader.emit_audio(audio_seq, &pcm);
                            audio_seq += 1;
                            // A segment that is still SPEAKING is not idle. Text finishes
                            // long before the audio describing it, so without this the
                            // gap timer fired mid-sentence and closed the segment while
                            // seconds of its own translation were still streaming. In
                            // Talk to Anyone that final also cleared the direction, and
                            // the rest of the sentence was never spoken at all.
                            idle.as_mut().reset(Instant::now() + Duration::from_millis(reader.segment_idle_ms));
                        }
                        // An explicit turn boundary: finalize the segment now.
                        QwenEvent::TurnComplete => {
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
                        // Errors are logged; if they're fatal the socket closes next and we
                        // reconnect via the Close/None arm above.
                        QwenEvent::Error(e) => {
                            tracing::warn!(lang = %reader.lang, "qwen session error: {e}");
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
/// Routing is engine-agnostic (the same broadcast surface the premium engines use).
struct SessionReader {
    lang: String,
    is_primary: bool,
    /// Idle gap (ms) that closes a caption segment — `QwenConfig::segment_idle_ms`,
    /// shared with the webinar ingest so both Qwen surfaces draw the boundary alike.
    segment_idle_ms: u64,
    /// Which wire dialect this session's model speaks — decides whether the teardown may
    /// send `input_audio_buffer.commit` / `response.create`.
    dialect: qwen::QwenDialect,
    rooms: Arc<RoomManager>,
    transcripts: Option<TranscriptService>,
    room: String,
    speaker_id: String,
    speaker_name: String,
    source_lang: String,
    session_id: Uuid,
    speaker_user_id: Option<Uuid>,
    /// Listener-pays (spec 0099): deliver this output only to the listeners who chose
    /// Standard, not every listener of the language.
    listener_pays: bool,
}

impl SessionReader {
    /// Deliver one output frame to this language's listeners. In listener-pays mode
    /// that's the `(lang, Standard)` subset; otherwise every listener of the language.
    fn deliver(&self, message: &str) {
        if self.listener_pays {
            self.rooms
                .broadcast_to_lang_engine(&self.room, &self.lang, STANDARD_ID, message);
        } else {
            self.rooms
                .broadcast_to_lang(&self.room, &self.lang, message);
        }
    }

    /// Tell the SPEAKER their base tier is down. Standard has no lower tier to fall back
    /// to, so a language session that exhausts its retry budget would otherwise fail
    /// silently — the speaker would talk on with no captions and no explanation.
    fn report_unavailable(&self) {
        self.rooms.relay_to_peer(
            &self.room,
            &self.speaker_id,
            &ServerMessage::Error {
                message: "speech service unavailable".to_string(),
                code: None,
            }
            .to_json(),
        );
    }

    /// Live translated caption to the listeners of this session's language.
    fn emit_interim_to_lang(&self, translated: &str, original: &str) {
        self.deliver(
            &ServerMessage::SubtitleInterim {
                speaker_id: self.speaker_id.clone(),
                speaker_name: self.speaker_name.clone(),
                text: translated.to_string(),
                lang: self.source_lang.clone(),
                // `text` is a translation here, so the ORIGINAL rides along and a
                // dual-language client can stream both lines instead of waiting a
                // whole idle gap for the final.
                original: (!original.trim().is_empty()).then(|| original.trim().to_string()),
            }
            .to_json(),
        );
    }

    /// Forward one translated-audio chunk (PCM16 @ 24 kHz) to the listeners of this
    /// language. The PCM was base64-decoded on parse (validating it); re-encode for the
    /// JSON frame the client plays via its AudioWorklet.
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
                original: None,
            }
            .to_json(),
        );
    }

    /// Deliver an ORIGINAL-language caption to LISTENERS whose output language is the
    /// speaker's source language. No per-language session serves them (source→source is
    /// not translated), so without this they'd see nothing. The speaker is excluded (they
    /// get their own via `emit_interim_to_speaker`); in listener-pays only same-language
    /// Standard listeners are targeted.
    fn deliver_source(&self, message: &str) {
        if self.listener_pays {
            self.rooms.broadcast_to_lang_engine_except(
                &self.room,
                &self.source_lang,
                STANDARD_ID,
                &self.speaker_id,
                message,
            );
        } else {
            self.rooms.broadcast_to_lang_except(
                &self.room,
                &self.source_lang,
                &self.speaker_id,
                message,
            );
        }
    }

    /// Live original caption to same-language listeners (see [`deliver_source`]).
    fn emit_interim_to_source_listeners(&self, original: &str) {
        self.deliver_source(
            &ServerMessage::SubtitleInterim {
                speaker_id: self.speaker_id.clone(),
                speaker_name: self.speaker_name.clone(),
                text: original.to_string(),
                lang: self.source_lang.clone(),
                original: None,
            }
            .to_json(),
        );
    }

    /// Final original caption to same-language listeners: a `subtitle_final` whose
    /// translations map carries the SOURCE language = the original text, so those
    /// listeners resolve `translations[my_lang]` to it. Does NOT record — the per-target
    /// `flush_final` already persists the segment once.
    fn flush_final_to_source_listeners(&self, original: &str) {
        let original = original.trim();
        if original.is_empty() {
            return;
        }
        let mut translations = HashMap::new();
        translations.insert(self.source_lang.clone(), original.to_string());
        self.deliver_source(
            &ServerMessage::SubtitleFinal {
                speaker_id: self.speaker_id.clone(),
                speaker_name: self.speaker_name.clone(),
                original: original.to_string(),
                lang: self.source_lang.clone(),
                translations,
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
        // The primary session also captions same-language listeners in the original.
        if self.is_primary {
            self.flush_final_to_source_listeners(original);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::moderation::Moderator;
    use crate::rooms::{Peer, PeerTx, RoomManager, Visibility};

    fn cfg(max_sessions: usize) -> QwenConfig {
        QwenConfig {
            api_key: "k".into(),
            model: "qwen3.5-omni-flash-realtime".into(),
            asr_model: "qwen3-asr-flash-realtime".into(),
            endpoint: "wss://dashscope-intl.aliyuncs.com/api-ws/v1/realtime".into(),
            fallback: None,
            workspace_id: None,
            voice: None,
            turn_detection: "semantic_vad".into(),
            silence_duration_ms: 500,
            segment_idle_ms: 900,
            cost_per_minute: 0.0036,
            markup: 0.25,
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
            user_id: None,
            engine: STANDARD_ID.to_string(),
            avatar_url: None,
            cartesia_voice_id: None,
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
            translator: crate::translator::Translator::new(crate::groq::Groq::new(
                "k".into(),
                "openai/gpt-oss-20b".into(),
            )),
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
            segmentation: None,
        }
    }

    #[test]
    fn metadata_labels_qwen_as_standard_speech_to_speech() {
        let engine = StandardEngine::new(&cfg(4));
        let m = engine.metadata();
        assert_eq!(m.id, STANDARD_ID); // stable id unchanged — billing/analytics stay valid
        assert_eq!(m.display_name, "Standard");
        assert_eq!(m.tier, "standard");
        // The tier is speech-to-speech now: the browser no longer synthesizes locally.
        assert!(m.capabilities.translated_audio);
        assert!(m.capabilities.cost_scales_per_language);
        assert!(!m.capabilities.client_direct);
        // User rate = cost × (1 + markup) = 0.0036 × 1.25.
        assert!((m.user_rate_per_minute() - 0.0045).abs() < 1e-9);
    }

    #[test]
    fn standard_stays_cheaper_than_the_premium_tiers() {
        // The whole point of the swap: the base tier must remain the cheapest per
        // language-minute. Guards against someone bumping QWEN_COST_PER_MINUTE past the
        // tiers that are supposed to sit above it.
        let standard = StandardEngine::new(&cfg(4));
        let gemini = crate::engine::PremiumEngine::new(&crate::config::GeminiConfig {
            api_key: "k".into(),
            model: "m".into(),
            cost_per_minute: 0.023,
            markup: 0.5,
            max_sessions: 4,
            voice: None,
        });
        assert!(
            standard.metadata().user_rate_per_minute() < gemini.metadata().user_rate_per_minute()
        );
    }

    #[tokio::test]
    async fn at_capacity_still_starts_never_rejects() {
        // Standard is the capacity-fallback engine: with every permit taken it must STILL
        // return Started (degraded to zero upstream languages, recovered by reconcile)
        // rather than AtCapacity, which would leave the caller with nothing to fall back to.
        let engine = StandardEngine::new(&cfg(1));
        let _held = engine.sessions.clone().acquire_owned().await.unwrap();

        let rm = RoomManager::new();
        join_peer(&rm, "r", "spk", "it");
        join_peer(&rm, "r", "lis", "en"); // one distinct target language

        let out = engine
            .start_session(speaker_ctx("r", "spk", "it"), deps(rm))
            .await;
        assert!(matches!(out, SessionOutcome::Started(_)));
    }

    #[tokio::test]
    async fn no_targets_returns_started() {
        // Speaker alone → nothing to translate → Started (no permit, no upstream session,
        // no cost), so the call flows normally.
        let engine = StandardEngine::new(&cfg(4));
        let rm = RoomManager::new();
        join_peer(&rm, "r", "spk", "it");

        let out = engine
            .start_session(speaker_ctx("r", "spk", "it"), deps(rm))
            .await;
        assert!(matches!(out, SessionOutcome::Started(_)));
    }

    #[tokio::test]
    async fn partial_capacity_starts_the_languages_it_can() {
        // Two target languages, one permit: the session starts and translates ONE
        // language immediately instead of failing the whole speaker.
        let engine = StandardEngine::new(&cfg(1));
        let rm = RoomManager::new();
        join_peer(&rm, "r", "spk", "it");
        join_peer(&rm, "r", "a", "en");
        join_peer(&rm, "r", "b", "fr");

        let out = engine
            .start_session(speaker_ctx("r", "spk", "it"), deps(rm))
            .await;
        assert!(matches!(out, SessionOutcome::Started(_)));
        // The one permit was taken by the session that did start.
        assert_eq!(engine.sessions.available_permits(), 0);
    }
}
