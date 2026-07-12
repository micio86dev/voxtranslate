//! Webinar realtime subtitles — the host's server-side STT ingest (SPEC "Webinar
//! Mode" Fase 2).
//!
//! In a webinar the broadcast media travels host → WHIP → LL-HLS, which the server
//! never touches. For subtitles the host client opens a SECOND, audio-only path to
//! this WebSocket (mirroring the P2P call's MediaRecorder → server bridge): it
//! streams WebM/Opus chunks here, the server runs Deepgram streaming STT on them,
//! translates each finalized utterance into every viewer language currently
//! present, and pushes subtitle frames to viewers over the EXISTING presence WS
//! ([`crate::webinar::presence::broadcast_subtitle`]).
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
//! This reuses the P2P call's [`crate::deepgram::forward_audio`] pump verbatim:
//! the host sends **binary WebSocket frames**, each a WebM/Opus chunk (spec 0043,
//! 100 ms). There are NO `start`/`stop` JSON control frames — the STREAM ITSELF is
//! the signal: the first binary chunk starts STT, and CLOSING the socket flushes
//! pending finals (`forward_audio` emits Deepgram's `CloseStream` when its audio
//! channel drops). Inbound text frames are ignored.

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
use crate::deepgram::{forward_audio, open_deepgram_ws, AudioFormat};
use crate::protocol::DeepgramResponse;
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

/// The confidence floor for accepting a Deepgram transcript. Below this we drop
/// the frame (garbage/near-silence) — same gate as the P2P `process_transcripts`.
const MIN_CONFIDENCE: f32 = 0.4;

/// The subtitle decision for one parsed Deepgram frame: broadcast an interim, a
/// final (which the caller then translates), or drop it. Pure + unit-testable —
/// exactly the classification `process_webinar_transcripts` applies per frame.
#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptDecision {
    /// A live partial (interim) in the source language — broadcast untranslated.
    Interim(String),
    /// A finalized utterance — the caller fans out a translation and broadcasts.
    Final(String),
    /// Nothing to do (not a transcript, empty text, or below the confidence gate).
    Drop,
}

/// Classify one raw Deepgram text frame into a [`TranscriptDecision`]. Pure, so
/// the confidence gate + interim/final split + empty-text drop are unit-tested
/// without a live Deepgram (models `deepgram.rs`'s parse tests).
pub fn classify_transcript(raw: &str) -> TranscriptDecision {
    let Ok(parsed) = serde_json::from_str::<DeepgramResponse>(raw) else {
        return TranscriptDecision::Drop; // a non-Results frame we don't model
    };
    // `best_alternative` already trims + drops empty transcripts and non-Results.
    let Some((text, confidence)) = parsed.best_alternative() else {
        return TranscriptDecision::Drop;
    };
    if confidence < MIN_CONFIDENCE {
        return TranscriptDecision::Drop;
    }
    if parsed.is_final {
        TranscriptDecision::Final(text.to_string())
    } else {
        TranscriptDecision::Interim(text.to_string())
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
    // multiply Deepgram+Groq cost or double up subtitles. Acquire BEFORE the
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

/// On upgrade: bridge the host's audio into Deepgram and drive the subtitle
/// fan-out. Binary frames → Deepgram; Deepgram finals → translate → broadcast.
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
    // Open the per-host Deepgram streaming connection (WebM/Opus, spec 0043 —
    // same as the P2P call's MediaRecorder path). On failure we just close: no
    // subtitles, but the webinar (WHIP media) is unaffected.
    let (dg_sink, dg_source) =
        match open_deepgram_ws(&source_language, &state.config, AudioFormat::WebmOpus).await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(%code, "webinar STT: deepgram connect failed: {e}");
                return;
            }
        };

    let (mut ws_tx, mut ws_rx) = socket.split();
    // Audio bridge: host binary frames → Deepgram (reuses the call's pump).
    let (audio_tx, audio_rx) = mpsc::channel::<Vec<u8>>(64);
    let forward = tokio::spawn(forward_audio(audio_rx, dg_sink));
    let process = tokio::spawn(process_webinar_transcripts(
        dg_source,
        state.clone(),
        code.clone(),
        source_language.clone(),
        webinar_id,
        record_transcript,
    ));
    // Abort-on-drop: if this future is dropped before the clean teardown below
    // (e.g. runtime shutdown, or the read loop never returning on a half-open
    // socket), both tasks are cancelled so the Deepgram connection can't leak and
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
                // A full audio channel means Deepgram is behind; drop the chunk
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

    // Host disconnected: drop the audio channel so `forward_audio` flushes finals
    // via CloseStream, then let the processor drain.
    drop(audio_tx);
    let _ = forward.await;
    let _ = process.await;
}

