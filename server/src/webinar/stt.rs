//! Webinar realtime subtitles — the host's server-side STT ingest (SPEC "Webinar
//! Mode" Fase 2).
//!
//! In a webinar the broadcast media travels host → WHIP → LL-HLS, which the server
//! never touches. For subtitles the host client opens a SECOND, audio-only path to
//! this WebSocket (mirroring the P2P call's capture → server bridge): it streams
//! PCM16 chunks here, the server runs Qwen-Omni Realtime STT on them, translates each
//! finalized utterance into every viewer language currently present, and pushes
//! subtitle frames to viewers over the EXISTING presence WS
//! ([`crate::webinar::presence::broadcast_subtitle`]).
//!
//! # Why transcribe-only, not the Standard tier's speech-to-speech shape
//!
//! A call has ≤4 peers, so the Standard engine can afford one Qwen session per target
//! language. A webinar fans ONE host out to arbitrarily many viewer languages and
//! renders **text subtitles only** — so it opens a single
//! [`crate::engine::qwen::open_transcribe_session`] and translates the finals with Groq.
//! Ten viewer languages cost one upstream session here, not ten.
//!
//! Translated TTS audio is out of scope **for this module**, not for webinars. The
//! Enhanced tier speaks the translation through a separate path: the viewer's browser
//! mints a TTS-only Cartesia token from `GET /api/w/{code}/tts-session`
//! ([`crate::webinar::routes::tts_session`]) and synthesises client-side, in the host's
//! cloned voice where one is set. This file only ever produces subtitle frames.
//!
//! The line this replaces said webinars were "subtitles only", full stop. That was true
//! when it was written and stopped being true when the Enhanced TTS path shipped — it
//! then spent months telling every reader of this file the wrong thing about the product.
//!
//! # WebSocket: `GET /api/webinars/{id}/stt?token=<JWT>`
//!
//! Auth mirrors the presence WS: browsers can't set WebSocket request headers, so
//! the host's session **JWT** rides the `token` query parameter (NOT an
//! `Authorization: Bearer` header). The token is verified and the user must be a
//! MEMBER of the webinar's org (cross-tenant → 404, exactly like the REST host
//! API), and the webinar must be `live` or `scheduled`. Guests / missing / invalid
//! tokens are rejected before the upgrade (401), a non-member is 404, and a wrong
//! lifecycle state is 409.
//!
//! # Control protocol (what the host client sends)
//!
//! The host sends **binary WebSocket frames**, each a PCM16 mono chunk at
//! [`crate::engine::qwen::CAPTURE_HZ`] (24 kHz, 100 ms — the same capture the Standard
//! tier produces; the server resamples to the 16 kHz Qwen wants). There are NO
//! `start`/`stop` JSON control frames — the STREAM ITSELF is the signal: the first
//! binary chunk starts STT, and CLOSING the socket commits the buffered tail so the last
//! utterance is still transcribed. Inbound text frames are ignored.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde::Deserialize;
use tokio::sync::mpsc;
use uuid::Uuid;

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use tokio::time::{interval, MissedTickBehavior};

use crate::auth::verify_jwt;
use crate::business::{db_err, not_found, require_pool, require_role, MEMBER};
use crate::config::QwenConfig;
use crate::engine::gemini::resample_pcm16_mono;
use crate::engine::qwen::{self, QwenEvent, QwenSink, QwenSource};
use crate::webinar::find_by_id;
use crate::webinar::presence::{AudioEvent, SubtitleEvent};
use crate::AppState;

/// Handlers return `Result<Response, Response>` so `?` short-circuits with a
/// status code (mirrors `webinar::routes`).
#[allow(clippy::result_large_err)]
type HandlerResult = Result<Response, Response>;

/// Query params for the STT ingest upgrade: the host's session JWT.
#[derive(Deserialize)]
pub struct SttParams {
    /// The host's session JWT (browsers can't set WS headers — mirror the
    /// presence WS query-param auth). Absent → guest → 401.
    #[serde(default)]
    token: Option<String>,
}

/// Capacity of the host→Qwen audio channel. 100 ms PCM16 chunks at 24 kHz are ~4.8 KiB,
/// so 64 chunks (~6 s) absorbs a hiccup while bounding a stalled session's memory.
const AUDIO_CHANNEL_CAP: usize = 64;

