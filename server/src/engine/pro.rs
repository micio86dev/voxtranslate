//! The **Pro** engine: OpenAI GPT-Realtime-Translate (spec 0093).
//!
//! End-to-end speech-to-speech (the "Pro" tier). For one speaker we open **one OpenAI
//! realtime session per distinct target language** in the room (deduped, capped by a
//! process-wide semaphore), stream the speaker's PCM16 audio to all of them, and
//! map each session's transcript deltas to room subtitles. Because OpenAI emits
//! append-only deltas with no segment boundary, captions are segmented by an idle
//! debounce. Translated **audio** deltas are forwarded to listeners. Its stable
//! persisted id is the literal `"premium"` ([`super::OPENAI_ID`]) — a historical
//! value from before the Pro/Premium label swap, kept so billing data stays valid.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use futures::{SinkExt as _, StreamExt as _};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio::time::{interval, sleep, Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use crate::config::OpenAiConfig;
use crate::deepgram::SpeakerCtx;
use crate::protocol::ServerMessage;
use crate::rooms::RoomManager;
use crate::transcripts::{EventKind, TranscriptEvent, TranscriptService};
use crate::translator::Translator;

use super::metadata::{EngineCapabilities, EngineMetadata};
use super::openai::{self, OaSink, OaSource, OpenAiEvent};
use super::{reconcile_langs, SessionDeps, SessionOutcome, TranslationEngine, OPENAI_ID};

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

/// How often a live session re-checks the room's target languages. Targets are fixed
/// at the first `Start`, so a peer who joins (or whose `auto` language resolves)
/// *after* the speaker began talking would never be translated for — a new language is
/// added/removed within this interval instead. Cheap (a room-map read per active
/// speaker); 1 s keeps a new joiner's wait short.
const RECONCILE_MS: u64 = 1000;

/// Why a single OpenAI connection ended.
enum ConnOutcome {
    /// The speaker stopped (audio channel closed) — the session is done.
    AudioClosed,
    /// The connection dropped unexpectedly — reconnect with backoff.
    Dropped,
}

/// OpenAI GPT-Realtime-Translate, behind the engine trait.
pub struct ProEngine {
    meta: EngineMetadata,
    config: OpenAiConfig,
    /// Process-wide cap on concurrent OpenAI sessions (group-room backpressure).
    sessions: Arc<Semaphore>,
}

