//! **Talk to Anyone** — two people, one device, one microphone (spec 0110).
//!
//! ## Why this is a room, not a new pipeline
//!
//! VoxTranslate's model is a room of symmetric peers, each with one `lang` that is both
//! what they speak and what they hear. A face-to-face conversation on one phone is
//! asymmetric: one audio SOURCE carrying two speakers, and two LISTENERS who happen to
//! be the same pair of ears. [`crate::extension`] already solved the two-peer version of
//! this; Talk to Anyone is the same trick with a second listener:
//!
//! ```text
//!   source peer   id = "<sid>-src"     lang = "auto"    owns the engine session
//!   listener USER id = "<sid>"         lang = <user>    the signed-in user's language
//!   listener OTHER id = "<sid>-other"  lang = <other>   the person opposite them
//! ```
//!
//! Everything then falls out of machinery that already exists and is already tested:
//! the engine sees two target languages and opens one upstream session for each
//! ([`crate::engine::standard`] and friends), `reconcile_langs` keeps them alive, the
//! moderator runs, and the meter bills two streams. **No engine is modified.**
//!
//! ## What is genuinely new: deciding the direction
//!
//! A `lang = "auto"` speaker is translated into EVERY room language, so each sentence
//! comes back twice — once translated, and once as a source→source echo. Playing the
//! echo would speak the user's own words back at them through the phone's speaker, which
//! is both useless and the first turn of the feedback loop in brief §9.
//!
//! No provider will tell us which language was spoken (see [`direction`]), so we decide
//! it ourselves from the original transcript every engine already returns, and
//! [`utterance`] holds the translated audio for the few hundred milliseconds that takes.
//! One implementation covers Standard, Pro and Premium because it never touches a
//! provider — only the room's own frames.
//!
//! The room is always `Private`, so it is never listed and never joinable.

pub mod direction;
pub mod utterance;

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::deepgram::SpeakerCtx;
use crate::engine::{SessionDeps, SessionOutcome};
use crate::protocol::ServerMessage;
use crate::rooms::{Peer, PeerTx, Visibility, OUT_CHANNEL_CAP};
use crate::usage::{run_usage_meter, MeterConfig, MeterScope};
use crate::AppState;

use direction::{Direction, Resolver};
use utterance::{FrameKind, Outcome, Utterance};

/// The source peer's language. Never a real code: the engine must identify each
/// utterance itself, which is what makes [`crate::engine::qwen::session_update_json`]
/// omit the source field instead of defaulting it to English.
const SOURCE_LANG: &str = "auto";

/// Query parameters for `GET /ws/talk`.
#[derive(Debug, Clone, Deserialize)]
pub struct TalkParams {
    /// The signed-in user's own language — what they speak and what they want to hear.
    pub lang: String,
    /// The other person's language. The only thing the setup screen asks for.
    pub other: String,
    /// Session JWT. This is a billed feature with no guest tier (same as the extension).
    #[serde(default)]
    pub token: Option<String>,
    /// Chosen engine id; unknown ids fall back to the default.
    #[serde(default)]
    pub engine: Option<String>,
}

