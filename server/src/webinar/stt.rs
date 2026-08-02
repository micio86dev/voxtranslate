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
//! Fase 3 (translated TTS audio) is out of scope here — subtitles only.
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

use crate::auth::verify_jwt;
use crate::business::{db_err, not_found, require_pool, require_role, MEMBER};
use crate::engine::gemini::resample_pcm16_mono;
use crate::engine::qwen::{self, QwenEvent, QwenSink, QwenSource};
use crate::webinar::find_by_id;
use crate::webinar::presence::SubtitleEvent;
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

/// The subtitle decision for one Qwen transcription event: broadcast an interim, a
/// final (which the caller then translates), or drop it. Pure + unit-testable —
/// exactly the classification `process_webinar_transcripts` applies per event.
#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptDecision {
    /// A live partial (interim) in the source language — broadcast untranslated.
    Interim(String),
    /// A finalized utterance — the caller fans out a translation and broadcasts.
    Final(String),
    /// Nothing to do (not a transcription event, or empty/whitespace text).
    Drop,
}

/// Fold one Qwen event into the running interim `buffer` and return what to broadcast.
///
/// Qwen streams the host's words as [`QwenEvent::InputTranscript`] fragments — which may
/// be appends OR full snapshots, hence [`crate::engine::qwen::TextUpdate`] — and then
/// repeats the whole utterance as [`QwenEvent::InputTranscriptDone`]. So the interim is
/// the folded buffer (a growing partial, matching what viewers used to see) while the
/// final comes from the `Done` payload, which also resets the buffer for the next
/// utterance.
///
/// Pure (the buffer is the caller's), so the interim/final split, the accumulation, and
/// the empty-text drop are unit-tested without a live Qwen connection.
pub fn fold_event(buffer: &mut String, event: &QwenEvent) -> TranscriptDecision {
    match event {
        QwenEvent::InputTranscript(update) => {
            // Delta appends, Snapshot replaces. Blindly appending would repeat the
            // sentence on every frame from a `*.text` event — the duplication observed
            // against the live API.
            update.apply(buffer);
            let text = buffer.trim();
            if text.is_empty() {
                TranscriptDecision::Drop
            } else {
                TranscriptDecision::Interim(text.to_string())
            }
        }
        QwenEvent::InputTranscriptDone(whole) => {
            // Reset regardless: the next utterance must not inherit this one's partial,
            // even when the final itself is dropped as empty.
            buffer.clear();
            let text = whole.trim();
            if text.is_empty() {
                TranscriptDecision::Drop
            } else {
                TranscriptDecision::Final(text.to_string())
            }
        }
        // The transcribe-only session generates no speech and no translation, so every
        // other event (session.updated, response.done, errors) is inert here.
        _ => TranscriptDecision::Drop,
    }
}

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
    // Open the per-host Qwen TRANSCRIBE-only connection. On failure we just close: no
    // subtitles, but the webinar (WHIP media) is unaffected.
    let (qwen_sink, qwen_source) = match qwen::open_transcribe_session(&state.config.qwen).await {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!(%code, "webinar STT: qwen connect failed: {e}");
            return;
        }
    };

    let (mut ws_tx, mut ws_rx) = socket.split();
    // Audio bridge: host binary frames → Qwen.
    let (audio_tx, audio_rx) = mpsc::channel::<Vec<u8>>(AUDIO_CHANNEL_CAP);
    let forward = tokio::spawn(forward_audio_to_qwen(audio_rx, qwen_sink));
    let process = tokio::spawn(process_webinar_transcripts(
        qwen_source,
        state.clone(),
        code.clone(),
        source_language.clone(),
        webinar_id,
        record_transcript,
    ));
    // Abort-on-drop: if this future is dropped before the clean teardown below
    // (e.g. runtime shutdown, or the read loop never returning on a half-open
    // socket), both tasks are cancelled so the Qwen connection can't leak and
    // keep billing. On the normal path the awaits below finish first, making these
    // no-ops.
    let _abort = (
        AbortOnDrop(forward.abort_handle()),
        AbortOnDrop(process.abort_handle()),
    );
    // The presence WS pushes count/subtitle frames; the STT socket is audio-in
    // only, so there's nothing to send back on it. Keep the sink alive to close
    // it cleanly at the end.
    let _ = &mut ws_tx;

    while let Some(Ok(msg)) = ws_rx.next().await {
        match msg {
            Message::Binary(chunk) => {
                // A full audio channel means Qwen is behind; drop the chunk
                // rather than block the socket read (best-effort live captions).
                if audio_tx.try_send(chunk.to_vec()).is_err() {
                    tracing::debug!(%code, "webinar STT: audio buffer full, dropping chunk");
                }
            }
            // No `start`/`stop` control protocol: the stream itself is the signal.
            // Text frames are ignored; a Close ends the session.
            Message::Close(_) => break,
            _ => {}
        }
    }

    // Host disconnected: drop the audio channel so the pump commits the buffered
    // tail, then let the processor drain.
    drop(audio_tx);
    let _ = forward.await;
    let _ = process.await;
}