impl ProEngine {
    pub fn new(config: &OpenAiConfig) -> Self {
        // OpenAI's 13 realtime-translate output languages, from the shared map (spec 0102).
        let langs: Vec<String> = super::langmap::tier_output_langs("pro");
        let meta = EngineMetadata {
            id: OPENAI_ID.to_string(),
            display_name: "Pro".to_string(),
            tier: "pro".to_string(),
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
impl TranslationEngine for ProEngine {
    fn metadata(&self) -> &EngineMetadata {
        &self.meta
    }

    async fn start_session(&self, ctx: SpeakerCtx, deps: SessionDeps) -> SessionOutcome {
        // INITIAL target languages = distinct OTHER languages in the room, excluding
        // the speaker's own (translating to the source language is a no-op).
        // `run_session` then reconciles these live against room membership, so a peer
        // who joins (or whose `auto` resolves) after Start is picked up too.
        // Listener-pays (spec 0099): a paid OpenAI session is opened for a language
        // only when a listener of that language chose THIS engine; otherwise every
        // distinct other language (legacy speaker-pays).
        let mut targets = if deps.listener_pays {
            deps.rooms
                .target_langs_for_engine(&ctx.room, &ctx.speaker_id, OPENAI_ID)
        } else {
            deps.rooms.get_room_languages(&ctx.room, &ctx.speaker_id)
        };
        targets.retain(|l| l != &ctx.speaker_lang);
        tracing::info!(
            speaker = %ctx.speaker_id,
            source = %ctx.speaker_lang,
            ?targets,
            "pro: start_session"
        );

        let (audio_tx, audio_rx) = mpsc::channel::<Vec<u8>>(AUDIO_CHANNEL_CAP);

        // Reserve one upstream-session permit per INITIAL target language —
        // all-or-nothing and WITHOUT blocking (spec 0094). If we can't get them all
        // right now the engine is at capacity, so the caller falls back to Standard
        // rather than queueing the speaker silently. Dropping `initial` on the early
        // return releases any we already took. Empty targets (alone / all same
        // language) reserve nothing — zero cost — but we still spawn the session so it
        // picks up later joiners by reconcile.
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

/// Coordinator for one speaker: keep one self-contained, reconnecting task per
/// *currently-present* target language and fan the speaker's captured PCM16 to all of
/// them — while **reconciling** the language set against live room membership so a late
/// joiner (or a now-resolved `auto` language) starts being translated for without
/// restarting the existing sessions. `initial` carries the targets known at `Start`
/// (each with a pre-acquired permit); it may be empty.
async fn run_session(
    config: OpenAiConfig,
    ctx: SpeakerCtx,
    deps: SessionDeps,
    sessions: Arc<Semaphore>,
    initial: Vec<(String, OwnedSemaphorePermit)>,
    mut audio_rx: mpsc::Receiver<Vec<u8>>,
) {
    // lang → its audio feed. A language session self-exits (flush, close the OpenAI
    // session, release its permit) when its feed sender is dropped, so removing a
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
                // Fan each captured PCM16 chunk to every session. `try_send` so one
                // stalled / reconnecting session never blocks the others (its buffer
                // just drops).
                Some(chunk) => {
                    for feed in active.values() {
                        let _ = feed.try_send(chunk.clone());
                    }
                }
                // Speaker stopped: dropping `active` closes every feed → each task
                // flushes, closes its OpenAI session, releases its permit, and exits.
                None => return,
            },
            _ = reconcile.tick() => {
                let mut want = if deps.listener_pays {
                    deps.rooms
                        .target_langs_for_engine(&ctx.room, &ctx.speaker_id, OPENAI_ID)
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
    config: &OpenAiConfig,
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
        translator: deps.translator.clone(),
    };
    tokio::spawn(session_task(config.clone(), reader, feed_rx, permit));
    if is_primary {
        *primary = Some(lang.clone());
    }
    active.insert(lang, feed_tx);
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
                            reader.emit_interim_to_source_listeners(&original);
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
#[derive(Clone)]
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
    /// Listener-pays (spec 0099): deliver this OpenAI output only to the listeners
    /// who chose OpenAI ("Pro"), not every listener of the language (a Standard or
    /// Gemini listener of the same language is served by their own engine).
    listener_pays: bool,
    /// Groq fallback for the subtitle TEXT when OpenAI ships an empty output
    /// transcript for a segment (see [`Self::flush_final`]).
    translator: Translator,
}

impl SessionReader {
    /// Deliver one output frame to this language's listeners. In listener-pays mode
    /// that's the `(lang, OpenAI)` subset; otherwise every listener of the language.
    fn deliver(&self, message: &str) {
        if self.listener_pays {
            self.rooms
                .broadcast_to_lang_engine(&self.room, &self.lang, OPENAI_ID, message);
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

    /// Forward one translated-audio chunk to the listeners of this language. The
    /// PCM was base64-decoded on parse (validating it); re-encode for the JSON
    /// frame the client plays via its AudioWorklet.
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

    /// Deliver an ORIGINAL-language caption to LISTENERS whose output language is the
    /// speaker's source language. No per-language session serves them (source→source is
    /// not translated), so without this they'd see nothing while the speaker uses Pro —
    /// unlike Standard, which captions everyone. The speaker is excluded (they get their
    /// own via `emit_interim_to_speaker`); in listener-pays only same-language Pro
    /// listeners are targeted, so a Standard listener's Deepgram captions aren't doubled.
    fn deliver_source(&self, message: &str) {
        if self.listener_pays {
            self.rooms.broadcast_to_lang_engine_except(
                &self.room,
                &self.source_lang,
                OPENAI_ID,
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
            }
            .to_json(),
        );
    }

    /// Final original caption to same-language listeners: a `subtitle_final` whose
    /// translations map carries the SOURCE language = the original text, so those listeners
    /// resolve `translations[my_lang]` to it. Does NOT record — the per-target `flush_final`
    /// already persists the segment once.
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

    /// Finalize a segment: record it (once) and broadcast a `subtitle_final` to
    /// the listeners of this language. The translations map carries this one
    /// language; each language's listeners get their own targeted message — so a
    /// listener never sees text that differs from the audio they hear.
    ///
    /// OpenAI's gpt-realtime-translate intermittently ships an EMPTY output
    /// transcript for a segment even though it translated the AUDIO fine, leaving the
    /// caption with no translated text — so the client falls back to the untranslated
    /// original ("Pro subtitles show the original, never the translation"). When that
    /// happens we recover the caption text off the reliable Groq path (the same one
    /// the Standard tier uses) in a spawned task, so the audio pipeline is never
    /// stalled waiting on it. If Groq also fails we deliver the empty translation, so
    /// behaviour is never worse than today.
    fn flush_final(&self, original: &str, translated: &str) {
        let original = original.trim().to_string();
        let translated = translated.trim().to_string();
        if translated.is_empty() && original.is_empty() {
            return;
        }
        // Same-language listeners are captioned in the original immediately (they hear
        // the speaker's real voice — no translation involved), regardless of whether
        // the translated text below needs recovering.
        if self.is_primary {
            self.flush_final_to_source_listeners(&original);
        }
        // Timestamp the segment now (when spoken), not after any Groq round-trip, so
        // transcript ordering matches reality.
        let ts = Utc::now();
        if !translated.is_empty() {
            self.emit_translated(&original, &translated, ts);
            return;
        }
        // Empty upstream transcript → recover the caption text via Groq without
        // blocking the audio loop. `SessionReader` is cheap to clone (Arc/String).
        let reader = self.clone();
        tokio::spawn(async move {
            let recovered = reader
                .translator
                .translate_one(&original, &reader.source_lang, &reader.lang, None)
                .await
                .map(|(text, _hit)| text)
                .unwrap_or_default();
            reader.emit_translated(&original, &recovered, ts);
        });
    }

    /// Record the segment (once) and deliver the translated `subtitle_final` to this
    /// language's listeners. `translated` may be empty (upstream AND the Groq fallback
    /// both blank) — the client then shows the original source line. Shared by the
    /// synchronous path and the empty-transcript recovery in [`Self::flush_final`].
    fn emit_translated(&self, original: &str, translated: &str, ts: DateTime<Utc>) {
        let mut translations = HashMap::new();
        translations.insert(self.lang.clone(), translated.to_string());
        if let Some(svc) = self.transcripts.as_ref() {
            svc.record(TranscriptEvent {
                session_id: self.session_id,
                kind: EventKind::Speech,
                speaker_peer_id: self.speaker_id.clone(),
                speaker_user_id: self.speaker_user_id,
                speaker_name: self.speaker_name.clone(),
                original_text: original.to_string(),
                original_lang: self.source_lang.clone(),
                translations: translations.clone(),
                ts,
            });
        }
        self.deliver(
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::moderation::Moderator;
    use crate::rooms::{Peer, PeerTx, RoomManager, Visibility};

    fn cfg(max_sessions: usize) -> OpenAiConfig {
        OpenAiConfig {
            api_key: "k".into(),
            model: "gpt-realtime-translate".into(),
            cost_per_minute: 0.04,
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
            user_id: None,
            engine: "standard".to_string(),
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
            pcm_input: false,
            translator: Translator::new(crate::groq::Groq::new(
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
        }
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

    /// Join a peer and keep its receiver so the test can assert what it was sent.
    fn join_peer_rx(
        rm: &RoomManager,
        room: &str,
        id: &str,
        lang: &str,
    ) -> tokio::sync::mpsc::Receiver<String> {
        let (tx, rx, _ovf) = PeerTx::channel(8);
        let peer = Peer {
            id: id.into(),
            conn: Uuid::new_v4(),
            name: id.into(),
            lang: lang.into(),
            user_id: None,
            engine: "standard".to_string(),
            avatar_url: None,
            cartesia_voice_id: None,
            tx,
            speaking: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        rm.join(room, peer, Visibility::Private).unwrap();
        rx
    }

    /// A primary reader for speaker `spk` (source `it`) translating into `target_lang`,
    /// in speaker-pays mode (delivers to every listener of the language).
    fn reader(rm: Arc<RoomManager>, target_lang: &str) -> SessionReader {
        SessionReader {
            lang: target_lang.into(),
            is_primary: true,
            rooms: rm,
            transcripts: None,
            room: "r".into(),
            speaker_id: "spk".into(),
            speaker_name: "spk".into(),
            source_lang: "it".into(),
            session_id: Uuid::new_v4(),
            speaker_user_id: None,
            listener_pays: false,
            translator: Translator::new(crate::groq::Groq::new(
                "k".into(),
                "openai/gpt-oss-20b".into(),
            )),
        }
    }

    #[tokio::test]
    async fn flush_final_delivers_translation_to_foreign_listener() {
        // Healthy path: OpenAI supplied the translated transcript, so a foreign listener
        // receives a `subtitle_final` whose translations map carries THEIR language — the
        // caption they read is the translation, not the untranslated original.
        let rm = Arc::new(RoomManager::new());
        join_peer(&rm, "r", "spk", "it");
        let mut r_lis = join_peer_rx(&rm, "r", "lis", "en");

        reader(rm.clone(), "en").flush_final("ciao a tutti", "hello everyone");

        let msg = r_lis.try_recv().expect("foreign listener captioned");
        assert!(
            msg.contains(r#""type":"subtitle_final""#),
            "is a final: {msg}"
        );
        assert!(
            msg.contains("hello everyone"),
            "carries the translation, not just the original: {msg}"
        );
    }

    #[tokio::test]
    async fn flush_final_ignores_a_fully_empty_segment() {
        // Nothing spoken AND nothing translated → no caption at all (never a blank frame).
        let rm = Arc::new(RoomManager::new());
        join_peer(&rm, "r", "spk", "it");
        let mut r_lis = join_peer_rx(&rm, "r", "lis", "en");

        reader(rm.clone(), "en").flush_final("   ", "  ");

        assert!(r_lis.try_recv().is_err(), "empty segment delivers nothing");
    }
}