/// Per-language audio buffer. Each language drains its own copy; if one stalls its buffer
/// fills and drops chunks rather than back-pressuring the others or the host.
const PER_SESSION_AUDIO_CAP: usize = 128;

/// Minimum gap between two "dropping audio" log lines from the same site. The COUNTER
/// (`voxtranslate_webinar_audio_dropped_total`) carries the volume; the log only has to
/// mark when an incident starts and that it is still going.
const DROP_LOG_EVERY: Duration = Duration::from_secs(1);

/// Rate-limits the drop log without hiding the start of an incident: the first drop after
/// a quiet period always logs, then at most one line per [`DROP_LOG_EVERY`].
#[derive(Default)]
struct DropLog {
    last: Option<std::time::Instant>,
}

impl DropLog {
    fn should_log(&mut self, now: std::time::Instant) -> bool {
        let due = self
            .last
            .is_none_or(|prev| now.duration_since(prev) >= DROP_LOG_EVERY);
        if due {
            self.last = Some(now);
        }
        due
    }
}

/// How often the language set is re-checked against who is actually watching. A viewer
/// who joins mid-talk starts being translated within this window; a language nobody is
/// left watching stops costing money within it.
const RECONCILE_MS: u64 = 1000;

/// Reconnect backoff bounds (ms) and the cap on CONSECUTIVE failed re-opens before one
/// language gives up (a persistent failure, e.g. a bad key).
const RECONNECT_BASE_MS: u64 = 500;
const RECONNECT_MAX_MS: u64 = 8000;
const MAX_OPEN_FAILURES: u32 = 6;

// The idle gap that closes a segment lives in `QwenConfig::segment_idle_ms`
// (`QWEN_SEGMENT_IDLE_MS`), shared with the Standard tier in calls so both Qwen surfaces
// draw the sentence boundary alike.
//
// It is load-bearing here, not a nicety: the realtime ASR streams
// `conversation.item.input_audio_transcription.text` snapshots and — verified against the
// live API, including with three seconds of trailing silence — NEVER sends `.completed`.
// Waiting for one leaves viewers with interims that are never translated, persisted, or
// finalised: subtitles that flicker and vanish, and a viewer TTS with nothing to speak.

/// Whether a webinar in `status` may open an STT stream. `live` is the normal
/// case; `scheduled` is allowed so the host can warm the mic just before going on
/// air. Any other state (`ended`, `cancelled`) is rejected.
fn stt_allowed(status: &str) -> bool {
    matches!(status, "live" | "scheduled")
}

/// `GET /api/webinars/{id}/stt?token=<JWT>` — host STT ingest (WebSocket).
///
/// Authenticates + authorizes BEFORE the upgrade so a guest/non-member/bad-state
/// gets a clean HTTP status, never a half-open socket.
#[allow(clippy::result_large_err)] // the Err IS the handler's HTTP response
pub async fn stt_ws(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(params): Query<SttParams>,
) -> HandlerResult {
    let pool = require_pool(&state)?;
    let billing = state.config.billing.as_ref().ok_or_else(unauthorized)?;

    // Query-param JWT (browsers can't set WS headers) → user id.
    let token = params.token.as_deref().ok_or_else(unauthorized)?;
    let claims = verify_jwt(&billing.jwt_secret, token).map_err(|_| unauthorized())?;
    let user_id = Uuid::parse_str(&claims.sub).map_err(|_| unauthorized())?;

    // Must be a member of the webinar's org — cross-tenant is a 404, exactly like
    // the REST host API's `require_webinar_role`.
    let w = find_by_id(pool, id)
        .await
        .map_err(db_err)?
        .ok_or_else(|| not_found("webinar not found"))?;
    require_role(pool, w.org_id, user_id, MEMBER).await?;

    if !stt_allowed(&w.status) {
        return Err((StatusCode::CONFLICT, "webinar is not live").into_response());
    }

    // Single-flight: refuse a second concurrent STT stream for this webinar so a
    // reconnect race (or an abusive/buggy client opening many sockets) can't
    // multiply Qwen+Groq cost or double up subtitles. Acquire BEFORE the
    // upgrade so the caller gets a clean 409, and hold the guard for the socket's
    // whole life (it frees the slot on drop).
    let stt_guard = state
        .webinar_presence
        .try_begin_stt(&w.code)
        .ok_or_else(|| (StatusCode::CONFLICT, "an STT stream is already active").into_response())?;

    let code = w.code.clone();
    let source_language = w.source_language.clone();
    let record = w.record_transcript;
    let webinar_id = w.id;
    Ok(ws.on_upgrade(move |socket| {
        handle_stt(
            socket,
            state,
            code,
            source_language,
            webinar_id,
            record,
            stt_guard,
        )
    }))
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
}