/// Read Deepgram transcripts for the host and drive viewer subtitles: interims
/// broadcast untranslated; finals fan out a translation into every viewer
/// language present RIGHT NOW (late joiners covered) and, when the webinar records
/// its transcript, persist a row best-effort. Modeled on the P2P
/// `deepgram::process_transcripts` (confidence gate + dup-final window) but wired
/// to the presence registry instead of `RoomManager`.
async fn process_webinar_transcripts(
    mut source: crate::deepgram::DgSource,
    state: AppState,
    code: String,
    source_language: String,
    webinar_id: Uuid,
    record_transcript: bool,
) {
    // Guard against Deepgram occasionally re-emitting a byte-identical final for
    // the same utterance (the "repeats the same phrase" glitch) — same window as
    // the P2P path.
    const DUP_FINAL_WINDOW: std::time::Duration = std::time::Duration::from_secs(8);
    let mut last_final: Option<(String, std::time::Instant)> = None;

    // Deepgram is a tokio-tungstenite client, so its frames are tungstenite's
    // `Message` — a DISTINCT type from axum's `Message` used for the host socket.
    use tokio_tungstenite::tungstenite::Message as DgMessage;
    while let Some(msg) = source.next().await {
        let raw = match msg {
            Ok(DgMessage::Text(t)) => t,
            Ok(DgMessage::Close(_)) => break,
            Ok(_) => continue, // ping/pong/binary — ignore
            Err(e) => {
                tracing::warn!(%code, "webinar STT: deepgram stream error: {e}");
                break;
            }
        };

        match classify_transcript(raw.as_str()) {
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
                // Groq call never head-of-line-blocks subsequent finals & interims
                // (mirrors the P2P path in `deepgram::process_transcripts`, which
                // also spawns per final). The dup-final guard above stays in the
                // loop, so ordering of the dedup decision is preserved.
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

/// Persist a finalized utterance IFF the webinar records its transcript (P4).
/// Returns whether an insert was attempted, so the gate is unit-testable without
/// a live Deepgram. When `record` is false this is a no-op (privacy default:
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

    fn dg_frame(is_final: bool, transcript: &str, confidence: f32) -> String {
        serde_json::json!({
            "type": "Results",
            "is_final": is_final,
            "channel": { "alternatives": [
                { "transcript": transcript, "confidence": confidence }
            ]}
        })
        .to_string()
    }

    #[test]
    fn classifies_final_above_confidence() {
        let d = classify_transcript(&dg_frame(true, "ciao a tutti", 0.95));
        assert_eq!(d, TranscriptDecision::Final("ciao a tutti".to_string()));
    }

    #[test]
    fn classifies_interim_above_confidence() {
        let d = classify_transcript(&dg_frame(false, "ciao a", 0.9));
        assert_eq!(d, TranscriptDecision::Interim("ciao a".to_string()));
    }

    #[test]
    fn drops_below_confidence_gate() {
        // A final AND an interim below the floor are both dropped.
        assert_eq!(
            classify_transcript(&dg_frame(true, "garbage", 0.2)),
            TranscriptDecision::Drop
        );
        assert_eq!(
            classify_transcript(&dg_frame(false, "garbage", 0.39)),
            TranscriptDecision::Drop
        );
        // Exactly at the floor is accepted.
        assert_eq!(
            classify_transcript(&dg_frame(true, "ok", MIN_CONFIDENCE)),
            TranscriptDecision::Final("ok".to_string())
        );
    }

    #[test]
    fn drops_empty_and_whitespace_text() {
        assert_eq!(
            classify_transcript(&dg_frame(true, "", 0.99)),
            TranscriptDecision::Drop
        );
        assert_eq!(
            classify_transcript(&dg_frame(false, "   ", 0.99)),
            TranscriptDecision::Drop
        );
    }

    #[test]
    fn trims_transcript_text() {
        assert_eq!(
            classify_transcript(&dg_frame(true, "  hola  ", 0.9)),
            TranscriptDecision::Final("hola".to_string())
        );
    }

    #[test]
    fn drops_non_results_and_garbage_frames() {
        // Metadata / SpeechStarted etc. are not `Results`.
        let meta = serde_json::json!({ "type": "Metadata" }).to_string();
        assert_eq!(classify_transcript(&meta), TranscriptDecision::Drop);
        // Not even JSON.
        assert_eq!(classify_transcript("not json"), TranscriptDecision::Drop);
        // Results with no alternatives.
        let empty = serde_json::json!({ "type": "Results", "is_final": true, "channel": { "alternatives": [] }}).to_string();
        assert_eq!(classify_transcript(&empty), TranscriptDecision::Drop);
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
