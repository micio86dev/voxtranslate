//! `/ws/live` — one phone's socket in a Live Connect conversation.
//!
//! Each phone opens its own socket with the token it was issued. The server puts all of them in a
//! private room named after the session and reuses the meeting machinery for everything
//! after that: the engine opens one upstream session per speaker **per target language**, so
//! phones speaking different languages are independent concurrent translation streams
//! with nothing here coordinating them. This module adds what meetings do not have: a seat
//! that already exists (no client-chosen identity), a payer who is not on the call, and an
//! explicit end-of-conversation signal.
//!
//! Hardening is copied from `/ws`, not `/ws/talk` (which has none): connection cap, per-IP
//! connect limit, frame and message-rate caps, ping/idle eviction.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use chrono::Utc;
use futures::{SinkExt as _, StreamExt as _};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use super::billing::{EndCause, LiveMeter, LivePayer};
use super::echo::{EchoConfig, EchoFilter};
use super::enhanced::{self, EnhancedFrame, FrameGate, Notifier, Pipeline, Segment};
use super::routes::{api_err, owner_can_afford};
use super::sessions::{self, PeerAuth};
use super::state::SessionLatency;
use super::tiers;
use super::tokens::PeerRole;
use crate::db::Pool;
use crate::deepgram::SpeakerCtx;
use crate::engine::{SessionDeps, SessionOutcome, TranscriptWriter};
use crate::protocol::{ClientMessage, ServerMessage};
use crate::rooms::{LeaveOutcome, Peer, PeerTx, Visibility, OUT_CHANNEL_CAP};
use crate::{
    AppState, ConnGuard, MAX_FRAME_BYTES, WS_IDLE_TIMEOUT, WS_MSG_MAX, WS_MSG_WINDOW,
    WS_PING_INTERVAL,
};

/// How long the writer gets to flush the last frames (the end-of-conversation notice) before
/// the socket is dropped.
const FLUSH_GRACE: Duration = Duration::from_secs(2);

/// A 100 ms frame counts as speech when its mean absolute sample is at least this (of 32 768,
/// roughly -44 dBFS): far below speech, far above a muted or gated microphone's digital silence.
/// Deliberately low, so a quiet talker is measured rather than skipped.
const VOICED_MEAN_ABS: u32 = 80;
/// Speech after at least this much non-speech is a new utterance. The client streams its
/// microphone continuously while it is not muted, so there are no gaps in the *frames*: the
/// onset has to come from the audio itself.
const QUIET_GAP: Duration = Duration::from_millis(1000);
/// Largest binary audio frame a phone may send. An honest client sends ~100 ms of PCM16 mono at
/// 24 kHz, 4 800 bytes; this leaves room for a 340 ms worklet block and for some jitter, and no
/// more. The general frame cap (64 KB) would let one frame carry 1.3 s of audio, so a client
/// could outrun the wall-clock meter by packing time into frames.
pub const MAX_AUDIO_FRAME_BYTES: usize = 16 * 1024;
/// The most throttle reports one socket writes to the session's diagnostics: enough to see that
/// it happened, never enough to fill the table's per-session bound with noise.
const MAX_THROTTLE_REPORTS: u32 = 10;
/// The exact start of every translated-audio frame (`ServerMessage` is internally tagged with
/// `type` first), so the pump can recognise one without parsing or allocating.
const TRANSLATED_AUDIO_PREFIX: &str = r#"{"type":"translated_audio""#;

/// Mean absolute sample of `frame` (PCM16 little-endian). One pass over 4.8 kB, no allocation.
fn mean_abs(frame: &[u8]) -> u32 {
    let mut sum: u64 = 0;
    let mut n: u64 = 0;
    let (samples, _) = frame.as_chunks::<2>();
    for s in samples {
        sum += u64::from(i16::from_le_bytes(*s).unsigned_abs());
        n += 1;
    }
    u32::try_from(sum.checked_div(n).unwrap_or(0)).unwrap_or(u32::MAX)
}

/// Whether `frame` (PCM16 little-endian) is voiced: its mean absolute sample reaches
/// [`VOICED_MEAN_ABS`].
fn is_voiced(frame: &[u8]) -> bool {
    mean_abs(frame) >= VOICED_MEAN_ABS
}

/// Does this forwarded frame start an utterance? True for the first voiced frame after a
/// [`Start`](ClientMessage::Start) (the caller clears `last_voiced`) or after [`QUIET_GAP`] without
/// one. Updates `last_voiced` as it goes.
fn utterance_onset(
    last_voiced: &mut Option<tokio::time::Instant>,
    frame: &[u8],
    now: tokio::time::Instant,
) -> bool {
    if !is_voiced(frame) {
        return false;
    }
    let onset = last_voiced.is_none_or(|t| now.saturating_duration_since(t) >= QUIET_GAP);
    *last_voiced = Some(now);
    onset
}

/// What one phone is allowed to feed the engine: a token bucket in bytes, refilled at
/// `rate` bytes per second with one second of burst. The meter bills the wall clock, so audio
/// that arrives faster than it plays would be translated (and billed upstream) for less than it
/// costs the payer. Over budget a frame is DROPPED, not queued: queueing turns an abusive sender
/// into latency for an honest one, and a real-time conversation has no use for old audio.
///
/// Honest traffic is one 4 800-byte frame every 100 ms (48 000 B/s), 1.5x inside the default
/// budget of 72 000 B/s; a network stall that delivers a second of audio at once still fits the
/// burst.
#[derive(Debug)]
struct AudioGate {
    rate: f64,
    tokens: f64,
    last: tokio::time::Instant,
    dropped_frames: u64,
    dropped_bytes: u64,
    oversized: u64,
    last_report: Option<tokio::time::Instant>,
    reports: u32,
}

impl AudioGate {
    fn new(bytes_per_sec: i64, now: tokio::time::Instant) -> Self {
        let rate = bytes_per_sec.max(1) as f64;
        Self {
            rate,
            tokens: rate,
            last: now,
            dropped_frames: 0,
            dropped_bytes: 0,
            oversized: 0,
            last_report: None,
            reports: 0,
        }
    }