/// Aborts a spawned task when dropped. Dropping a bare `JoinHandle` only detaches
/// the task; this ensures a dropped STT session actually cancels its work.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// On upgrade: bridge the host's audio into Qwen and drive the subtitle fan-out.
/// Binary frames → Qwen; transcription finals → translate → broadcast.
async fn handle_stt(
    socket: WebSocket,
    state: AppState,
    code: String,
    source_language: String,
    webinar_id: Uuid,
    record_transcript: bool,
    // Held for the socket's whole life; frees the single-flight slot on drop
    // (normal close OR the whole future being dropped on shutdown).
    _stt_guard: crate::webinar::presence::SttGuard,
) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    // Audio bridge: host binary frames → the per-language coordinator.
    let (audio_tx, audio_rx) = mpsc::channel::<Vec<u8>>(AUDIO_CHANNEL_CAP);
    let session = tokio::spawn(run_webinar_session(
        state.config.qwen.clone(),
        state.clone(),
        code.clone(),
        source_language.clone(),
        webinar_id,
        record_transcript,
        audio_rx,
    ));
    // Abort-on-drop: if this future is dropped before the clean teardown below (runtime
    // shutdown, or a read loop that never returns on a half-open socket), the coordinator
    // is cancelled so its upstream sessions cannot leak and keep billing.
    let _abort = AbortOnDrop(session.abort_handle());
    // The presence WS pushes count/subtitle/audio frames; the STT socket is audio-in
    // only, so there is nothing to send back on it. Keep the sink alive to close cleanly.
    let _ = &mut ws_tx;

    let mut drop_log = DropLog::default();
    while let Some(Ok(msg)) = ws_rx.next().await {
        match msg {
            Message::Binary(chunk) => {
                // A full audio channel means the coordinator is behind; drop the chunk
                // rather than block the socket read (best-effort live captions).
                crate::metrics::record_webinar_chunk();
                if audio_tx.try_send(chunk.to_vec()).is_err() {
                    // Counted, not just logged: a dropped chunk is a hole in what Qwen
                    // hears, and it surfaces to viewers as chopped subtitles AND chopped
                    // translated speech. At `debug` this was invisible in production,
                    // which is how it stayed a mystery. `warn` is the right level — this
                    // is silent output degradation, not a routine event.
                    crate::metrics::record_webinar_drop(crate::metrics::WebinarHop::Ingest);
                    if drop_log.should_log(std::time::Instant::now()) {
                        tracing::warn!(%code, "webinar STT: ingest buffer full, dropping host audio");
                    }
                }
            }
            // No `start`/`stop` control protocol: the stream itself is the signal.
            // Text frames are ignored; a Close ends the session.
            Message::Close(_) => break,
            _ => {}
        }
    }

    // Host disconnected: dropping the channel closes every language feed, so each session
    // flushes its tail and exits.
    drop(audio_tx);
    let _ = session.await;
}