/// Accept `a`–`z`, digits and `-`, 1..=8 chars — the same rule the room `/ws` route
/// applies. Kept identical because these values reach provider URLs and prompts.
fn valid_lang(code: &str) -> bool {
    (1..=8).contains(&code.len()) && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// `GET /ws/talk` — upgrade a face-to-face conversation session.
pub async fn ws_talk(
    ws: WebSocketUpgrade,
    Query(params): Query<TalkParams>,
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Response {
    let user_lang = params.lang.trim().to_lowercase();
    let other_lang = params.other.trim().to_lowercase();

    if !valid_lang(&user_lang) || !valid_lang(&other_lang) {
        return (StatusCode::BAD_REQUEST, "invalid language code").into_response();
    }
    // Both ends must be real languages. `auto` belongs to the source pseudo-peer only —
    // as a listener language it would make the fan-out skip that side entirely and half
    // the conversation would silently produce nothing.
    if user_lang == SOURCE_LANG || other_lang == SOURCE_LANG {
        return (StatusCode::BAD_REQUEST, "language cannot be auto").into_response();
    }
    // Same language on both sides is not a conversation to translate; it would also make
    // the room hold ONE target and every utterance an echo.
    if user_lang == other_lang {
        return (
            StatusCode::BAD_REQUEST,
            "the two languages must be different",
        )
            .into_response();
    }

    let ip = crate::observability::client_ip(&headers);
    if !state.rate_limiter.allow(
        &format!("talkws:{ip}"),
        state.ws_connect_max,
        Duration::from_secs(60),
    ) {
        return (StatusCode::TOO_MANY_REQUESTS, "too many connections").into_response();
    }

    let params = TalkParams {
        lang: user_lang,
        other: other_lang,
        ..params
    };
    ws.on_upgrade(move |socket| handle_talk_session(socket, params, state))
}

/// Everything one conversation owns, so teardown is a single place. A leaked peer keeps
/// a meter alive for a user who has walked away.
struct SessionGuard {
    rooms: Arc<crate::rooms::RoomManager>,
    room: String,
    peer_ids: Vec<String>,
    conn: Uuid,
    billing: Option<crate::billing::BillingService>,
    usage_session: Option<Uuid>,
}

impl SessionGuard {
    async fn finish(self, meter_cancel: Option<oneshot::Sender<()>>) {
        if let Some(cancel) = meter_cancel {
            let _ = cancel.send(());
        }
        for id in &self.peer_ids {
            self.rooms.remove(&self.room, id, self.conn);
        }
        if let (Some(svc), Some(sid)) = (self.billing.as_ref(), self.usage_session) {
            if let Err(e) = svc.finalize_session(sid).await {
                tracing::error!("talk finalize session failed: {e}");
            }
        }
    }
}

/// A classification that came back from a spawned task, tagged with the utterance it was
/// asked about. Verdicts are slower than speech, so one can easily arrive after its
/// sentence ended — `generation` is what stops it being applied to the next one.
struct Verdict {
    generation: u64,
    /// `Err` = the classifier itself failed, as opposed to abstaining.
    outcome: Result<Direction, String>,
}

/// Consecutive classifier failures before the client is told. One is a hiccup; three in
/// a row means the provider is down and the user is staring at "Listening…" wondering
/// why nothing happens. Silence is the worst way for this feature to fail — it is
/// indistinguishable from it working.
const MAX_SILENT_CLASSIFY_FAILURES: u32 = 3;

/// Pull the ORIGINAL transcript out of a subtitle frame.
///
/// Both shapes carry it: `subtitle_interim` as the optional `original` alongside the
/// translated `text`, `subtitle_final` as the required `original`. This is the only place
/// a frame is parsed for content, and it runs a few times a second at most — audio
/// frames are classified by prefix and never parsed (see [`FrameKind::classify`]).
fn original_text(frame: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(frame).ok()?;
    let original = value.get("original")?.as_str()?.trim();
    (!original.is_empty()).then(|| original.to_string())
}

async fn handle_talk_session(socket: WebSocket, params: TalkParams, state: AppState) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // --- authenticate ------------------------------------------------------
    let authed = match crate::authorize(&state, params.token.as_deref()).await {
        Ok(Some(peer)) => peer,
        Ok(None) => {
            let _ = ws_tx
                .send(Message::Text(
                    ServerMessage::Error {
                        message: "sign in to start a conversation".to_string(),
                        code: Some("invalid_token".to_string()),
                    }
                    .to_json()
                    .into(),
                ))
                .await;
            return;
        }
        Err(msg) => {
            let _ = ws_tx.send(Message::Text(msg.to_json().into())).await;
            return;
        }
    };

    // --- resolve the engine ------------------------------------------------
    let requested = state.engines.resolve(params.engine.as_deref());
    // A client-direct tier (Cartesia "Enhanced") runs the provider in the BROWSER and the
    // server never sees the audio — so it cannot host this mode, whose whole job is to
    // gate server-side frames. Its selling point is cloning each REMOTE peer's voice,
    // which is meaningless when both people share one microphone. Fall back rather than
    // refuse, and say so, so the user is never left staring at a dead Start button.
    let downgraded_from = requested
        .metadata()
        .capabilities
        .client_direct
        .then(|| requested.metadata().id.clone());
    let engine = match downgraded_from {
        Some(_) => state.engines.default(),
        None => requested,
    };
    let engine_id = engine.metadata().id.clone();
    let rate_per_second = engine.metadata().user_rate_per_second();
    let scale_by_target_count = engine.metadata().capabilities.cost_scales_per_language;
    // Speech-to-speech engines consume PCM16 @ 24 kHz. Feeding one WebM is not a
    // degradation — it reads the bytes as samples and produces silence, with no error.
    let needs_pcm = engine.metadata().capabilities.translated_audio;

    let user_lang = params.lang.clone();
    let other_lang = params.other.clone();

    // --- build the three-peer private room ---------------------------------
    let session_uuid = Uuid::new_v4();
    let room = format!("talk-{session_uuid}");
    let user_id = session_uuid.to_string();
    let other_id = format!("{session_uuid}-other");
    let source_id = format!("{session_uuid}-src");
    let conn = Uuid::new_v4();

    // One channel per peer. `out_tx` is the browser's own socket; the other two are
    // gated by this handler before anything reaches it.
    let (out_tx, out_rx, _out_overflow) = PeerTx::channel(OUT_CHANNEL_CAP);
    let (user_tx, mut user_rx, _user_overflow) = PeerTx::channel(OUT_CHANNEL_CAP);
    let (other_tx, mut other_rx, _other_overflow) = PeerTx::channel(OUT_CHANNEL_CAP);
    let (src_tx, mut src_rx, _src_overflow) = PeerTx::channel(OUT_CHANNEL_CAP);

    let peer = |id: &str, name: &str, lang: &str, tx: PeerTx, speaking: bool| Peer {
        id: id.to_string(),
        conn,
        name: name.to_string(),
        lang: lang.to_string(),
        user_id: Some(authed.user_id),
        engine: engine_id.clone(),
        avatar_url: None,
        cartesia_voice_id: None,
        tx,
        speaking: Arc::new(AtomicBool::new(speaking)),
    };

    let joined = match state.rooms.join(
        &room,
        // NOT `out_tx`: a listener peer wired straight to the browser socket would
        // bypass the gate, and the very first thing through it would be the
        // source→source echo this feature exists to suppress.
        peer(&user_id, "You", &user_lang, user_tx, false),
        Visibility::Private,
    ) {
        Ok(j) => j,
        Err(()) => {
            let _ = ws_tx
                .send(Message::Text(ServerMessage::RoomFull.to_json().into()))
                .await;
            return;
        }
    };
    // The room is brand new and holds one peer against a cap of four, so neither of these
    // can realistically fail — but a half-built room would sit there billing with nothing
    // driving it, so unwind explicitly rather than rely on that reasoning staying true.
    let rest = [
        peer(&other_id, "Them", &other_lang, other_tx, false),
        peer(&source_id, "Microphone", SOURCE_LANG, src_tx, true),
    ];
    for p in rest {
        if state.rooms.join(&room, p, Visibility::Private).is_err() {
            state.rooms.remove(&room, &user_id, conn);
            state.rooms.remove(&room, &other_id, conn);
            let _ = ws_tx
                .send(Message::Text(ServerMessage::RoomFull.to_json().into()))
                .await;
            return;
        }
    }

    // --- billing -----------------------------------------------------------
    let usage_session = match state.billing.as_ref() {
        Some(svc) => match svc.create_session(authed.user_id, &room, &engine_id).await {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::error!("talk usage session create failed: {e}");
                None
            }
        },
        None => None,
    };

    let (exhaust_tx, mut exhaust_rx) = mpsc::unbounded_channel::<()>();

    // Metered per SPEAKING session, not per connection: opening the setup screen and
    // never pressing Start must cost nothing. Speaker scope on the source peer, whose
    // language is `auto` — so `billable_streams` filters nothing and both live
    // translation directions are billed. That is the honest count: two interpreter
    // channels are held open so neither speaker is ever cut off (docs/pricing-talk-to-anyone.md).
    let start_meter = || -> Option<oneshot::Sender<()>> {
        let (svc, sid, cfg) = match (
            state.billing.as_ref(),
            usage_session,
            state.config.billing.as_ref(),
        ) {
            (Some(svc), Some(sid), Some(cfg)) => (svc, sid, cfg),
            _ => return None,
        };
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let meter_cfg = MeterConfig {
            interval_secs: cfg.pricing.usage_update_interval,
            rate_per_second,
            low_balance_threshold: cfg.pricing.low_balance_threshold,
            rooms: Some(state.rooms.clone()),
            room: room.clone(),
            scope: MeterScope::Speaker {
                speaker_id: source_id.clone(),
                speaker_lang: SOURCE_LANG.to_string(),
                scale_by_target_count,
            },
        };
        tokio::spawn(run_usage_meter(
            svc.clone(),
            authed.user_id,
            sid,
            meter_cfg,
            out_tx.clone(),
            exhaust_tx.clone(),
            cancel_rx,
        ));
        Some(cancel_tx)
    };

    let guard = SessionGuard {
        rooms: state.rooms.clone(),
        room: room.clone(),
        peer_ids: vec![user_id.clone(), other_id.clone(), source_id.clone()],
        conn,
        billing: state.billing.clone(),
        usage_session,
    };

    // --- hand the client its contract --------------------------------------
    let _ = out_tx.send(
        ServerMessage::RoomJoined {
            peer_id: user_id.clone(),
            peers: Vec::new(),
            session_id: Some(joined.session_id.to_string()),
            public: false,
        }
        .to_json(),
    );
    let _ = out_tx.send(ServerMessage::CaptureFormat { pcm: needs_pcm }.to_json());
    if let Some(from) = downgraded_from {
        let _ = out_tx.send(
            ServerMessage::EngineDowngraded {
                peer_id: user_id.clone(),
                from,
                to: engine_id.clone(),
                reason: "talk_client_direct_unsupported".to_string(),
            }
            .to_json(),
        );
    }

    let send_task = tokio::spawn(crate::pump_to_ws(out_rx, ws_tx));

    // --- direction resolution ----------------------------------------------
    let resolver = Resolver::new(
        state.groq.clone(),
        state.config.translation_model.clone(),
        user_lang.clone(),
        other_lang.clone(),
    );
    let (verdict_tx, mut verdict_rx) = mpsc::unbounded_channel::<Verdict>();
    let mut utterance = Utterance::new(user_lang.clone(), other_lang.clone());
    // Bumped at every utterance boundary so a verdict that arrives after its sentence
    // ended is discarded instead of steering the next one.
    let mut generation: u64 = 0;
    // Bounded to one in-flight classification: partials arrive faster than the model
    // answers, and a queue of stale questions helps nobody.
    let mut resolving = false;
    // Reset by any success; drives the "provider unavailable" notice.
    let mut classify_failures: u32 = 0;

    // --- session loop ------------------------------------------------------
    let mut audio_tx: Option<mpsc::Sender<Vec<u8>>> = None;
    let mut meter_cancel: Option<oneshot::Sender<()>> = None;
    // Once credits run out the meter task is gone for good; without this a client could
    // send `start` again and get an unmetered conversation.
    let mut exhausted = false;

    loop {
        tokio::select! {
            _ = exhaust_rx.recv() => {
                exhausted = true;
                audio_tx = None;
                meter_cancel = None;
            }

            Some(v) = verdict_rx.recv() => {
                resolving = false;
                let direction = match v.outcome {
                    Ok(d) => {
                        classify_failures = 0;
                        d
                    }
                    Err(_) => {
                        // Every utterance is held until a direction is committed, so a
                        // dead classifier means permanent silence. Say so, rather than
                        // let the UI keep claiming it is listening.
                        classify_failures += 1;
                        if classify_failures == MAX_SILENT_CLASSIFY_FAILURES {
                            let _ = out_tx.send(
                                ServerMessage::Error {
                                    message: "translation is unavailable right now"
                                        .to_string(),
                                    code: Some("provider_unavailable".to_string()),
                                }
                                .to_json(),
                            );
                        }
                        // A parked sentence whose verdict just failed will never be
                        // released. Close it out now, or it holds the next one hostage.
                        if utterance.has_pending_final() {
                            crate::metrics::record_talk_direction_unknown();
                            generation = generation.wrapping_add(1);
                            utterance.reset();
                        }
                        continue;
                    }
                };
                if v.generation != generation {
                    continue;
                }
                if direction == Direction::Unknown {
                    // The model saw the whole sentence and still could not place it — a
                    // third language, or genuinely ambiguous. Nothing is spoken (brief
                    // §6), but the utterance must still be closed out.
                    if utterance.has_pending_final() {
                        crate::metrics::record_talk_direction_unknown();
                        generation = generation.wrapping_add(1);
                        utterance.reset();
                    }
                    continue;
                }
                // `Some` means THIS verdict latched. A late second verdict — the free
                // script path having already decided — returns `None` and is silent, so
                // the direction is announced exactly once per utterance and the resolved
                // counter stays honest.
                let Some(flushed) = utterance.commit(direction) else {
                    continue;
                };
                emit_direction(direction, &resolver, &out_tx);
                for frame in flushed {
                    let _ = out_tx.send(frame);
                }
                // A sentence that finished before its direction did: release it now.
                if let Some(pending) = utterance.take_pending_final() {
                    let _ = out_tx.send(pending);
                    generation = generation.wrapping_add(1);
                    utterance.reset();
                }
            }

            // Frames addressed to the two listener peers. Which language a frame belongs
            // to is known from the channel it arrived on, so nothing has to be parsed to
            // route it.
            Some(frame) = user_rx.recv() => {
                route_frame(
                    &user_lang, frame, &mut utterance, &out_tx, &mut generation,
                    &mut resolving, &resolver, &verdict_tx,
                );
            }
            Some(frame) = other_rx.recv() => {
                route_frame(
                    &other_lang, frame, &mut utterance, &out_tx, &mut generation,
                    &mut resolving, &resolver, &verdict_tx,
                );
            }

            // The source pseudo-peer's channel. Engines address their failures to the
            // SPEAKER — "speech service unavailable" — and the speaker here is a peer with
            // no socket. Forwarding the diagnostics is the difference between a broken
            // session and a silent one; the rest would duplicate what the listeners
            // already carry.
            Some(frame) = src_rx.recv() => {
                if frame.contains(r#""type":"error""#)
                    || frame.contains(r#""type":"moderation_warning""#)
                {
                    let _ = out_tx.send(frame);
                }
            }

            incoming = ws_rx.next() => {
                let Some(Ok(message)) = incoming else { break };
                match message {
                    Message::Binary(bytes) => {
                        if let Some(tx) = audio_tx.as_ref() {
                            // Bounded: if the provider stalls this fills and the session
                            // ends cleanly rather than buffering without limit.
                            if tx.send(bytes.to_vec()).await.is_err() {
                                audio_tx = None;
                            }
                        }
                    }
                    Message::Text(text) => {
                        match serde_json::from_str::<crate::protocol::ClientMessage>(&text) {
                            Ok(crate::protocol::ClientMessage::Start) => {
                                if audio_tx.is_some() {
                                    continue; // a second start is a no-op, not a second session
                                }
                                if exhausted {
                                    let _ = out_tx.send(
                                        ServerMessage::Error {
                                            message: "out of credit".to_string(),
                                            code: Some("insufficient_balance".to_string()),
                                        }
                                        .to_json(),
                                    );
                                    continue;
                                }
                                let ctx = SpeakerCtx {
                                    room: room.clone(),
                                    speaker_id: source_id.clone(),
                                    speaker_name: "Microphone".to_string(),
                                    // Always `auto`: the engine identifies each utterance
                                    // itself, which is the whole premise of this mode.
                                    speaker_lang: SOURCE_LANG.to_string(),
                                    session_id: joined.session_id,
                                    speaker_user_id: Some(authed.user_id),
                                    glossary: None,
                                };
                                let deps = SessionDeps {
                                    rooms: state.rooms.clone(),
                                    moderator: state.moderator.clone(),
                                    // A face-to-face conversation persists nothing. The
                                    // privacy model treats it like the extension, not
                                    // like a recorded call.
                                    transcripts: None,
                                    participant_row: None,
                                    listener_pays: false,
                                    translator: state.translator.clone(),
                                };
                                match engine.start_session(ctx, deps).await {
                                    SessionOutcome::Started(tx) => {
                                        audio_tx = Some(tx);
                                        meter_cancel = start_meter();
                                    }
                                    SessionOutcome::AtCapacity | SessionOutcome::Failed => {
                                        let _ = out_tx.send(
                                            ServerMessage::Error {
                                                message: "translation service is busy"
                                                    .to_string(),
                                                code: Some("provider_unavailable".to_string()),
                                            }
                                            .to_json(),
                                        );
                                    }
                                }
                            }
                            Ok(crate::protocol::ClientMessage::Stop) => {
                                // Dropping the sender flushes and closes the upstream
                                // sessions; billing must not outlive the audio.
                                audio_tx = None;
                                if let Some(cancel) = meter_cancel.take() {
                                    let _ = cancel.send(());
                                }
                                // Whatever was half-said is void, and any verdict still
                                // in flight belongs to a sentence nobody will finish.
                                generation = generation.wrapping_add(1);
                                utterance.reset();
                            }
                            // Every other client message belongs to the call protocol and
                            // is meaningless here; ignoring it is not an error.
                            Ok(_) => {}
                            Err(_) => {}
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    }

    drop(audio_tx);
    send_task.abort();
    guard.finish(meter_cancel).await;
}

/// Gate one listener frame, and keep the direction resolver fed.
///
/// Split out of the `select!` arm so both listener channels share one implementation —
/// two copies of this is exactly how one side of a conversation ends up behaving
/// differently from the other.
#[allow(clippy::too_many_arguments)]
fn route_frame(
    lang: &str,
    frame: String,
    utterance: &mut Utterance,
    out_tx: &PeerTx,
    generation: &mut u64,
    resolving: &mut bool,
    resolver: &Resolver,
    verdict_tx: &mpsc::UnboundedSender<Verdict>,
) {
    let kind = FrameKind::classify(&frame);

    // Subtitles carry the original words — the only evidence we get about which language
    // was just spoken. Read it BEFORE routing, because a final resets the utterance.
    if matches!(kind, FrameKind::SubtitleInterim | FrameKind::SubtitleFinal) {
        if let Some(text) = original_text(&frame) {
            let wants_resolve = utterance.note_original(&text);
            // The free path first: a disjoint-script pair needs no model call at all, and
            // most travel pairs are disjoint. Skipped once latched — re-scoring every
            // partial buys nothing.
            let local = if utterance.direction() == Direction::Unknown {
                resolver.resolve_local(utterance.original())
            } else {
                Direction::Unknown
            };
            if let Some(flushed) = utterance.commit(local) {
                emit_direction(local, resolver, out_tx);
                for f in flushed {
                    let _ = out_tx.send(f);
                }
            } else if wants_resolve && !*resolving {
                *resolving = true;
                let gen = *generation;
                let text = utterance.original().to_string();
                let resolver = resolver.clone();
                let tx = verdict_tx.clone();
                tokio::spawn(async move {
                    let started = std::time::Instant::now();
                    let outcome = resolver.resolve(&text).await;
                    crate::metrics::record_talk_direction_ms(started.elapsed().as_millis() as u64);
                    let _ = tx.send(Verdict {
                        generation: gen,
                        outcome,
                    });
                });
            }
        }
    }

    let final_frame = kind == FrameKind::SubtitleFinal;
    match utterance.on_frame(lang, frame) {
        Outcome::Send(frames) => {
            for f in frames {
                let _ = out_tx.send(f);
            }
        }
        Outcome::Hold => {}
    }

    if !final_frame {
        return;
    }

    // The utterance is over. If the direction is settled, so is the sentence — bump the
    // generation so a verdict still in flight cannot steer the NEXT one.
    if utterance.direction() != Direction::Unknown || !utterance.has_pending_final() {
        *generation = generation.wrapping_add(1);
        return;
    }

    // Unresolved, with the complete sentence parked. This is the last chance to say
    // anything at all about it, so classify the FULL text — ignoring both the growth
    // throttle and the in-flight guard, which exist to avoid paying for every partial and
    // have no business suppressing the one question that matters. The generation is
    // deliberately NOT bumped: the answer is about THIS sentence, and bumping is exactly
    // what used to discard it.
    let gen = *generation;
    let text = utterance.original().to_string();
    if text.trim().is_empty() {
        *generation = generation.wrapping_add(1);
        crate::metrics::record_talk_direction_unknown();
        return;
    }
    *resolving = true;
    let resolver = resolver.clone();
    let tx = verdict_tx.clone();
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let outcome = resolver.resolve_final(&text).await;
        crate::metrics::record_talk_direction_ms(started.elapsed().as_millis() as u64);
        let _ = tx.send(Verdict {
            generation: gen,
            outcome,
        });
    });
}

fn emit_direction(direction: Direction, resolver: &Resolver, out_tx: &PeerTx) {
    let (Some(spoken), Some(target)) = (
        direction.source(resolver.user_lang(), resolver.other_lang()),
        direction.target(resolver.user_lang(), resolver.other_lang()),
    ) else {
        return;
    };
    crate::metrics::record_talk_direction_resolved();
    let _ = out_tx.send(
        ServerMessage::TalkDirection {
            spoken: spoken.to_string(),
            target: target.to_string(),
        }
        .to_json(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_lang_matches_the_ws_route_rules() {
        // Same rule as `/ws` and `/ws/extension`: these values reach provider URLs and
        // prompts, so a looser rule here would reopen an injection the others closed.
        assert!(valid_lang("it"));
        assert!(valid_lang("pt-br"));
        assert!(valid_lang("yue"));

        assert!(!valid_lang(""), "empty");
        assert!(!valid_lang("toolonglang"), "over 8 chars");
        assert!(!valid_lang("en&redact=pci"), "query smuggling");
        assert!(!valid_lang("en us"), "whitespace");
        assert!(!valid_lang("en/../x"), "path traversal");
    }

    #[test]
    fn original_text_is_read_from_both_subtitle_shapes() {
        let interim = ServerMessage::SubtitleInterim {
            speaker_id: "s".into(),
            speaker_name: "s".into(),
            text: "Quiero ir a la estación".into(),
            lang: "auto".into(),
            original: Some("  Vorrei andare alla stazione  ".into()),
        }
        .to_json();
        assert_eq!(
            original_text(&interim).as_deref(),
            Some("Vorrei andare alla stazione"),
            "whitespace is trimmed so it cannot skew the length throttle"
        );

        let mut translations = std::collections::HashMap::new();
        translations.insert("es".to_string(), "Quiero ir".to_string());
        let fin = ServerMessage::SubtitleFinal {
            speaker_id: "s".into(),
            speaker_name: "s".into(),
            original: "Vorrei andare".into(),
            lang: "auto".into(),
            translations,
        }
        .to_json();
        assert_eq!(original_text(&fin).as_deref(), Some("Vorrei andare"));
    }

    #[test]
    fn original_text_is_absent_when_there_is_nothing_to_classify() {
        // An interim carrying only the speaker's own words has no `original` field, and
        // an empty one is not evidence. Both must yield None rather than "".
        let bare = ServerMessage::SubtitleInterim {
            speaker_id: "s".into(),
            speaker_name: "s".into(),
            text: "Vorrei".into(),
            lang: "auto".into(),
            original: None,
        }
        .to_json();
        assert_eq!(original_text(&bare), None);

        let blank = ServerMessage::SubtitleInterim {
            speaker_id: "s".into(),
            speaker_name: "s".into(),
            text: "x".into(),
            lang: "auto".into(),
            original: Some("   ".into()),
        }
        .to_json();
        assert_eq!(original_text(&blank), None);

        assert_eq!(original_text("not json"), None);
        assert_eq!(
            original_text(&ServerMessage::BalanceExhausted.to_json()),
            None
        );
    }
}