    /// Whether a frame of `bytes` may be forwarded now (and if so, spend its bytes).
    fn admit(&mut self, bytes: usize, now: tokio::time::Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.rate).min(self.rate);
        let need = bytes as f64;
        if self.tokens >= need {
            self.tokens -= need;
            true
        } else {
            self.dropped_frames += 1;
            self.dropped_bytes += bytes as u64;
            false
        }
    }

    /// A frame over [`MAX_AUDIO_FRAME_BYTES`]: never forwarded, counted for the next report.
    fn refuse_oversized(&mut self, bytes: usize) {
        self.oversized += 1;
        self.dropped_frames += 1;
        self.dropped_bytes += bytes as u64;
    }

    /// What was dropped since the last report, at most once a second and a bounded number of
    /// times per socket: `(frames, bytes, oversized)`. Counts only, never audio.
    fn take_report(&mut self, now: tokio::time::Instant) -> Option<(u64, u64, u64)> {
        if self.dropped_frames == 0 || self.reports >= MAX_THROTTLE_REPORTS {
            return None;
        }
        if self
            .last_report
            .is_some_and(|t| now.saturating_duration_since(t) < Duration::from_secs(1))
        {
            return None;
        }
        self.last_report = Some(now);
        self.reports += 1;
        let out = (self.dropped_frames, self.dropped_bytes, self.oversized);
        self.dropped_frames = 0;
        self.dropped_bytes = 0;
        self.oversized = 0;
        Some(out)
    }
}

/// Same as the meeting socket's writer (frames out, a ping on a fixed cadence), plus two things.
///
/// When the frame just delivered is translated audio, close the latency measurement of the
/// utterance it answers. Kept here, at the last point before the socket, so the number is "the
/// other phone was handed its audio" and not "the engine produced it". The cost is one string
/// prefix check per frame and, per utterance, one atomic swap and one histogram increment.
///
/// And every frame first passes this seat's [`EchoFilter`]: the other phone's microphone also
/// hears this phone's speaker, and what the engine makes of that must not be played back to the
/// speaker. The filter may hold a new utterance of the other speaker for at most
/// [`super::echo::HOLD_MAX`], so the loop has a timer branch that releases it (fail open).
async fn pump_live(
    mut rx: tokio::sync::mpsc::Receiver<String>,
    mut ws_tx: futures::stream::SplitSink<WebSocket, Message>,
    latency: Arc<SessionLatency>,
    seat: usize,
    mut echo: EchoFilter,
    diag: (Pool, Uuid),
) {
    let mut ping = tokio::time::interval(WS_PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ping.tick().await;
    let mut out: Vec<String> = Vec::with_capacity(4);
    let my_id = echo.my_id().to_string();
    loop {
        let hold_until = echo.deadline();
        let hold_over = async {
            match hold_until {
                Some(t) => tokio::time::sleep_until(t).await,
                None => std::future::pending().await,
            }
        };
        out.clear();
        tokio::select! {
            msg = rx.recv() => {
                let now = tokio::time::Instant::now();
                match msg {
                    // Enhanced text mode has no microphone levels: the audio-level filter could only
                    // hold captions back, so those conversations use the text guard instead.
                    Some(m) if echo.enabled() && !latency.text_mode() => {
                        echo.note_own_voice(latency.mic_last_voiced(seat));
                        // Every speaker id on a frame is a peer id; each peer has a seat, and each
                        // seat its own smoothed microphone. A speaker whose phone has not
                        // connected has no seat and reads as 0, which is "unknown" and never
                        // suppresses (the filter fails open).
                        let level = |id: &str| {
                            let s = if id == my_id { Some(seat) } else { latency.seat_of(id) };
                            s.map_or(0, |s| latency.mic_level(s, now))
                        };
                        echo.on_frame(now, m, &level, &mut out);
                        if echo.has_report() && latency.allow_echo_report(now) {
                            let (suppressed, failed_open) = echo.take_report();
                            let (pool, session) = diag.clone();
                            tokio::spawn(async move {
                                super::sessions::record_event(
                                    &pool,
                                    session,
                                    "diagnostics",
                                    json!({ "echo_filter": {
                                        "suppressed": suppressed,
                                        "failed_open": failed_open,
                                    } }),
                                )
                                .await;
                            });
                        }
                    }
                    Some(m) => out.push(m),
                    None => {
                        // The channel closed: let go of anything still held, then stop.
                        echo.drain(&mut out);
                        for m in out.drain(..) {
                            let _ = send_frame(&mut ws_tx, &latency, seat, m).await;
                        }
                        break;
                    }
                }
            }
            () = hold_over => echo.on_timeout(tokio::time::Instant::now(), &mut out),
            _ = ping.tick() => {
                if ws_tx.send(Message::Ping(Vec::<u8>::new().into())).await.is_err() {
                    break;
                }
            }
        }
        for m in out.drain(..) {
            if send_frame(&mut ws_tx, &latency, seat, m).await.is_err() {
                return;
            }
        }
    }
}

/// Write one frame to the socket and, when it is translated audio, close the latency probe of
/// the utterance it answers.
async fn send_frame(
    ws_tx: &mut futures::stream::SplitSink<WebSocket, Message>,
    latency: &SessionLatency,
    seat: usize,
    m: String,
) -> Result<(), ()> {
    let audio = m.starts_with(TRANSLATED_AUDIO_PREFIX);
    // Whose speech this audio answers, read before the frame is handed to the socket.
    let speaker = if audio {
        speaker_seat(latency, &m)
    } else {
        None
    };
    if ws_tx.send(Message::Text(m.into())).await.is_err() {
        return Err(());
    }
    if let Some(speaker) = speaker {
        if let Some(d) = latency.audio_delivered(seat, speaker, tokio::time::Instant::now()) {
            crate::metrics::record_live_translation_ms(
                u64::try_from(d.as_micros().div_ceil(1000))
                    .unwrap_or(u64::MAX)
                    .max(1),
            );
        }
    }
    Ok(())
}

/// The seat of the speaker a `translated_audio` frame answers. The frame is
/// `{"type":"translated_audio","speaker_id":"<peer id>",...}` (`speaker_id` is the first field
/// after the tag), so the id is the text between the quotes that follow it. Once per audio chunk,
/// no allocation; `None` for anything that does not parse (the sample is simply not taken).
fn speaker_seat(latency: &SessionLatency, frame: &str) -> Option<usize> {
    const KEY: &str = r#""speaker_id":""#;
    let rest = frame
        .get(TRANSLATED_AUDIO_PREFIX.len()..)?
        .strip_prefix(',')?;
    let id = rest.strip_prefix(KEY)?.split_once('"')?.0;
    latency.seat_of(id)
}

/// The room a session's phones share. Server-minted and unguessable, so it can never
/// collide with (or be joined through) a public `/ws` room.
pub fn room_name(session_id: Uuid) -> String {
    format!("live-{session_id}")
}

#[derive(Debug, Deserialize)]
pub struct WsQuery {
    pub token: String,
}

pub async fn ws_live(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<WsQuery>,
) -> Response {
    let ip = crate::observability::client_ip(&headers);
    if !state.rate_limiter.allow(
        &format!("livews:{ip}"),
        state.ws_connect_max,
        Duration::from_secs(60),
    ) {
        return api_err(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "Too many requests",
        );
    }
    let Some(keys) = state.live.config.keys.as_ref() else {
        return api_err(
            StatusCode::NOT_FOUND,
            "live_disabled",
            "Live Connect is not available",
        );
    };
    // Refuse BEFORE upgrading: a bad, expired or ended-session token never costs a socket.
    let claims = match keys.verify_guest_token(&q.token, Utc::now().timestamp()) {
        Ok(c) => c,
        Err(e) => return api_err(StatusCode::UNAUTHORIZED, e.code(), "invalid token"),
    };
    let Some(pool) = state.pool.as_ref() else {
        return api_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "database not configured",
        );
    };
    let auth = match sessions::authorize_peer(pool, &claims).await {
        Ok(a) => a,
        Err(_) => {
            return api_err(
                StatusCode::GONE,
                "session_ended",
                "this conversation has ended",
            );
        }
    };
    ws.max_message_size(256 * 1024)
        .on_upgrade(move |socket| handle_live_peer(socket, state, auth))
}