/// One Qwen session per distinct viewer language, fed from the host's single stream.
///
/// This mirrors the call path's Standard engine ([`crate::engine::standard`]) rather than
/// the transcribe-and-translate-with-Groq shape a webinar used to have. The trade is
/// deliberate and it is not small: a webinar with ten viewer languages now opens ten
/// upstream sessions instead of one, and pays accordingly. What it buys is that a webinar
/// and a call are the same product — same engine, same 29 spoken languages, same voice —
/// instead of two systems that merely look alike.
///
/// Languages are reconciled every [`RECONCILE_MS`] against who is actually watching, so a
/// viewer who joins mid-talk starts being translated for without disturbing the sessions
/// already running, and a language nobody is left watching stops costing money.
async fn run_webinar_session(
    config: QwenConfig,
    state: AppState,
    code: String,
    source_language: String,
    webinar_id: Uuid,
    record_transcript: bool,
    mut audio_rx: mpsc::Receiver<Vec<u8>>,
) {
    let mut active: HashMap<String, mpsc::Sender<Vec<u8>>> = HashMap::new();
    // The primary session is the one that persists the transcript, so an utterance is
    // stored once rather than once per language.
    let mut primary: Option<String> = None;
    let mut reconcile = interval(Duration::from_millis(RECONCILE_MS));
    reconcile.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut fanout_drop_log = DropLog::default();

    loop {
        tokio::select! {
            chunk = audio_rx.recv() => match chunk {
                Some(chunk) => {
                    // Resample to 16 kHz ONCE, then fan out. `try_send` so one stalled or
                    // reconnecting language never back-pressures the others or the host.
                    let chunk16 = resample_pcm16_mono(&chunk, qwen::CAPTURE_HZ, qwen::QWEN_INPUT_HZ);
                    for (lang, feed) in active.iter() {
                        // Was `let _ = feed.try_send(...)` — a discarded Result, so a
                        // language whose upstream stalled lost audio with no log, no
                        // counter and no trace. That silence is why chopped subtitles and
                        // chopped voice had nothing to point at.
                        if feed.try_send(chunk16.clone()).is_err() {
                            crate::metrics::record_webinar_drop(crate::metrics::WebinarHop::Fanout);
                            if fanout_drop_log.should_log(std::time::Instant::now()) {
                                tracing::warn!(%code, %lang, "webinar STT: language feed full, dropping audio for this language");
                            }
                        }
                    }
                }
                // Host stopped: dropping `active` ends every language task.
                None => return,
            },
            _ = reconcile.tick() => {
                let mut want = state.webinar_presence.target_languages(&code);
                // A viewer already watching in the host's language needs no translation.
                want.retain(|l| l != &source_language);
                let have: HashSet<String> = active.keys().cloned().collect();
                let (drop_langs, add_langs) = crate::engine::reconcile_langs(&have, &want);
                for lang in &drop_langs {
                    active.remove(lang);
                    if primary.as_ref() == Some(lang) {
                        primary = None; // re-elected on the next add
                    }
                }
                for lang in add_langs {
                    let is_primary = primary.is_none();
                    let (feed_tx, feed_rx) = mpsc::channel::<Vec<u8>>(PER_SESSION_AUDIO_CAP);
                    tokio::spawn(lang_session(
                        config.clone(),
                        state.clone(),
                        code.clone(),
                        source_language.clone(),
                        lang.clone(),
                        webinar_id,
                        record_transcript,
                        is_primary,
                        feed_rx,
                    ));
                    if is_primary {
                        primary = Some(lang.clone());
                    }
                    active.insert(lang, feed_tx);
                }
            }
        }
    }
}

/// Keep one target language translating across transient drops, with capped backoff.
#[allow(clippy::too_many_arguments)]
async fn lang_session(
    config: QwenConfig,
    state: AppState,
    code: String,
    source_language: String,
    target: String,
    webinar_id: Uuid,
    record_transcript: bool,
    is_primary: bool,
    mut feed_rx: mpsc::Receiver<Vec<u8>>,
) {
    let mut failures: u32 = 0;
    loop {
        match qwen::open_session(&config, &source_language, &target).await {
            Ok((sink, source)) => {
                failures = 0;
                let ended = run_lang_connection(
                    sink,
                    source,
                    &mut feed_rx,
                    &state,
                    &code,
                    &source_language,
                    &target,
                    webinar_id,
                    record_transcript,
                    is_primary,
                    config.segment_idle_ms,
                )
                .await;
                if ended {
                    return; // host stopped
                }
                tracing::warn!(%code, %target, "webinar: qwen session dropped — reconnecting");
            }
            Err(e) => {
                failures += 1;
                tracing::warn!(%code, %target, failures, "webinar: qwen open failed: {e}");
                if failures >= MAX_OPEN_FAILURES {
                    tracing::error!(%code, %target, "webinar: giving up on this language");
                    return;
                }
            }
        }
        let backoff = (RECONNECT_BASE_MS << failures.min(4)).min(RECONNECT_MAX_MS);
        tokio::time::sleep(Duration::from_millis(backoff)).await;
        if feed_rx.is_closed() {
            return;
        }
    }
}