/// Pump host audio into the Qwen session: resample each 24 kHz capture chunk to the
/// 16 kHz the API wants and append it. When the channel closes (host disconnected) the
/// buffered tail is COMMITTED before the socket closes — otherwise turn detection would
/// wait for a silence that never arrives and the last utterance would never be
/// transcribed.
async fn forward_audio_to_qwen(mut audio_rx: mpsc::Receiver<Vec<u8>>, mut sink: QwenSink) {
    use futures::SinkExt as _;
    use tokio_tungstenite::tungstenite::Message as QwenMessage;

    while let Some(chunk) = audio_rx.recv().await {
        let chunk16 = resample_pcm16_mono(&chunk, qwen::CAPTURE_HZ, qwen::QWEN_INPUT_HZ);
        if sink
            .send(QwenMessage::text(qwen::audio_append_json(&chunk16)))
            .await
            .is_err()
        {
            return; // upstream gone; the processor sees the closed stream next
        }
    }
    let _ = sink
        .send(QwenMessage::text(qwen::audio_commit_json()))
        .await;
    let _ = sink.close().await;
}

/// Read Qwen transcription events for the host and drive viewer subtitles: interims
/// broadcast untranslated; finals fan out a translation into every viewer
/// language present RIGHT NOW (late joiners covered) and, when the webinar records
/// its transcript, persist a row best-effort. Wired to the presence registry instead
/// of `RoomManager`.
async fn process_webinar_transcripts(
    mut source: QwenSource,
    state: AppState,
    code: String,
    source_language: String,
    webinar_id: Uuid,
    record_transcript: bool,
) {
    // Guard against a byte-identical final arriving twice for the same utterance
    // (e.g. a reconnect replaying the tail).
    const DUP_FINAL_WINDOW: std::time::Duration = std::time::Duration::from_secs(8);
    let mut last_final: Option<(String, std::time::Instant)> = None;
    // The running partial the interim frames are built from — owned here, folded by
    // the pure `fold_event`.
    let mut partial = String::new();

    // Qwen is a tokio-tungstenite client, so its frames are tungstenite's
    // `Message` — a DISTINCT type from axum's `Message` used for the host socket.
    use tokio_tungstenite::tungstenite::Message as QwenMessage;
    while let Some(msg) = source.next().await {
        let raw = match msg {
            Ok(QwenMessage::Text(t)) => t.to_string(),
            Ok(QwenMessage::Binary(b)) => match String::from_utf8(b.to_vec()) {
                Ok(s) => s,
                Err(_) => continue,
            },
            Ok(QwenMessage::Close(_)) => break,
            Ok(_) => continue, // ping/pong — ignore
            Err(e) => {
                tracing::warn!(%code, "webinar STT: qwen stream error: {e}");
                break;
            }
        };

        for event in qwen::parse_server_message(&raw) {
            if let QwenEvent::Error(e) = &event {
                tracing::warn!(%code, "webinar STT: qwen session error: {e}");
            }
            match fold_event(&mut partial, &event) {
                TranscriptDecision::Drop => {}
                TranscriptDecision::Interim(text) => {
                    state.webinar_presence.broadcast_subtitle(
                        &code,
                        &SubtitleEvent::Interim {
                            text,
                            lang: source_language.clone(),
                        },
                    );
                }
                TranscriptDecision::Final(text) => {
                    // Drop a final that exactly repeats the previous one within the
                    // window — kills the duplicate-final glitch without affecting
                    // genuinely distinct (or later-repeated) speech.
                    let now = std::time::Instant::now();
                    if let Some((prev, when)) = &last_final {
                        if prev == &text && now.duration_since(*when) < DUP_FINAL_WINDOW {
                            continue;
                        }
                    }
                    last_final = Some((text.clone(), now));

                    // Spawn the translate → broadcast → persist work so a slow/stalled
                    // Groq call never head-of-line-blocks subsequent finals & interims.
                    // The dup-final guard above stays in the loop, so ordering of the
                    // dedup decision is preserved.
                    let state = state.clone();
                    let code = code.clone();
                    let source_language = source_language.clone();
                    tokio::spawn(async move {
                        // Fan out per distinct viewer language read LIVE from the
                        // registry (P2: late joiners covered — we read on every final).
                        let targets = state.webinar_presence.target_languages(&code);
                        let translations = state
                            .translator
                            .translate_fanout(&text, &source_language, &targets, None)
                            .await;

                        state.webinar_presence.broadcast_subtitle(
                            &code,
                            &SubtitleEvent::Final {
                                original: text.clone(),
                                lang: source_language.clone(),
                                translations: translations.clone(),
                            },
                        );

                        // P4: persist best-effort only when recording is on — same
                        // posture as `presence::record_join` (`let _ = ...`).
                        if let Some(pool) = state.pool.clone() {
                            persist_final(
                                record_transcript,
                                &pool,
                                webinar_id,
                                &text,
                                &source_language,
                                &translations,
                            )
                            .await;
                        }
                    });
                }
            }
        }
    }
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

    use crate::engine::qwen::TextUpdate;

    fn delta(text: &str) -> QwenEvent {
        QwenEvent::InputTranscript(TextUpdate::Delta(text.to_string()))
    }

    /// A `*.text` frame: the whole utterance so far, which must REPLACE the buffer.
    fn snapshot(text: &str) -> QwenEvent {
        QwenEvent::InputTranscript(TextUpdate::Snapshot(text.to_string()))
    }

    fn done(text: &str) -> QwenEvent {
        QwenEvent::InputTranscriptDone(text.to_string())
    }

    #[test]
    fn interims_accumulate_the_running_partial() {
        // Qwen streams fragments; viewers must see the sentence GROW, not each fragment
        // on its own.
        let mut buf = String::new();
        assert_eq!(
            fold_event(&mut buf, &delta("ciao ")),
            TranscriptDecision::Interim("ciao".to_string())
        );
        assert_eq!(
            fold_event(&mut buf, &delta("a ")),
            TranscriptDecision::Interim("ciao a".to_string())
        );
        assert_eq!(
            fold_event(&mut buf, &delta("tutti")),
            TranscriptDecision::Interim("ciao a tutti".to_string())
        );
    }

    #[test]
    fn snapshots_replace_instead_of_appending() {
        // The livetranslate family re-sends the WHOLE utterance every frame. Appending
        // them produced "ciaociao aciao a tutti" against the live API; the buffer must
        // track the latest snapshot instead.
        let mut buf = String::new();
        assert_eq!(
            fold_event(&mut buf, &snapshot("ciao")),
            TranscriptDecision::Interim("ciao".to_string())
        );
        assert_eq!(
            fold_event(&mut buf, &snapshot("ciao a")),
            TranscriptDecision::Interim("ciao a".to_string())
        );
        assert_eq!(
            fold_event(&mut buf, &snapshot("ciao a tutti")),
            TranscriptDecision::Interim("ciao a tutti".to_string())
        );
        assert_eq!(buf, "ciao a tutti");
    }

    #[test]
    fn completed_becomes_the_final_and_resets_the_partial() {
        let mut buf = String::new();
        fold_event(&mut buf, &delta("ciao a"));
        assert_eq!(
            fold_event(&mut buf, &done("ciao a tutti")),
            TranscriptDecision::Final("ciao a tutti".to_string())
        );
        // The next utterance starts clean — otherwise it would inherit "ciao a".
        assert!(buf.is_empty());
        assert_eq!(
            fold_event(&mut buf, &delta("benvenuti")),
            TranscriptDecision::Interim("benvenuti".to_string())
        );
    }

    #[test]
    fn drops_empty_and_whitespace_text() {
        let mut buf = String::new();
        assert_eq!(
            fold_event(&mut buf, &delta("   ")),
            TranscriptDecision::Drop
        );
        assert_eq!(fold_event(&mut buf, &done("")), TranscriptDecision::Drop);
        // An empty final still resets, so the whitespace partial can't leak forward.
        assert!(buf.is_empty());
    }

    #[test]
    fn trims_transcript_text() {
        let mut buf = String::new();
        assert_eq!(
            fold_event(&mut buf, &done("  hola  ")),
            TranscriptDecision::Final("hola".to_string())
        );
    }

    #[test]
    fn ignores_events_the_transcribe_session_does_not_produce() {
        // The transcribe-only session generates no speech and no translation; those
        // events must be inert rather than leaking into subtitles.
        let mut buf = String::new();
        for event in [
            QwenEvent::SessionReady,
            QwenEvent::TurnComplete,
            QwenEvent::OutputTranscript(TextUpdate::Delta("should not appear".into())),
            QwenEvent::OutputAudio(vec![1, 2, 3]),
            QwenEvent::Error("boom".into()),
        ] {
            assert_eq!(fold_event(&mut buf, &event), TranscriptDecision::Drop);
        }
        assert!(buf.is_empty());
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