/// Make sure the meeting-side row exists for the room's current session, and that the live
/// row points at it.
///
/// A room's session id is minted by the room when it is created, and a room is deleted when
/// its last peer leaves — so a conversation that outlives a flap of both phones spans more
/// than one. Each is recorded as a `room_session` event so finalization can gather them all.
async fn ensure_call_session(
    state: &AppState,
    auth: &PeerAuth,
    room: &str,
    session_id: Uuid,
) -> Result<(), sqlx::Error> {
    let Some(pool) = state.pool.as_ref() else {
        return Ok(());
    };
    sqlx::query(
        "INSERT INTO call_sessions (id, room, kind, org_id, project_id)
         VALUES ($1, $2, 'live_connect', $3, $4)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(session_id)
    .bind(room)
    .bind(auth.org_id)
    .bind(auth.project_id)
    .execute(pool)
    .await?;
    let moved = sqlx::query(
        "UPDATE live_sessions SET call_session_id = $1
         WHERE id = $2 AND call_session_id IS DISTINCT FROM $1",
    )
    .bind(session_id)
    .bind(auth.session_id)
    .execute(pool)
    .await?;
    if moved.rows_affected() == 1 {
        sessions::record_event(
            pool,
            auth.session_id,
            "room_session",
            json!({ "call_session_id": session_id }),
        )
        .await;
    }
    Ok(())
}

async fn send_close(mut ws_tx: futures::stream::SplitSink<WebSocket, Message>, msg: ServerMessage) {
    let _ = ws_tx.send(Message::Text(msg.to_json().into())).await;
    let _ = ws_tx.close().await;
}

async fn handle_live_peer(socket: WebSocket, state: AppState, auth: PeerAuth) {
    let (ws_tx, mut ws_rx) = socket.split();

    let (_conn, count) = ConnGuard::acquire(&state.ws_conns);
    if count > state.max_ws_conns {
        send_close(
            ws_tx,
            ServerMessage::Error {
                message: "server busy".into(),
                code: Some("server_busy".into()),
            },
        )
        .await;
        return;
    }
    let Some(pool) = state.pool.clone() else {
        return;
    };

    let room = room_name(auth.session_id);
    let peer_id = auth.peer_id.to_string();
    let conn = Uuid::new_v4();
    let (out_tx, out_rx, out_overflow) = PeerTx::channel(OUT_CHANNEL_CAP);

    // The seat is already decided: id, name and language come from the database, never from
    // the client. A reconnect with the same token reuses the id, so the room supersedes the
    // stale socket instead of seating a stranger.
    let peer = Peer {
        id: peer_id.clone(),
        conn,
        name: auth.display_name.clone(),
        lang: auth.lang.clone(),
        // Deliberately None: a room peer's `user_id` is broadcast to everyone in the room, and
        // the other phone has no business learning the owner's (or a staff member's) account
        // id. The engine and the transcript calls take `auth.user_id` directly.
        user_id: None,
        engine: session_engine(&state, &auth).metadata().id.clone(),
        avatar_url: None,
        cartesia_voice_id: None,
        tx: out_tx.clone(),
        speaking: Arc::new(AtomicBool::new(false)),
    };
    let joined = match state.rooms.join(&room, peer, Visibility::Private) {
        Ok(j) => j,
        Err(()) => {
            send_close(ws_tx, ServerMessage::RoomFull).await;
            return;
        }
    };
    let call_session = joined.session_id;

    // Persistence follows policy: a business session always has its row (the org ledger
    // references it); a consumer one only when the owner opted in to saving the transcript.
    let keep_row = auth.org_id.is_some() || auth.save_transcript;
    let mut participant_row = None;
    let mut call_row_ok = true;
    if keep_row {
        if let Err(e) = ensure_call_session(&state, &auth, &room, call_session).await {
            tracing::error!(error = %e, "live: could not record the call session");
            call_row_ok = false;
        }
    }
    // Without our own `live_connect` row, `session_started` would create a plain `call` row and
    // inherit whatever binding the room name carries. Better to store nothing than that.
    let transcripts = if auth.save_transcript && call_row_ok {
        state.transcripts.clone()
    } else {
        None
    };
    if let Some(t) = transcripts.as_ref() {
        // Same insert the meeting path does; `kind = 'live_connect'` is already in place.
        let _ = t.session_started(call_session, &room).await;
        participant_row = t
            .participant_joined(
                call_session,
                &peer_id,
                auth.user_id,
                &auth.display_name,
                &auth.lang,
            )
            .await
            .ok();
    }

    let _ = out_tx.send(
        ServerMessage::RoomJoined {
            peer_id: peer_id.clone(),
            peers: joined.existing,
            session_id: transcripts.as_ref().map(|_| call_session.to_string()),
            public: false,
        }
        .to_json(),
    );
    // Live Connect always captures raw PCM16: it is the Safari-safe path and what every
    // server engine takes.
    let _ = out_tx.send(ServerMessage::CaptureFormat { pcm: true }.to_json());
    state.rooms.broadcast_except(
        &room,
        &peer_id,
        &ServerMessage::PeerJoined {
            peer_id: peer_id.clone(),
            user_name: auth.display_name.clone(),
            lang: auth.lang.clone(),
            user_id: None,
            avatar_url: None,
            cartesia_voice_id: None,
        }
        .to_json(),
    );
    sessions::record_event(
        &pool,
        auth.session_id,
        "connected",
        json!({ "role": auth.role }),
    )
    .await;

    // The owner side receives balance updates for BOTH phones' speech.
    if auth.role != PeerRole::Guest {
        state.live.set_owner(auth.session_id, conn, out_tx.clone());
    }

    let latency = state.live.latency_for(auth.session_id);
    // This phone's seat for the probes and the echo filter's microphone levels: its own slot, the
    // same one again if it reconnects.
    let seat = latency.seat_for(&peer_id);
    // Enhanced conversations run on text: no engine session, no audio. Fixed for the socket's
    // life; the fallback to Standard is read live (see `is_text_mode`).
    let text_mode = is_text_mode(&state, &auth);
    latency.set_text_mode(text_mode);
    let echo = EchoFilter::new(
        EchoConfig {
            enabled: state.live.config.echo_filter,
            sim: state.live.config.echo_sim,
        },
        peer_id.clone(),
        auth.lang.clone(),
    );
    let send_task = tokio::spawn(pump_live(
        out_rx,
        ws_tx,
        latency.clone(),
        seat,
        echo,
        (pool.clone(), auth.session_id),
    ));
    // When this phone last sent a frame with speech in it, for the latency probe's onset rule.
    let mut last_voiced: Option<tokio::time::Instant> = None;

    // ---- Enhanced (text) state ---------------------------------------------------------
    // The translation worker of this phone as a speaker. Segments go in the order they were sent;
    // the worker is gone when `text_tx` is dropped at teardown.
    let text_tx = if text_mode {
        let (tx, rx) = mpsc::channel::<Segment>(enhanced::QUEUE_CAP);
        // The host's cloned voice goes out only for the host's own words, only when the host
        // opted into it for THIS conversation, only while cloning is enabled here.
        let voice_id = match (
            state.config.cartesia.as_ref(),
            state.billing.as_ref(),
            auth.user_id,
        ) {
            (Some(c), Some(b), Some(user))
                if auth.use_voice_clone
                    && auth.role == PeerRole::Host
                    && c.voice_cloning_enabled =>
            {
                b.get_cartesia_voice_id(user)
                    .await
                    .ok()
                    .flatten()
                    .filter(|v| !v.is_empty())
            }
            _ => None,
        };
        tokio::spawn(
            Pipeline {
                rooms: state.rooms.clone(),
                translator: state.translator.clone(),
                transcripts: transcripts.clone(),
                latency: latency.clone(),
                room: room.clone(),
                speaker_id: peer_id.clone(),
                speaker_name: auth.display_name.clone(),
                speaker_lang: auth.lang.clone(),
                speaker_user_id: auth.user_id,
                call_session,
                voice_id,
                out: out_tx.clone(),
            }
            .run(rx),
        );
        Some(tx)
    } else {
        None
    };
    let mut frame_gate = FrameGate::new(tokio::time::Instant::now());
    let mut partial_gate =
        FrameGate::with_rate(enhanced::PARTIALS_PER_SEC, tokio::time::Instant::now());
    let mut enhanced_notice = Notifier::default();
    let mut fallback_notice = Notifier::default();
    let mut told_not_enhanced = false;
    let mut echo_dropped: u64 = 0;
    // The meter running is the Enhanced "started" state (there is no engine session to hold it).
    let mut enhanced_meter = false;

    // ---- per-socket state ------------------------------------------------------------
    let mut audio_tx: Option<mpsc::Sender<Vec<u8>>> = None;
    let mut meter_cancel: Option<oneshot::Sender<()>> = None;
    let mut usage_session: Option<Uuid> = None;
    let (end_tx, mut end_rx) = mpsc::unbounded_channel::<EndCause>();
    // Frames forwarded to the engine, for the meter (billing follows audio, not flags).
    let sent = Arc::new(AtomicU64::new(0));
    let mut muted = state.live.is_muted(auth.session_id, auth.peer_id);
    if muted {
        // A reconnecting phone is told it is still muted, so its UI cannot show it live.
        let _ = out_tx.send(
            ServerMessage::PeerMuted {
                peer_id: peer_id.clone(),
                kind: "audio".into(),
                muted: true,
            }
            .to_json(),
        );
    }
    let mut gate = AudioGate::new(
        state.live.config.audio_max_bytes_per_sec,
        tokio::time::Instant::now(),
    );
    let mut msg_count: u32 = 0;
    let mut msg_window = tokio::time::Instant::now();
    let mut last_activity = tokio::time::Instant::now();
    let mut idle_check = tokio::time::interval(WS_PING_INTERVAL);
    idle_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // How often a connected phone re-checks that its conversation is still open (default 3 s, see
    // `LiveConfig::status_check_ms`). The database is the source of truth for "has it ended", so no
    // in-memory registry has to agree with it.
    let mut status_check =
        tokio::time::interval(Duration::from_millis(state.live.config.status_check_ms));
    status_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut end_notice: Option<ServerMessage> = None;
    let ceiling = chrono::Duration::minutes(state.live.config.max_session_min);

    loop {
        tokio::select! {
            incoming = ws_rx.next() => {
                let Some(Ok(msg)) = incoming else { break };
                last_activity = tokio::time::Instant::now();

                // Message-rate cap (audio at ~10 frames/s is far inside it).
                if msg_window.elapsed() > WS_MSG_WINDOW {
                    msg_window = tokio::time::Instant::now();
                    msg_count = 0;
                }
                msg_count += 1;
                if msg_count > WS_MSG_MAX {
                    tracing::warn!(session = %auth.session_id, "live: message rate cap exceeded");
                    break;
                }

                match msg {
                    Message::Binary(data) => {
                        if muted {
                            continue;
                        }
                        let now = tokio::time::Instant::now();
                        if data.len() > MAX_AUDIO_FRAME_BYTES {
                            // Never forwarded, however the sender packs it. Said once a second
                            // at most (below), not per frame.
                            gate.refuse_oversized(data.len());
                        } else if let Some(tx) = audio_tx.as_ref() {
                            // Faster than real time is dropped, counted and reported below.
                            if gate.admit(data.len(), now) {
                                match tx.try_send(data.to_vec()) {
                                    // A momentarily slow engine drops a 100 ms chunk rather than
                                    // ending the session: on a bad network that is a glitch, not
                                    // an outage.
                                    Ok(()) => {
                                        sent.fetch_add(1, Ordering::Relaxed);
                                        let level = mean_abs(&data);
                                        latency.mic_frame(seat, now, level, level >= VOICED_MEAN_ABS);
                                        if utterance_onset(&mut last_voiced, &data, now) {
                                            latency.speech_started(seat, now);
                                        }
                                    }
                                    Err(mpsc::error::TrySendError::Full(_)) => {}
                                    Err(mpsc::error::TrySendError::Closed(_)) => {
                                        audio_tx = None;
                                        // Say so. `/ws` drops the sender silently and the user
                                        // just stops being translated.
                                        let _ = out_tx.send(ServerMessage::Error {
                                            message: "translation stopped".into(),
                                            code: Some("audio_session_closed".into()),
                                        }.to_json());
                                    }
                                }
                            }
                        }
                        if let Some((frames, bytes, oversized)) = gate.take_report(now) {
                            sessions::record_event(
                                &pool,
                                auth.session_id,
                                "diagnostics",
                                json!({
                                    "audio_dropped": { "frames": frames, "bytes": bytes, "oversized": oversized },
                                }),
                            )
                            .await;
                            if oversized > 0 {
                                let _ = out_tx.send(ServerMessage::Error {
                                    message: "audio frame too large".into(),
                                    code: Some("frame_too_large".into()),
                                }.to_json());
                            }
                        }
                    }
                    Message::Text(text) => {
                        if text.len() > MAX_FRAME_BYTES {
                            continue;
                        }
                        // Enhanced's two frames first: Meet's `translate_text` / `enhanced_fallback`
                        // have other fields and mean nothing here.
                        if let Some(frame) = EnhancedFrame::parse(&text) {
                            let still_enhanced = is_text_mode(&state, &auth);
                            match frame {
                                EnhancedFrame::TranslateText { segment_id, text: body, is_final, .. } => {
                                    if !still_enhanced {
                                        // Standard (or fallen back): nothing translates text here.
                                        if !std::mem::replace(&mut told_not_enhanced, true) {
                                            let _ = out_tx.send(ServerMessage::Error {
                                                message: "this conversation does not use text translation".into(),
                                                code: Some("enhanced_not_active".into()),
                                            }.to_json());
                                        }
                                        continue;
                                    }
                                    // Only a started, unmuted seat speaks.
                                    if muted || !enhanced_meter {
                                        continue;
                                    }
                                    let body = body.trim();
                                    if body.is_empty() || !enhanced::valid_segment_id(&segment_id) {
                                        continue;
                                    }
                                    // An interim caption is only the newest words: cut to the last
                                    // few hundred characters instead of refused, so it can never
                                    // cost more than that at the paid translation hop.
                                    let body = if is_final { body } else { enhanced::clip_partial(body) };
                                    if body.chars().count() > enhanced::MAX_TEXT_CHARS {
                                        if enhanced_notice.due(tokio::time::Instant::now()) {
                                            let _ = out_tx.send(ServerMessage::Error {
                                                message: "text too long".into(),
                                                code: Some("text_too_long".into()),
                                            }.to_json());
                                        }
                                        continue;
                                    }
                                    let now = tokio::time::Instant::now();
                                    // Interim captions have a smaller allowance than finals, and
                                    // are checked first so a dropped one costs a final nothing.
                                    if !is_final && !partial_gate.admit(now) {
                                        continue;
                                    }
                                    if !frame_gate.admit(now) {
                                        continue;
                                    }
                                    if state.live.config.echo_filter
                                        && latency.text_echo(seat, &auth.lang, now, body, state.live.config.echo_sim)
                                    {
                                        // What another seat sent first: this phone heard it, it did
                                        // not say it. Counted, never logged, never billed.
                                        crate::metrics::record_live_echo_suppressed();
                                        echo_dropped += 1;
                                        if latency.allow_echo_report(now) {
                                            let n = std::mem::take(&mut echo_dropped);
                                            sessions::record_event(&pool, auth.session_id, "diagnostics",
                                                json!({ "echo_filter": {
                                                    "suppressed": n, "failed_open": 0, "text": true,
                                                } })).await;
                                        }
                                        continue;
                                    }
                                    // Accepted: this is what the meter bills.
                                    sent.fetch_add(1, Ordering::Relaxed);
                                    if let Some(tx) = text_tx.as_ref() {
                                        let seg = Segment { segment_id, text: body.to_string(), is_final };
                                        if tx.try_send(seg).is_err()
                                            && enhanced_notice.due(tokio::time::Instant::now())
                                        {
                                            let _ = out_tx.send(enhanced::translation_unavailable());
                                        }
                                    }
                                }
                                EnhancedFrame::Fallback { .. } if auth.role != PeerRole::Host => {
                                    // The whole conversation's tier and the host's voice choice
                                    // are the host's, and one guest's blocked network must not
                                    // take them from everybody. A guest that cannot reach Cartesia
                                    // keeps its captions; it just cannot be spoken to. Told at
                                    // most once in a while, never a stream of events.
                                    if fallback_notice.due(tokio::time::Instant::now()) {
                                        let _ = out_tx.send(ServerMessage::Error {
                                            message: "only the host can change the tier".into(),
                                            code: Some("fallback_not_allowed".into()),
                                        }.to_json());
                                    }
                                }
                                EnhancedFrame::Fallback { .. } => {
                                    // The host's phone cannot reach Cartesia (token refused,
                                    // blocked network). The WHOLE conversation moves to Standard,
                                    // once: every phone, not just this one, because they all
                                    // depend on each other's text and speech.
                                    if still_enhanced {
                                        downgrade_to_default(&state, &auth, &room,
                                            crate::engine::CARTESIA_ID, "enhanced_unavailable").await;
                                    }
                                    // This phone's Enhanced meter ends now; it re-starts on
                                    // Standard when the client sends `start` again.
                                    if enhanced_meter {
                                        enhanced_meter = false;
                                        if let Some(c) = meter_cancel.take() { let _ = c.send(()); }
                                    }
                                }
                            }
                            continue;
                        }
                        let Ok(cmd) = serde_json::from_str::<ClientMessage>(&text) else { continue };
                        match cmd {
                            ClientMessage::Start => {
                                // A phone whose Enhanced meter outlived the fallback starts again
                                // on the tier that now serves it.
                                if enhanced_meter && !is_text_mode(&state, &auth) {
                                    enhanced_meter = false;
                                    if let Some(c) = meter_cancel.take() { let _ = c.send(()); }
                                }
                                if audio_tx.is_some() || meter_cancel.is_some() {
                                    continue; // already speaking: Start is idempotent
                                }
                                last_voiced = None;
                                match start_speaking(&state, &auth, &room, call_session,
                                    transcripts.clone(), participant_row, &end_tx, &sent,
                                    &mut usage_session).await
                                {
                                    Ok((tx, cancel)) => {
                                        enhanced_meter = tx.is_none();
                                        audio_tx = tx;
                                        meter_cancel = Some(cancel);
                                    }
                                    Err(code) => {
                                        let _ = out_tx.send(ServerMessage::Error {
                                            message: "could not start translation".into(),
                                            code: Some(code.into()),
                                        }.to_json());
                                    }
                                }
                            }
                            ClientMessage::Stop => {
                                audio_tx = None;
                                enhanced_meter = false;
                                if let Some(c) = meter_cancel.take() { let _ = c.send(()); }
                            }
                            ClientMessage::MuteAudio { muted: m } => {
                                // Enforced here, not just relayed: a muted phone sends nothing
                                // to the engine, whatever the client keeps capturing.
                                muted = m;
                                last_voiced = None;
                                state.live.set_muted(auth.session_id, auth.peer_id, m);
                                state.rooms.broadcast_except(&room, &peer_id,
                                    &ServerMessage::PeerMuted {
                                        peer_id: peer_id.clone(),
                                        kind: "audio".into(),
                                        muted: m,
                                    }.to_json());
                            }
                            // Everything else a meeting client may send (chat, WebRTC
                            // signalling, whiteboard, …) has no meaning here.
                            _ => {}
                        }
                    }
                    Message::Close(_) => break,
                    // Pings are answered by the stack; any frame already counted as activity.
                    _ => {}
                }
            }

            Some(cause) = end_rx.recv() => {
                // The conversation cannot continue: the payer ran out, or charging kept
                // failing. Both phones are told, and it ends cleanly.
                let reason = match cause {
                    EndCause::BalanceExhausted => {
                        state.rooms.broadcast(&room, &ServerMessage::BalanceExhausted.to_json());
                        "balance_exhausted"
                    }
                    EndCause::BillingError => "billing_error",
                };
                if matches!(sessions::end_session(&pool, auth.session_id, reason).await, Ok(true)) {
                    super::finalize::on_session_ended(&state, auth.session_id);
                }
                sessions::record_event(&pool, auth.session_id, "ended_by_billing", json!({ "reason": reason })).await;
                end_notice = Some(ServerMessage::SessionState {
                    status: "ended".into(),
                    reason: Some(reason.into()),
                });
                break;
            }

            _ = out_overflow.notified() => {
                // A stalled reader: drop it (it reconnects with its token) rather than queue.
                tracing::warn!(session = %auth.session_id, "live: slow consumer dropped");
                sessions::record_event(&pool, auth.session_id, "slow_consumer", json!({})).await;
                break;
            }

            _ = idle_check.tick() => {
                if last_activity.elapsed() > WS_IDLE_TIMEOUT {
                    sessions::record_event(&pool, auth.session_id, "idle_timeout", json!({})).await;
                    break;
                }
            }

            _ = status_check.tick() => {
                // The conversation fell back to Standard while this phone's Enhanced meter was
                // running (another seat asked for it): stop billing at the Enhanced rate. The
                // client re-starts on Standard when it sees `engine_downgraded`.
                if enhanced_meter && !is_text_mode(&state, &auth) {
                    enhanced_meter = false;
                    if let Some(c) = meter_cancel.take() { let _ = c.send(()); }
                }
                if Utc::now() - auth.created_at >= ceiling
                    && matches!(sessions::end_session(&pool, auth.session_id, "max_duration").await, Ok(true))
                {
                    super::finalize::on_session_ended(&state, auth.session_id);
                }
                // Ended by the other phone, the sweeper, an admin or the ceiling above.
                if sessions::authorize_peer(&pool, &super::tokens::GuestClaims::new(
                    auth.session_id, auth.peer_id, auth.role, i64::MAX)).await.is_err()
                {
                    let reason: Option<String> = sqlx::query_scalar(
                        "SELECT end_reason FROM live_sessions WHERE id = $1")
                        .bind(auth.session_id).fetch_optional(&pool).await.ok().flatten();
                    end_notice = Some(ServerMessage::SessionState {
                        status: "ended".into(),
                        reason,
                    });
                    break;
                }
            }
        }
    }

    // ---- teardown --------------------------------------------------------------------
    state.live.clear_owner(auth.session_id, conn);
    drop(audio_tx);
    drop(text_tx);
    if let Some(c) = meter_cancel.take() {
        let _ = c.send(());
    }
    if let (Some(id), Some(svc)) = (usage_session, state.billing.as_ref()) {
        let _ = svc.finalize_session(id).await;
    }
    if let (Some(row), Some(t)) = (participant_row, transcripts.as_ref()) {
        let _ = t.participant_left(row).await;
    }
    if let Some(notice) = end_notice {
        let _ = out_tx.send(notice.to_json());
    }
    match state.rooms.remove(&room, &peer_id, conn) {
        LeaveOutcome::Left(emptied) => {
            if let (Some(sid), Some(t)) = (emptied, transcripts.as_ref()) {
                let _ = t.finalize_session(sid).await;
            }
            state.rooms.broadcast(
                &room,
                &ServerMessage::PeerLeft {
                    peer_id: peer_id.clone(),
                }
                .to_json(),
            );
        }
        // A newer socket for this same seat took over; it owns the room entry now.
        LeaveOutcome::Superseded => {}
        LeaveOutcome::LeftPhoneOnly(_) => {}
    }
    sessions::record_event(
        &pool,
        auth.session_id,
        "disconnected",
        json!({ "role": auth.role }),
    )
    .await;

    // Let the writer flush the end-of-conversation notice, then let go of the socket.
    drop(out_tx);
    if tokio::time::timeout(FLUSH_GRACE, send_task).await.is_err() {
        tracing::debug!("live: writer did not flush in time");
    }
}

/// The engine serving this conversation right now: the tier it was created on, or the default
/// once a capacity fallback moved the whole conversation there. Every seat asks the same
/// question, so all of them run on one tier.
fn session_engine(state: &AppState, auth: &PeerAuth) -> Arc<dyn crate::engine::TranslationEngine> {
    tiers::resolve(
        &state.engines,
        auth.engine_id.as_deref(),
        state.live.is_downgraded(auth.session_id),
    )
}

/// Whether this conversation runs on Enhanced text right now: it was created on `cartesia`, the
/// tier is still registered, and no seat has asked for the fallback yet. Read live (the fallback
/// is a process-wide fact every seat sees), not cached on the socket.
fn is_text_mode(state: &AppState, auth: &PeerAuth) -> bool {
    tiers::is_enhanced(
        &state.engines,
        auth.engine_id.as_deref(),
        state.live.is_downgraded(auth.session_id),
    )
}

/// Move the whole conversation from `from` to the default engine and say so. Returns the engine
/// to retry on. `reason` is the machine code the clients word their notice with:
/// `premium_at_capacity` (Premium has no free upstream session) or `enhanced_unavailable` (a phone
/// cannot reach Cartesia).
///
/// Every seat that starts speaking afterwards uses the default too (the override is shared), the
/// stored tier follows so a reconnect agrees, and the room gets the same `engine_downgraded` frame
/// Meet sends: the client shows a notice and nothing else changes for it, because Live Connect
/// always captures PCM16 and both tiers take it. Only the call that made the change announces it,
/// so two seats hitting the limit together produce one notice, not two.
async fn downgrade_to_default(
    state: &AppState,
    auth: &PeerAuth,
    room: &str,
    from: &str,
    reason: &str,
) -> Arc<dyn crate::engine::TranslationEngine> {
    let fallback = state.engines.default();
    let to = fallback.metadata().id.clone();
    if state.live.downgrade_session(auth.session_id) {
        // From here every seat is on the default engine: the audio-level echo filter applies again.
        state.live.latency_for(auth.session_id).set_text_mode(false);
        if let Some(pool) = state.pool.as_ref() {
            if let Err(e) = sessions::set_engine(pool, auth.session_id, &to).await {
                tracing::warn!(session = %auth.session_id, error = %e, "live: could not store the fallback tier");
            }
            sessions::record_event(
                pool,
                auth.session_id,
                "engine_downgraded",
                json!({ "from": from, "to": to, "reason": reason, "peer": auth.peer_id }),
            )
            .await;
        }
        state.rooms.broadcast(
            room,
            &ServerMessage::EngineDowngraded {
                peer_id: auth.peer_id.to_string(),
                from: from.to_string(),
                to,
                reason: reason.to_string(),
            }
            .to_json(),
        );
    }
    fallback
}

/// Open this phone's engine session and the meter that bills it to the owner.
///
/// Returns the audio sender and the meter's cancel handle, or a stable error code for the
/// client. Nothing is started unless somebody is paying: a conversation that cannot be metered
/// does not get free translation.
///
/// On Enhanced there is no engine session and no audio: the sender is `None` and the meter alone
/// is the "started" state, billing at the Enhanced rate for the `translate_text` frames the socket
/// accepts.
#[allow(clippy::too_many_arguments)] // one cohesive start-of-speech context
async fn start_speaking(
    state: &AppState,
    auth: &PeerAuth,
    room: &str,
    call_session: Uuid,
    transcripts: Option<crate::transcripts::TranscriptService>,
    participant_row: Option<Uuid>,
    end_tx: &mpsc::UnboundedSender<EndCause>,
    sent: &Arc<AtomicU64>,
    usage_session: &mut Option<Uuid>,
) -> Result<(Option<mpsc::Sender<Vec<u8>>>, oneshot::Sender<()>), &'static str> {
    let pool = state.pool.as_ref().ok_or("unavailable")?;
    let billing = state.billing.as_ref().ok_or("billing_unavailable")?;
    // Resolved per Start, not per socket: a seat that connected before the conversation fell
    // back to the default must not start on the tier that just ran out.
    let text_mode = is_text_mode(state, auth);
    let mut engine = if text_mode {
        // Enhanced: the metadata-only engine supplies the rate and the usage row's tier.
        state
            .engines
            .get(crate::engine::CARTESIA_ID)
            .ok_or("unavailable")?
    } else {
        session_engine(state, auth)
    };

    // Who pays, checked again NOW: the balance may have moved since the session was created.
    let payer = match (auth.org_id, auth.owner_user_id) {
        (Some(org_id), _) => {
            if let Some(_why) = crate::business::ws_gate::check(pool, org_id)
                .await
                .map_err(|_| "unavailable")?
            {
                return Err("insufficient_credits");
            }
            LivePayer::Org {
                org_id,
                call_session,
            }
        }
        (None, Some(owner)) => {
            // Scales with the tier's rate: Premium needs the same runway as Standard's minimum.
            if !owner_can_afford(state, owner, &*engine)
                .await
                .map_err(|_| "unavailable")?
            {
                return Err("insufficient_balance");
            }
            if usage_session.is_none() {
                *usage_session = billing
                    .create_session(owner, room, engine.metadata().id.as_str())
                    .await
                    .ok();
            }
            let usage_session = usage_session.ok_or("unavailable")?;
            LivePayer::User {
                user_id: owner,
                usage_session,
            }
        }
        (None, None) => return Err("unavailable"),
    };
    let pricing = &state
        .config
        .billing
        .as_ref()
        .ok_or("billing_unavailable")?
        .pricing;

    let audio_tx = if text_mode {
        None
    } else {
        let build_ctx = || SpeakerCtx {
            room: room.to_string(),
            speaker_id: auth.peer_id.to_string(),
            speaker_name: auth.display_name.clone(),
            speaker_lang: auth.lang.clone(),
            session_id: call_session,
            speaker_user_id: auth.user_id,
            glossary: None,
            segmentation: None,
        };
        let build_deps = || SessionDeps {
            rooms: state.rooms.clone(),
            moderator: state.moderator.clone(),
            transcripts: transcripts.clone(),
            participant_row,
            listener_pays: false,
            translator: state.translator.clone(),
            transcript_writer: TranscriptWriter::default(),
        };
        let mut outcome = engine.start_session(build_ctx(), build_deps()).await;
        // Capacity fallback (spec 0094, as Meet does it, but for the whole conversation): the tier
        // has no free upstream session, so everybody moves to the default engine and translation
        // never stops. The default never reports `AtCapacity` (it skips what it cannot start), so a
        // fallback never reaches the client as `live_busy`; the default itself at capacity does, as
        // it always has.
        if matches!(outcome, SessionOutcome::AtCapacity)
            && engine.metadata().id != state.engines.default().metadata().id
        {
            let from = engine.metadata().id.clone();
            engine = downgrade_to_default(state, auth, room, &from, "premium_at_capacity").await;
            outcome = engine.start_session(build_ctx(), build_deps()).await;
        }
        let audio_tx = match outcome {
            SessionOutcome::Started(tx) => tx,
            SessionOutcome::AtCapacity => return Err("live_busy"),
            SessionOutcome::Failed => return Err("engine_unavailable"),
        };
        // The usage row was tagged before we knew who would serve; keep it honest.
        if let Some(usage) = usage_session.as_ref() {
            let _ = sqlx::query(
                "UPDATE usage_sessions SET engine_id = $2 WHERE id = $1 AND engine_id IS DISTINCT FROM $2",
            )
            .bind(*usage)
            .bind(engine.metadata().id.as_str())
            .execute(pool)
            .await;
        }
        Some(audio_tx)
    };

    // Nobody translates for free: the meter is part of starting, not an afterthought. Balance
    // updates reach the owner's phone only — the guest never sees someone else's money.
    let (cancel, cancel_rx) = oneshot::channel();
    tokio::spawn(
        LiveMeter {
            pool: pool.clone(),
            live: state.live.clone(),
            rooms: state.rooms.clone(),
            payer,
            session: auth.session_id,
            peer: auth.peer_id,
            room: room.to_string(),
            speaker_lang: auth.lang.clone(),
            interval_secs: pricing.usage_update_interval,
            rate_per_second: engine.metadata().user_rate_per_second(),
            low_balance_threshold: pricing.low_balance_threshold,
            activity: sent.clone(),
        }
        .run(end_tx.clone(), cancel_rx),
    );
    Ok((audio_tx, cancel))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(sample: i16, samples: usize) -> Vec<u8> {
        std::iter::repeat_n(sample.to_le_bytes(), samples)
            .flatten()
            .collect()
    }

    #[test]
    fn digital_silence_and_a_quiet_hiss_are_not_speech_but_a_voice_is() {
        assert!(!is_voiced(&[]));
        assert!(!is_voiced(&pcm(0, 2400)));
        assert!(!is_voiced(&pcm(20, 2400)), "a hiss well below speech");
        assert!(is_voiced(&pcm(1000, 2400)));
        assert!(is_voiced(&pcm(-1000, 2400)), "sign does not matter");
    }

    #[test]
    fn an_utterance_starts_at_the_first_voiced_frame_and_again_after_a_quiet_gap() {
        let t0 = tokio::time::Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let (voice, silence) = (pcm(1000, 2400), pcm(0, 2400));
        let mut last = None;

        // Silence after a Start is not an onset, however long it lasts.
        assert!(!utterance_onset(&mut last, &silence, ms(0)));
        assert!(!utterance_onset(&mut last, &silence, ms(3000)));
        // The first voiced frame is, and the rest of the same utterance is not.
        assert!(utterance_onset(&mut last, &voice, ms(3100)));
        assert!(!utterance_onset(&mut last, &voice, ms(3200)));
        // A short pause inside a sentence does not split it.
        assert!(!utterance_onset(&mut last, &silence, ms(3300)));
        assert!(!utterance_onset(&mut last, &voice, ms(3800)));
        // A pause of a second or more does.
        assert!(utterance_onset(&mut last, &voice, ms(4800)));
        // A Start (or unmute) clears the memory, so the next voiced frame is an onset again.
        last = None;
        assert!(utterance_onset(&mut last, &voice, ms(4900)));
    }

    /// One 100 ms frame of PCM16 mono at 24 kHz.
    const HONEST_FRAME: usize = 4_800;

    #[test]
    fn a_client_sending_real_time_audio_is_never_throttled() {
        let t0 = tokio::time::Instant::now();
        let mut gate = AudioGate::new(72_000, t0);
        // A minute of one frame every 100 ms, with the usual scheduling jitter.
        for i in 0..600u64 {
            let jitter = (i * 37) % 40; // 0-39 ms late
            assert!(
                gate.admit(HONEST_FRAME, t0 + Duration::from_millis(i * 100 + jitter)),
                "frame {i} dropped"
            );
        }
        assert_eq!(gate.take_report(t0 + Duration::from_secs(61)), None);
    }

    #[test]
    fn a_stall_that_delivers_a_second_of_audio_at_once_still_fits() {
        let t0 = tokio::time::Instant::now();
        let mut gate = AudioGate::new(72_000, t0);
        for i in 0..20u64 {
            assert!(gate.admit(HONEST_FRAME, t0 + Duration::from_millis(i * 100)));
        }
        // The network holds everything for 800 ms, then hands over eight frames together.
        let after = t0 + Duration::from_millis(2_000 + 800);
        for i in 0..8 {
            assert!(gate.admit(HONEST_FRAME, after), "burst frame {i} dropped");
        }
    }

    #[test]
    fn audio_faster_than_real_time_is_dropped_down_to_the_budget() {
        let t0 = tokio::time::Instant::now();
        let mut gate = AudioGate::new(72_000, t0);
        // 70x real time for ten seconds: 7 000 ms of audio per 100 ms of wall clock.
        let mut forwarded = 0usize;
        for i in 0..1_000u64 {
            let at = t0 + Duration::from_millis(i * 10);
            // 7 frames per 10 ms = 70 frames of audio per 100 ms.
            for _ in 0..7 {
                if gate.admit(HONEST_FRAME, at) {
                    forwarded += HONEST_FRAME;
                }
            }
        }
        // Budget over 10 s: the rate plus the one-second burst it started with.
        let ceiling = 72_000 * 10 + 72_000;
        assert!(forwarded <= ceiling, "{forwarded} bytes got through");
        assert!(
            forwarded >= 72_000 * 10,
            "an honest ceiling, not a starved one: {forwarded}"
        );
    }

    #[test]
    fn throttling_is_reported_at_most_once_a_second_and_a_bounded_number_of_times() {
        let t0 = tokio::time::Instant::now();
        let mut gate = AudioGate::new(24_000, t0);
        assert_eq!(gate.take_report(t0), None, "nothing dropped, nothing said");
        for _ in 0..30 {
            gate.admit(HONEST_FRAME, t0);
        }
        gate.refuse_oversized(60_000);
        let (frames, bytes, oversized) = gate.take_report(t0).expect("the first report is due");
        assert!(frames > 1 && bytes > 60_000 && oversized == 1);
        // Drops go on, but not another word within the second...
        gate.admit(HONEST_FRAME, t0 + Duration::from_millis(500));
        gate.refuse_oversized(60_000);
        assert_eq!(gate.take_report(t0 + Duration::from_millis(999)), None);
        // ...and the report that follows carries only what happened since.
        let (frames, _, oversized) = gate.take_report(t0 + Duration::from_secs(1)).unwrap();
        assert!(frames >= 1 && oversized == 1);
        // A socket that never stops cannot write more than the bound.
        let mut reports = 2;
        for s in 2..60u64 {
            gate.refuse_oversized(60_000);
            if gate.take_report(t0 + Duration::from_secs(s)).is_some() {
                reports += 1;
            }
        }
        assert_eq!(reports, MAX_THROTTLE_REPORTS);
    }

    #[test]
    fn the_speaker_of_an_audio_frame_is_read_from_the_frame_the_protocol_really_emits() {
        let l = SessionLatency::new_for_test();
        let (host, g1, g2) = (l.seat_for("host-peer"), l.seat_for("g1"), l.seat_for("g2"));
        let audio = |who: &str| {
            ServerMessage::TranslatedAudio {
                speaker_id: who.into(),
                lang: "es".into(),
                seq: 1,
                pcm16_b64: "AA==".into(),
            }
            .to_json()
        };
        assert_eq!(speaker_seat(&l, &audio("host-peer")), Some(host));
        assert_eq!(speaker_seat(&l, &audio("g1")), Some(g1));
        assert_eq!(speaker_seat(&l, &audio("g2")), Some(g2));
        // A speaker whose phone never connected, and text that is not an audio frame at all, take
        // no sample rather than guessing a seat.
        assert_eq!(speaker_seat(&l, &audio("nobody")), None);
        assert_eq!(speaker_seat(&l, r#"{"type":"translated_audio"}"#), None);
        assert_eq!(speaker_seat(&l, "{}"), None);
    }

    #[test]
    fn the_prefix_the_pump_looks_for_is_what_the_protocol_really_emits() {
        let frame = ServerMessage::TranslatedAudio {
            speaker_id: "s".into(),
            lang: "es".into(),
            seq: 1,
            pcm16_b64: "AA==".into(),
        }
        .to_json();
        assert!(frame.starts_with(TRANSLATED_AUDIO_PREFIX), "{frame}");
        let other = ServerMessage::PeerLeft {
            peer_id: "p".into(),
        }
        .to_json();
        assert!(!other.starts_with(TRANSLATED_AUDIO_PREFIX));
    }
}