/// Drive one live connection for one language. Returns `true` when the host stopped
/// (the feed closed), `false` when the connection dropped and should be retried.
// `flush!` resets the segment state, which is dead on the two paths that flush and then
// return — but correct on the loop paths, and duplicating the macro to avoid it would be
// worse than the warning it silences.
#[allow(unused_assignments)]
#[allow(clippy::too_many_arguments)]
async fn run_lang_connection(
    mut sink: QwenSink,
    mut source: QwenSource,
    feed_rx: &mut mpsc::Receiver<Vec<u8>>,
    state: &AppState,
    code: &str,
    source_language: &str,
    target: &str,
    webinar_id: Uuid,
    record_transcript: bool,
    is_primary: bool,
    segment_idle_ms: u64,
) -> bool {
    use futures::SinkExt as _;
    use tokio_tungstenite::tungstenite::Message as QwenMessage;

    let mut original = String::new();
    let mut translated = String::new();
    let mut dirty = false;
    let mut audio_seq: u64 = 0;
    let mut last_final: Option<(String, std::time::Instant)> = None;
    let idle = tokio::time::sleep(Duration::from_millis(segment_idle_ms));
    tokio::pin!(idle);

    // Closing a segment: the viewers of THIS language get the final, and only the primary
    // session writes the transcript row so an utterance is stored once.
    macro_rules! flush {
        () => {
            if dirty {
                let text = translated.trim().to_string();
                if !text.is_empty() {
                    let mut translations = HashMap::new();
                    translations.insert(target.to_string(), text.clone());
                    state.webinar_presence.broadcast_to_lang(
                        code,
                        target,
                        &SubtitleEvent::Final {
                            original: original.trim().to_string(),
                            lang: source_language.to_string(),
                            translations: translations.clone(),
                        }
                        .to_json(),
                    );
                    if is_primary {
                        finalize_record(
                            state,
                            webinar_id,
                            record_transcript,
                            &mut last_final,
                            original.trim().to_string(),
                            source_language,
                            translations,
                        );
                    }
                }
                original.clear();
                translated.clear();
                dirty = false;
            }
            idle.as_mut()
                .reset(tokio::time::Instant::now() + Duration::from_secs(3600));
        };
    }

    loop {
        tokio::select! {
            chunk = feed_rx.recv() => match chunk {
                Some(c) => {
                    let _ = sink.send(QwenMessage::text(qwen::audio_append_json(&c))).await;
                }
                None => {
                    flush!();
                    let _ = sink.close().await;
                    return true;
                }
            },
            msg = source.next() => {
                let raw = match msg {
                    Some(Ok(QwenMessage::Text(t))) => t.to_string(),
                    Some(Ok(QwenMessage::Binary(b))) => match String::from_utf8(b.to_vec()) {
                        Ok(s) => s,
                        Err(_) => continue,
                    },
                    Some(Ok(QwenMessage::Close(_))) | None => { flush!(); return false }
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => {
                        tracing::warn!(%code, %target, "webinar: qwen stream error: {e}");
                        flush!();
                        return false;
                    }
                };
                for event in qwen::parse_server_message(&raw) {
                    match event {
                        QwenEvent::InputTranscript(u) => {
                            u.apply(&mut original);
                            dirty = true;
                            idle.as_mut().reset(
                                tokio::time::Instant::now() + Duration::from_millis(segment_idle_ms),
                            );
                        }
                        QwenEvent::OutputTranscript(u) => {
                            u.apply(&mut translated);
                            dirty = true;
                            // A live partial, already in the viewer's language.
                            state.webinar_presence.broadcast_to_lang(
                                code,
                                target,
                                &SubtitleEvent::Interim {
                                    text: translated.trim().to_string(),
                                    lang: source_language.to_string(),
                                }
                                .to_json(),
                            );
                            idle.as_mut().reset(
                                tokio::time::Instant::now() + Duration::from_millis(segment_idle_ms),
                            );
                        }
                        QwenEvent::OutputAudio(pcm) => {
                            state.webinar_presence.broadcast_to_lang(
                                code,
                                target,
                                &AudioEvent {
                                    lang: target.to_string(),
                                    seq: audio_seq,
                                    pcm16_b64: BASE64.encode(&pcm),
                                }
                                .to_json(),
                            );
                            audio_seq += 1;
                        }
                        QwenEvent::TurnComplete => { flush!(); }
                        QwenEvent::Error(e) => {
                            tracing::warn!(%code, %target, "webinar: qwen session error: {e}");
                        }
                        // Session lifecycle and the whole-utterance echo are inert here: the
                        // segment boundary is TurnComplete or the idle gap.
                        QwenEvent::SessionReady | QwenEvent::InputTranscriptDone(_) => {}
                    }
                }
            }
            _ = &mut idle => { flush!(); }
        }
    }
}

/// Persist one finalized utterance, off-thread and de-duplicated.
///
/// Only the PRIMARY language session calls this, so an utterance is stored once rather
/// than once per language. The stored `translations` map therefore carries the primary
/// language rather than every language present — a real narrowing versus the old Groq
/// fan-out, which produced all of them in a single call. Recovering the full map would
/// need the per-language sessions to agree on an utterance identity they do not share.
fn finalize_record(
    state: &AppState,
    webinar_id: Uuid,
    record_transcript: bool,
    last_final: &mut Option<(String, std::time::Instant)>,
    original: String,
    source_language: &str,
    translations: HashMap<String, String>,
) {
    if original.is_empty() || !record_transcript {
        return;
    }
    // Drop a final that exactly repeats the previous one within the window — kills the
    // duplicate-final glitch without affecting genuinely distinct speech.
    const DUP_FINAL_WINDOW: Duration = Duration::from_secs(8);
    let now = std::time::Instant::now();
    if let Some((prev, when)) = last_final.as_ref() {
        if prev == &original && now.duration_since(*when) < DUP_FINAL_WINDOW {
            return;
        }
    }
    *last_final = Some((original.clone(), now));

    let Some(pool) = state.pool.clone() else {
        return;
    };
    let source_language = source_language.to_string();
    tokio::spawn(async move {
        persist_final(
            true,
            &pool,
            webinar_id,
            &original,
            &source_language,
            &translations,
        )
        .await;
    });
}

/// Persist a finalized utterance IFF the webinar records its transcript (P4).
/// Returns whether an insert was attempted, so the gate is unit-testable without
/// a live Qwen connection. When `record` is false this is a no-op (privacy default:
/// nothing is written unless the host explicitly opted in).
pub(crate) async fn persist_final(
    record: bool,
    pool: &crate::db::Pool,
    webinar_id: Uuid,
    original_text: &str,
    original_lang: &str,
    translations: &std::collections::HashMap<String, String>,
) -> bool {
    if !record {
        return false;
    }
    record_transcript_row(pool, webinar_id, original_text, original_lang, translations).await;
    true
}

/// Best-effort insert of one finalized utterance + its translation map (P4).
/// Fire-and-forget: a failed write is logged and swallowed — subtitles are live,
/// the transcript is a bonus.
async fn record_transcript_row(
    pool: &crate::db::Pool,
    webinar_id: Uuid,
    original_text: &str,
    original_lang: &str,
    translations: &std::collections::HashMap<String, String>,
) {
    let payload = serde_json::to_value(translations).unwrap_or_else(|_| serde_json::json!({}));
    let res = sqlx::query(
        "INSERT INTO webinar_transcripts
            (webinar_id, original_text, original_lang, translations)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(webinar_id)
    .bind(original_text)
    .bind(original_lang)
    .bind(payload)
    .execute(pool)
    .await;
    if let Err(e) = res {
        tracing::warn!(%webinar_id, "webinar STT: transcript persist failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- drop-log throttling ----

    #[test]
    fn first_drop_logs_immediately() {
        // An operator needs to know the INSTANT audio starts being discarded — a drop
        // that waits for the throttle window would hide the start of the incident.
        let mut t = DropLog::default();
        let t0 = std::time::Instant::now();
        assert!(t.should_log(t0));
    }

    #[test]
    fn a_burst_of_drops_logs_once_per_window() {
        let mut t = DropLog::default();
        let t0 = std::time::Instant::now();
        assert!(t.should_log(t0));
        // A sustained stall drops ~10 chunks/s per language; logging each would bury the
        // rest of the log and tell the operator nothing the counter doesn't.
        assert!(!t.should_log(t0 + Duration::from_millis(100)));
        assert!(!t.should_log(t0 + Duration::from_millis(900)));
        assert!(t.should_log(t0 + DROP_LOG_EVERY));
    }

    #[test]
    fn a_new_incident_after_quiet_logs_again() {
        // Edge-triggered on purpose: two separate stalls must produce two log lines, not
        // one, or a recurring fault reads as a single old event.
        let mut t = DropLog::default();
        let t0 = std::time::Instant::now();
        assert!(t.should_log(t0));
        assert!(t.should_log(t0 + DROP_LOG_EVERY * 2));
    }

    #[test]
    fn stt_allowed_only_for_live_or_scheduled() {
        assert!(stt_allowed("live"));
        assert!(stt_allowed("scheduled"));
        assert!(!stt_allowed("ended"));
        assert!(!stt_allowed("cancelled"));
        assert!(!stt_allowed(""));
    }

    // ---- P4: transcript persistence (DB-gated, mirrors db.rs unit tests) ----

    use std::collections::HashMap;
    use uuid::Uuid;

    /// Build the minimal FK chain (user → org → webinar) and return the webinar id.
    async fn seed_webinar(pool: &crate::db::Pool, record: bool) -> Uuid {
        let owner: Uuid = sqlx::query_scalar(
            "INSERT INTO users (google_id, email, name) VALUES ($1, $2, 'Host') RETURNING id",
        )
        .bind(format!("g-{}", Uuid::new_v4()))
        .bind(format!("{}@x.com", Uuid::new_v4()))
        .fetch_one(pool)
        .await
        .unwrap();
        let org: Uuid = sqlx::query_scalar(
            "INSERT INTO organizations (name, slug, owner_id) VALUES ('Acme', $1, $2) RETURNING id",
        )
        .bind(format!("org-{}", Uuid::new_v4().simple()))
        .bind(owner)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query_scalar(
            "INSERT INTO webinars (org_id, host_user_id, code, title, source_language, record_transcript)
             VALUES ($1, $2, $3, 'Launch', 'it', $4) RETURNING id",
        )
        .bind(org)
        .bind(owner)
        .bind(format!("code-{}", Uuid::new_v4().simple()))
        .bind(record)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn transcript_count(pool: &crate::db::Pool, webinar_id: Uuid) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM webinar_transcripts WHERE webinar_id = $1")
            .bind(webinar_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn persist_final_inserts_only_when_recording() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skipping db test — no DATABASE_URL");
            return;
        };
        let pool = crate::db::connect(&url).await.expect("connect");
        crate::db::migrate(&pool).await.expect("migrate");

        let mut translations = HashMap::new();
        translations.insert("it".to_string(), "ciao a tutti".to_string());
        translations.insert("en".to_string(), "hello everyone".to_string());

        // record_transcript = true → a row is inserted, JSONB round-trips.
        let wid_on = seed_webinar(&pool, true).await;
        let attempted =
            persist_final(true, &pool, wid_on, "ciao a tutti", "it", &translations).await;
        assert!(attempted, "recording on → insert attempted");
        assert_eq!(
            transcript_count(&pool, wid_on).await,
            1,
            "one row persisted"
        );

        let row: (String, String, serde_json::Value) = sqlx::query_as(
            "SELECT original_text, original_lang, translations
             FROM webinar_transcripts WHERE webinar_id = $1",
        )
        .bind(wid_on)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, "ciao a tutti");
        assert_eq!(row.1, "it");
        assert_eq!(row.2["en"], "hello everyone");
        assert_eq!(row.2["it"], "ciao a tutti");

        // record_transcript = false → nothing is written.
        let wid_off = seed_webinar(&pool, false).await;
        let attempted =
            persist_final(false, &pool, wid_off, "ciao a tutti", "it", &translations).await;
        assert!(!attempted, "recording off → no insert attempted");
        assert_eq!(
            transcript_count(&pool, wid_off).await,
            0,
            "no row persisted when record_transcript is off"
        );
    }
}
