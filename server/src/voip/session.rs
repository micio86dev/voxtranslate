//! Attaching a telephone to a room (spec 0111, D3, R14–R18).
//!
//! This is the join between two things that were built separately: the room, which already
//! knows how to translate between peers, and the media bridge, which already knows how to
//! turn phone audio into engine audio. What was missing is that they happen at different
//! times.
//!
//! The room and its phone peer are created when the call is **dialled**. The media socket
//! arrives later — after the far end answers, from the provider's network, on a connection
//! that carries no session of ours. So the pieces the socket will need are parked here in
//! between, keyed by call id, and the ticket in the URL is what proves a connection may
//! claim them.
//!
//! ## Why an in-memory registry is right here, and where its limit is
//!
//! A live media socket is inherently attached to one process: it is a stream, not a record.
//! If the process dies the socket dies, on any architecture. What must survive a restart is
//! the call's *state and money*, and that is in Postgres — R27 is about the record, not the
//! stream.
//!
//! The real constraint is different and it is inherited, not introduced: **rooms are
//! in-memory per instance** across the whole product, so the media socket has to reach the
//! same instance that holds the call's room. Today the deployment is single-replica
//! (`docs/gdpr-readiness-2026-09.md` §1 says so), which is why this works. When rooms
//! become distributed, this registry moves with them — and that sentence is here so the
//! coupling is found by reading rather than by an outage.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc::Receiver;
use uuid::Uuid;

use crate::deepgram::SpeakerCtx;
use crate::engine::{EngineRegistry, SessionDeps, SessionOutcome, TranslationEngine};
use crate::rooms::{Peer, PeerTx, Visibility, OUT_CHANNEL_CAP};
use crate::telephony::MediaCodec;
use crate::voip::media::{self, BridgeHandles, Leg};
use crate::voip::state::FailureReason;

/// The name shown for the telephone participant in the room and the transcript.
///
/// Deliberately not the number: the room's peer list reaches every participant and the
/// transcript is exported. The number lives on `voip_calls`, behind authorisation.
pub const PHONE_PEER_NAME: &str = "Phone";

/// Everything the media socket will need, parked between dial and connect.
pub struct PendingLeg {
    pub call_id: Uuid,
    /// The `call_sessions` row, so the transcript lands against the same session as every
    /// other kind of call.
    pub session_id: Uuid,
    pub org_id: Uuid,
    pub user_id: Option<Uuid>,
    pub room: String,
    pub peer_id: String,
    pub conn: Uuid,
    pub engine_id: String,
    /// What the telephone SPEAKS — the recipient's language. The engine translates out of
    /// this into the room's other languages, which is the caller's.
    pub phone_language: String,
    /// The phone peer's room channel. Everything the room sends this peer arrives here;
    /// [`media::pump`] keeps the one message a telephone can render.
    pub from_room: Receiver<String>,
}

/// Legs waiting for their media socket.
#[derive(Default)]
pub struct LiveCalls {
    pending: Mutex<HashMap<Uuid, PendingLeg>>,
}

impl LiveCalls {
    pub fn new() -> Self {
        Self::default()
    }

    /// Park a leg. Replacing an existing entry for the same call is deliberate: a
    /// reconnect after a media drop re-parks, and the stale receiver should go.
    pub fn park(&self, leg: PendingLeg) {
        self.pending
            .lock()
            .expect("live calls poisoned")
            .insert(leg.call_id, leg);
    }

    /// Claim a parked leg. Removing rather than borrowing is what makes a second socket
    /// for the same call impossible — the ticket is single-use and so is this.
    pub fn take(&self, call_id: Uuid) -> Option<PendingLeg> {
        self.pending
            .lock()
            .expect("live calls poisoned")
            .remove(&call_id)
    }

    /// Read a parked leg's room and peer without claiming it.
    ///
    /// The ticket is minted against these two, and minting must not consume the leg — the
    /// socket that redeems the ticket is what claims it.
    pub fn peek(&self, call_id: Uuid) -> Option<(String, String)> {
        let m = self.pending.lock().expect("live calls poisoned");
        m.get(&call_id).map(|l| (l.room.clone(), l.peer_id.clone()))
    }

    /// Drop a parked leg that will never be claimed — the call failed before answering.
    /// Without this a refused call leaks its receiver until the process restarts.
    pub fn discard(&self, call_id: Uuid) {
        self.take(call_id);
    }

    pub fn len(&self) -> usize {
        self.pending.lock().expect("live calls poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A telephone peer that is already in its room, waiting for the call rows to exist.
///
/// Returned by [`create_phone_peer`] and consumed by [`LiveCalls::park`]. Between those two
/// the caller owns it, and owning it means being responsible for [`abandon`] if the
/// transaction that was supposed to create the call fails.
pub struct PhonePeer {
    /// **The room's session id, which becomes the call's.**
    ///
    /// This is the reason the peer is created before the database row rather than after.
    /// A room mints its own session id on creation, and the browser caller — who joins by
    /// the ordinary WebSocket path — takes that id for its transcript events. If the
    /// `call_sessions` row were inserted with an id of its own, the two halves of the same
    /// conversation would be written under two different sessions and the call detail page
    /// would show only the telephone's side.
    pub session_id: Uuid,
    pub room: String,
    pub peer_id: String,
    pub conn: Uuid,
    from_room: Receiver<String>,
}

/// Create the room and put the telephone in it.
///
/// The peer is a full room member, which is the entire trick: from here on the engines,
/// the subtitle fan-out, the transcript writer and the meter treat the telephone exactly
/// as they treat a browser. Nothing downstream knows the difference.
#[allow(clippy::result_unit_err)] // `()` = "room full", exactly as `RoomManager::join`
pub fn create_phone_peer(
    rooms: &crate::rooms::RoomManager,
    room: &str,
    engine_id: &str,
    phone_language: &str,
) -> Result<PhonePeer, ()> {
    let peer_id = format!("phone-{}", Uuid::new_v4().simple());
    let conn = Uuid::new_v4();
    let (tx, from_room, _overflow) = PeerTx::channel(OUT_CHANNEL_CAP);

    let peer = Peer {
        id: peer_id.clone(),
        conn,
        name: PHONE_PEER_NAME.to_string(),
        lang: phone_language.to_string(),
        // The telephone has no account. `None` is also what keeps the transcript's user
        // attribution honest about who was actually there.
        user_id: None,
        engine: engine_id.to_string(),
        avatar_url: None,
        cartesia_voice_id: None,
        tx,
        // Not speaking until audio actually arrives on the socket.
        speaking: Arc::new(AtomicBool::new(false)),
    };

    // Private, always. A telephone call is not something to advertise in `/rooms`.
    let joined = rooms.join(room, peer, Visibility::Private)?;

    Ok(PhonePeer {
        session_id: joined.session_id,
        room: room.to_string(),
        peer_id,
        conn,
        from_room,
    })
}

/// Removes a phone peer unless the call it belongs to actually got off the ground.
///
/// `dial` fails in a dozen places — a database error, the concurrency re-check inside the
/// admission lock, an empty credit pool, a provider refusal — and every one of them is a
/// `?` or an early return. A guard is used instead of cleanup at each site because the
/// failure that matters is the one nobody remembered to handle.
pub struct PeerGuard<'a> {
    rooms: &'a crate::rooms::RoomManager,
    room: String,
    peer_id: String,
    conn: Uuid,
    armed: bool,
}

impl<'a> PeerGuard<'a> {
    pub fn new(rooms: &'a crate::rooms::RoomManager, peer: &PhonePeer) -> Self {
        Self {
            rooms,
            room: peer.room.clone(),
            peer_id: peer.peer_id.clone(),
            conn: peer.conn,
            armed: true,
        }
    }

    /// The call is live; the room now belongs to it.
    pub fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for PeerGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.rooms.remove(&self.room, &self.peer_id, self.conn);
        }
    }
}

impl LiveCalls {
    /// Park a created peer against the call that now exists for it.
    #[allow(clippy::too_many_arguments)]
    pub fn park_peer(
        &self,
        peer: PhonePeer,
        call_id: Uuid,
        org_id: Uuid,
        user_id: Option<Uuid>,
        engine_id: &str,
        phone_language: &str,
    ) {
        self.park(PendingLeg {
            call_id,
            session_id: peer.session_id,
            org_id,
            user_id,
            room: peer.room,
            peer_id: peer.peer_id,
            conn: peer.conn,
            engine_id: engine_id.to_string(),
            phone_language: phone_language.to_string(),
            from_room: peer.from_room,
        });
    }
}

/// Open a speaking session for the phone leg, retrying once on the default engine if
/// `requested` reports [`SessionOutcome::AtCapacity`] or [`SessionOutcome::Failed`].
///
/// Mirrors the capacity fallback the browser tiers already run in `lib.rs` (spec 0094:
/// Premium/Pro `AtCapacity` → retry on Standard), extended here to `Failed` too — a phone
/// call has no listener to show a downgrade message to, so silently losing the call is
/// strictly worse than silently switching it to Standard. The retry is skipped when
/// `requested` already IS the default: retrying the same engine with the same inputs
/// would just fail again, and that case is what the caller's own teardown is for.
///
/// Production (2026-09-16, two outbound calls minutes apart): a phone leg's stored engine
/// was Cartesia (Enhanced) — the only client-direct engine, whose provider runs in a
/// BROWSER a telephone call does not have — and `start_session` unconditionally returning
/// `Failed` for it (see `engine::cartesia`'s module docs) hung the call up the instant the
/// recipient granted consent. `EngineRegistry::resolve_for_phone` now keeps a client-direct
/// engine from ever being dialled in the first place; this is the second half of the same
/// fix, for a genuine Pro/Premium `AtCapacity` and for any call dialled before that fix.
///
/// `build_ctx`/`build_deps` are closures rather than one-shot values because a retry needs
/// its own fresh [`SpeakerCtx`]/[`SessionDeps`] — the first attempt's are consumed by
/// `start_session` whether or not it succeeds.
///
/// **`voip_calls.engine_id` is deliberately left untouched by a fallback here.** Billing
/// (`voip::webhook::settle`) charges `quoted_price_per_min`, which was fixed at dial time
/// from the SAME resolved engine that was stored as `engine_id` — a runtime downgrade
/// changes what serves the call, not what the customer was quoted or is charged, so there
/// is nothing to reconcile. This matches the browser tiers: their own `AtCapacity`
/// fallback in `lib.rs` never rewrites a persisted "chosen engine" field either. Analytics
/// grouped by `engine_id` (`voip::analytics`) will attribute a downgraded call to the
/// engine it was quoted at rather than the one that carried it — an accepted, pre-existing
/// gap shared with the browser tiers, and out of scope for this fix.
async fn open_engine_session(
    engines: &EngineRegistry,
    call_id: Uuid,
    requested: std::sync::Arc<dyn TranslationEngine>,
    build_ctx: impl Fn() -> SpeakerCtx,
    build_deps: impl Fn() -> SessionDeps,
) -> (SessionOutcome, std::sync::Arc<dyn TranslationEngine>) {
    let outcome = requested.start_session(build_ctx(), build_deps()).await;
    if !matches!(outcome, SessionOutcome::AtCapacity | SessionOutcome::Failed) {
        return (outcome, requested);
    }
    let fallback = engines.default();
    if fallback.metadata().id == requested.metadata().id {
        return (outcome, requested);
    }
    tracing::warn!(
        call_id = %call_id,
        requested_engine = %requested.metadata().id,
        fallback_engine = %fallback.metadata().id,
        "phone leg engine unavailable; retrying on the default engine"
    );
    let outcome = fallback.start_session(build_ctx(), build_deps()).await;
    (outcome, fallback)
}

/// Take a claimed leg all the way to a running bridge.
///
/// Opens the engine session for the telephone as a *speaker*, then hands the socket to
/// [`media::pump`]. The engine's output for the caller's language reaches the caller's own
/// peer channel by the room's ordinary fan-out — nothing here forwards it, which is why
/// there is no place for raw audio to cross.
pub async fn run_leg<S, E>(
    state: &crate::AppState,
    leg: PendingLeg,
    codec: MediaCodec,
    socket: S,
) -> Result<(), media::MediaError>
where
    S: futures::Sink<String, Error = E> + futures::Stream<Item = Result<String, E>> + Unpin + Send,
{
    // `resolve_for_phone` (called at dial time by both `routes::quote` and `routes::dial`)
    // already refuses to ever store a client-direct engine here — but a call dialled
    // before that fix shipped, or a genuine Pro/Premium `AtCapacity`, can still name one.
    // `open_engine_session` below is the safety net for both.
    let engine = state.engines.resolve(Some(&leg.engine_id));

    let build_ctx = || SpeakerCtx {
        room: leg.room.clone(),
        speaker_id: leg.peer_id.clone(),
        speaker_name: PHONE_PEER_NAME.to_string(),
        speaker_lang: leg.phone_language.clone(),
        session_id: leg.session_id,
        // The telephone has no account, so nothing is attributed to a user id.
        speaker_user_id: None,
        glossary: state.glossary.clone(),
        // A telephone conversation is cut like a call, not like Talk to Anyone: captions
        // should land fast, and a clipped sentence is a caption problem rather than a beat
        // of silence.
        segmentation: None,
    };

    // **The transcript service is withheld unless this call may keep words.**
    //
    // The engine persists a segment whenever it holds a transcript service — see
    // `engine::standard::record_segment`, which gates on `transcripts.is_some()` and
    // nothing else. Handing it one unconditionally would write every syllable of a
    // telephone conversation into `transcript_events` from the first word: before the
    // disclosure has been spoken, before a press-key gate could be answered, and even for
    // an org that never asked for transcription at all. The consent machinery stamps
    // columns and gates the carrier's recorder; it has no reach into this pipeline, so
    // the gate has to be here, at the only place that decides what the engine is given.
    //
    // `transcription_started_at` is the authority, not the request: it is written by
    // `voip::disclosure::start_capture` only after the disclosure is on the record and,
    // where a gate applies, granted.
    let may_transcribe = transcription_permitted(state, leg.call_id).await;
    // Built once and cloned into every attempt below (including a fallback retry), so a
    // downgrade to the default engine still elects itself as this turn's writer instead
    // of losing the claim to nobody. See `engine::TranscriptWriter`.
    let transcript_writer = crate::engine::TranscriptWriter::default();
    let build_deps = || SessionDeps {
        rooms: state.rooms.clone(),
        moderator: state.moderator.clone(),
        transcripts: if may_transcribe {
            state.transcripts.clone()
        } else {
            None
        },
        participant_row: None,
        listener_pays: state.config.listener_pays,
        translator: state.translator.clone(),
        transcript_writer: transcript_writer.clone(),
    };

    let (outcome, active_engine) =
        open_engine_session(&state.engines, leg.call_id, engine, &build_ctx, &build_deps).await;

    let to_engine = match outcome {
        SessionOutcome::Started(tx) => tx,
        // Reached only once BOTH the requested engine and the default have failed
        // (`open_engine_session` already retried on the default when the two differ).
        // Standard — the default — never reports `AtCapacity` on its own, so getting here
        // means the upstream is genuinely unavailable. Tearing the room down is right at
        // that point: a call whose audio cannot be translated is not a call this product
        // is selling.
        SessionOutcome::AtCapacity | SessionOutcome::Failed => {
            crate::metrics::record_voip_provider_error();
            tracing::error!(
                call_id = %leg.call_id,
                engine = %active_engine.metadata().id,
                "could not open a translation session for the phone leg"
            );
            state.rooms.remove(&leg.room, &leg.peer_id, leg.conn);
            // Ending the call is not optional here. Removing the peer alone would leave a
            // row that still reads as live: it keeps consuming a concurrency slot, keeps
            // the customer's credits held, and keeps the carrier billing us — until the
            // maximum-duration reaper notices, up to an hour later.
            end_call(state, leg.call_id, FailureReason::EngineUnavailable).await;
            return Err(media::MediaError::Malformed(
                "translation session unavailable".into(),
            ));
        }
    };

    // Digits are consumed by the consent gate. Bounded and small: a keypad produces a
    // handful of events, and a full channel here means nobody is listening for them.
    let (digits, digit_rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(consume_digits(digit_rx));

    let result = media::pump(
        socket,
        Leg::new(codec),
        BridgeHandles {
            to_engine,
            from_room: leg.from_room,
            digits,
        },
    )
    .await;

    // The socket is gone: so is the telephone. Leaving the peer would keep the room open
    // with a participant nobody can hear.
    state.rooms.remove(&leg.room, &leg.peer_id, leg.conn);
    crate::metrics::record_voip_media_disconnect();

    // What happens next depends on WHY the pump ended, not just that it did.
    //
    // Production (2026-09-15, one call): the media socket closed at 20:23:23.242; the
    // carrier's own hangup webhooks landed at .901 and .994 — under a second later. The
    // comment this replaces assumed the opposite order ("a normal hangup closes this
    // socket too", i.e. `end_call` would already be a no-op by the time it ran). It is
    // not: media close reliably PRECEDES the hangup webhook, so failing the call the
    // instant the socket closes wins the race against a webhook that was already on its
    // way — the row is marked `failed/media_lost` with no duration and no settlement, and
    // the carrier bills us for a call our own records say never connected.
    //
    // A room-ended pump (the peer was removed out from under a still-open socket — the
    // room decided, not the carrier) has no such webhook coming, so it is failed
    // immediately exactly as before. So is a pump that returned `Err`: something went
    // wrong with the stream itself, not with an ordinary hangup.
    match &result {
        Ok(media::PumpExit::ProviderEnded) => {
            // Spawned so this function's return — and whatever called it — is not held
            // up by the grace window; see `finish_after_pump`'s doc comment for why the
            // wait itself is still bounded.
            let call_id = leg.call_id;
            let st = state.clone();
            tokio::spawn(async move {
                finish_after_pump(
                    &st,
                    call_id,
                    media::PumpExit::ProviderEnded,
                    MEDIA_LOST_GRACE,
                )
                .await;
            });
        }
        Ok(media::PumpExit::RoomEnded) | Err(_) => {
            end_call(state, leg.call_id, FailureReason::MediaLost).await;
        }
    }
    result.map(|_| ())
}

/// How long to wait, after the media stream ends on the PROVIDER side, for the carrier's
/// own hangup webhook to settle the call before ending it ourselves.
///
/// Observed in production (2026-09-15): the media socket closes roughly 0.7s BEFORE the
/// carrier's hangup webhook arrives. The wait still has to be bounded, not indefinite —
/// a stream that dies while the call is genuinely still up (a dead handset, a crashed
/// provider leg) must not keep billing until the max-duration reaper notices, up to an
/// hour later. Ten seconds is generous against an observed ~0.7s gap while staying far
/// short of anything a customer would notice as "the call hung around after I hung up".
const MEDIA_LOST_GRACE: Duration = Duration::from_millis(10_000);

/// How often [`finish_after_pump`] re-checks the row while waiting out the grace window.
/// A poll rather than a blind sleep-then-end: a webhook that lands early ends the wait
/// early instead of every provider-ended call paying the full grace regardless.
const MEDIA_LOST_GRACE_POLL: Duration = Duration::from_millis(250);

/// The post-pump decision, split out of `run_leg` so it can be driven directly in a test
/// against a real database and `webhook::apply`, without a socket, a room or a provider.
///
/// `grace` is a parameter rather than always [`MEDIA_LOST_GRACE`] so a test can wait
/// milliseconds instead of the real ~10s window.
async fn finish_after_pump(
    state: &crate::AppState,
    call_id: Uuid,
    exit: media::PumpExit,
    grace: Duration,
) {
    if exit == media::PumpExit::ProviderEnded {
        if let Some(pool) = state.pool.as_ref() {
            let deadline = tokio::time::Instant::now() + grace;
            loop {
                if call_is_terminal(pool, call_id).await == Some(true) {
                    // The carrier's hangup webhook already settled the row — completed
                    // with a real duration, or failed for its own reason. `end_call`
                    // below would be a no-op anyway, but returning early skips the rest
                    // of the wait.
                    return;
                }
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(MEDIA_LOST_GRACE_POLL.min(grace)).await;
            }
        }
    }
    // Either the room ended it, the pump errored, there is no pool to poll, or the grace
    // window elapsed with no webhook. `end_call` is a no-op on a call already terminal —
    // the ordinary outcome when the webhook DID land inside the grace window — and ends
    // it for real otherwise, which is the case a media stream that dies mid-call exists
    // to cover.
    end_call(state, call_id, FailureReason::MediaLost).await;
}

/// Whether a call's row is already in a terminal state, for [`finish_after_pump`]'s poll.
/// `None` on any uncertainty (no row, a query error) so the wait runs its full course
/// rather than ending early on a guess.
async fn call_is_terminal(pool: &crate::db::Pool, call_id: Uuid) -> Option<bool> {
    let status: String = sqlx::query_scalar("SELECT status FROM voip_calls WHERE id = $1")
        .bind(call_id)
        .fetch_optional(pool)
        .await
        .unwrap_or_else(|e| {
            tracing::error!(%call_id, error = %e, "could not poll call status while waiting for a hangup webhook");
            None
        })?;
    Some(matches!(status.as_str(), "completed" | "failed"))
}

/// End a call from the media side: hang the carrier's leg up, mark the row terminal, and
/// release the credit hold.
///
/// All three, or none of them is worth doing. A row left non-terminal keeps a concurrency
/// slot and a credit hold; a carrier leg left up keeps billing; and doing only the
/// database half would tell the customer their money was returned while the call was
/// still running.
///
/// Every step is idempotent and none is fatal: the ordinary path through here is a call
/// that has *already* ended, whose hangup webhook closed everything a moment earlier.
async fn end_call(state: &crate::AppState, call_id: Uuid, reason: FailureReason) {
    let Some(pool) = state.pool.as_ref() else {
        return;
    };

    if let (Some(provider), Some(pid)) = (
        state.telephony.as_deref(),
        provider_leg(state, call_id).await,
    ) {
        if let Err(e) = provider.hangup(&crate::telephony::LegId::new(pid)).await {
            tracing::debug!(%call_id, error = %e, "hangup while ending a call from the bridge failed");
        }
    }

    let row: Option<(Uuid,)> = sqlx::query_as(
        "UPDATE voip_calls
         SET status = 'failed',
             failure_reason = COALESCE(failure_reason, $2),
             ended_at = COALESCE(ended_at, now()),
             updated_at = now()
         WHERE id = $1 AND status NOT IN ('completed', 'failed')
         RETURNING session_id",
    )
    .bind(call_id)
    .bind(reason.as_str())
    .fetch_optional(pool)
    .await
    .unwrap_or_else(|e| {
        tracing::error!(%call_id, error = %e, "could not mark a call failed from the bridge");
        None
    });

    // Only when THIS call actually moved the row. Releasing a hold on a call someone else
    // just settled would give the credits back twice.
    if let Some((session_id,)) = row {
        if let Err(e) = crate::voip::reservation::release(pool, call_id, session_id).await {
            tracing::error!(%call_id, error = %e, "could not release the credit hold");
        }
    }
}

/// The carrier's leg id for a call, so the bridge can end it without holding one.
async fn provider_leg(state: &crate::AppState, call_id: Uuid) -> Option<String> {
    let pool = state.pool.as_ref()?;
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT provider_leg_ids[array_upper(provider_leg_ids, 1)]
         FROM voip_calls WHERE id = $1 AND status NOT IN ('completed', 'failed')",
    )
    .bind(call_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .flatten()
}

/// Whether this call has actually been permitted to keep a transcript.
///
/// Public so the invariant can be asserted directly: it is the single gate between a
/// telephone conversation and `transcript_events`, and it is worth a test of its own.
///
/// Fails closed on every uncertainty — no database, an unreadable row, a query error. A
/// call that transcribes nothing is a lost feature; a call that transcribes someone who
/// was never asked is the thing this whole module exists to prevent.
pub async fn transcription_permitted(state: &crate::AppState, call_id: Uuid) -> bool {
    let Some(pool) = state.pool.as_ref() else {
        return false;
    };
    match sqlx::query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>(
        "SELECT transcription_started_at FROM voip_calls WHERE id = $1",
    )
    .bind(call_id)
    .fetch_optional(pool)
    .await
    {
        Ok(Some(stamp)) => stamp.is_some(),
        Ok(None) => false,
        Err(e) => {
            tracing::error!(%call_id, error = %e, "could not read consent state; not transcribing");
            false
        }
    }
}

/// Drain DTMF arriving on the MEDIA socket.
///
/// Not the consent gate. The gate is driven by `call.dtmf.received` **webhooks** — see
/// `telephony::TelephonyProvider::gather`, whose contract says digits arrive that way, and
/// `voip::routes::inbound_webhook`, which is what calls `disclosure::on_dtmf`. Digits that
/// appear in-band on the media stream are a second, redundant copy of the same keypresses
/// and are deliberately not acted on: two sources resolving one gate is how a decision
/// gets made twice.
///
/// They are still drained, because an unread channel makes `try_send` fail on every
/// keypress and the media pump would log a full channel for something nobody wants.
async fn consume_digits(mut rx: tokio::sync::mpsc::Receiver<char>) {
    while let Some(d) = rx.recv().await {
        tracing::debug!(digit = %d, "voip dtmf");
    }
}

/// Take a leg out of the registry and its peer out of the room, for a call that is over.
///
/// The registry is not self-cleaning and cannot be. A parked leg holds the phone peer's
/// room `Receiver`, so `RoomManager::prune` — which reclaims a room once its peers'
/// channels close — can never reclaim a room whose leg is still parked. A call that is
/// dialled and then never answered (the provider accepts and goes quiet, which is the
/// exact case `webhook::fail_stalled_calls` exists for) would otherwise hold a leg, a
/// peer and a room for the life of the process.
///
/// Safe to call for a call that was never parked, or whose socket already claimed it.
pub fn reclaim(state: &crate::AppState, call_id: Uuid) {
    if let Some(leg) = state.voip_calls.take(call_id) {
        state.rooms.remove(&leg.room, &leg.peer_id, leg.conn);
        tracing::debug!(%call_id, "reclaimed a phone leg whose call ended before its media socket");
    }
}

// ---------------------------------------------------------------------------
// Arming the media socket
// ---------------------------------------------------------------------------

/// Ask the provider to open its media socket against us, once the far end has answered.
///
/// Not before: a stream started while the phone is still ringing carries ringback, and the
/// provider bills the streaming operation from the moment it starts.
///
/// The URL is built here and only here, from `VOIP_MEDIA_WS_BASE` plus a freshly minted
/// single-use ticket. Nothing a client sent reaches it — an attacker-chosen `stream_url`
/// would make the telephony provider a confused deputy pointed at whatever host they like.
pub async fn arm_media(
    state: &crate::AppState,
    cfg: &crate::config::VoipConfig,
    provider: &dyn crate::telephony::TelephonyProvider,
    call_id: Uuid,
    leg: &crate::telephony::LegId,
) {
    let Some((room, peer_id)) = state.voip_calls.peek(call_id) else {
        // The leg was never parked, or a socket already claimed it. Both are survivable and
        // neither is worth failing the webhook over — a webhook we reject is retried, and
        // retrying this would not create the leg.
        tracing::warn!(%call_id, "call answered with no phone leg parked; no media started");
        return;
    };

    let ticket = crate::voip::token::issue(
        &cfg.media_ticket_key,
        call_id,
        leg.as_str(),
        &room,
        &peer_id,
        chrono::Utc::now().timestamp(),
    );
    let url = crate::voip::token::media_url(&cfg.media_ws_base, &ticket);

    let req = crate::telephony::MediaStreamConfig {
        url,
        // Requested, not guaranteed: the route decides, and `Leg::renegotiate` adopts
        // whatever the `start` frame actually reports.
        codec: crate::telephony::MediaCodec::L16,
        // Inbound only. `Both` would send us the translated audio we just played, and the
        // engine would translate the translation.
        track: crate::telephony::MediaTrack::Inbound,
        bidirectional: true,
    };

    if let Err(e) = provider.start_media_stream(leg, req).await {
        crate::metrics::record_voip_provider_error();
        // The call is up and both parties can hear ringing silence. Nothing here can fix
        // that, but leaving the leg parked would hold a room open for a socket that will
        // never arrive.
        tracing::error!(%call_id, error = %e, "could not start the media stream; the call has no audio path");
        // Claim the leg rather than discarding it: taking it back yields the connection id,
        // and removing a peer with the wrong one would evict whoever reconnected under
        // that peer id instead.
        if let Some(leg) = state.voip_calls.take(call_id) {
            state.rooms.remove(&leg.room, &leg.peer_id, leg.conn);
        }
    }
}

// ---------------------------------------------------------------------------
// The socket
// ---------------------------------------------------------------------------

/// An axum WebSocket seen as the text-frame channel [`media::pump`] expects.
///
/// The media protocol is JSON text frames throughout. Ping/pong and any binary frame are
/// skipped rather than surfaced: a keep-alive must not look like a malformed media frame
/// and take the call's audio down.
pub struct TextSocket(pub axum::extract::ws::WebSocket);

impl futures::Stream for TextSocket {
    type Item = Result<String, axum::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use axum::extract::ws::Message;
        use std::task::Poll;
        loop {
            match std::pin::Pin::new(&mut self.0).poll_next(cx) {
                Poll::Ready(Some(Ok(Message::Text(t)))) => {
                    return Poll::Ready(Some(Ok(t.to_string())))
                }
                Poll::Ready(Some(Ok(Message::Close(_)))) | Poll::Ready(None) => {
                    return Poll::Ready(None)
                }
                Poll::Ready(Some(Ok(_))) => continue,
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl futures::Sink<String> for TextSocket {
    type Error = axum::Error;

    fn poll_ready(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::pin::Pin::new(&mut self.0).poll_ready(cx)
    }

    fn start_send(mut self: std::pin::Pin<&mut Self>, item: String) -> Result<(), Self::Error> {
        std::pin::Pin::new(&mut self.0).start_send(axum::extract::ws::Message::Text(item.into()))
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::pin::Pin::new(&mut self.0).poll_close(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rooms::RoomManager;

    fn park_one(live: &LiveCalls, call: Uuid) {
        let (_tx, rx, _o) = PeerTx::channel(4);
        live.park(PendingLeg {
            call_id: call,
            session_id: Uuid::new_v4(),
            org_id: Uuid::new_v4(),
            user_id: None,
            room: "r".into(),
            peer_id: "p".into(),
            conn: Uuid::new_v4(),
            engine_id: "standard".into(),
            phone_language: "zh".into(),
            from_room: rx,
        });
    }

    #[test]
    fn a_parked_leg_can_be_claimed_exactly_once() {
        // The ticket in the URL is single-use; so is the leg it points at. Two sockets for
        // one call would both feed the same engine session.
        let live = LiveCalls::new();
        let call = Uuid::new_v4();
        park_one(&live, call);

        assert!(live.take(call).is_some());
        assert!(live.take(call).is_none(), "a second socket finds nothing");
        assert!(live.is_empty());
    }

    #[test]
    fn re_parking_replaces_the_stale_receiver() {
        // A reconnect after a media drop parks again. Keeping the old receiver would leave
        // the room writing into a channel nobody drains.
        let live = LiveCalls::new();
        let call = Uuid::new_v4();
        park_one(&live, call);
        park_one(&live, call);
        assert_eq!(live.len(), 1);
    }

    #[test]
    fn minting_a_ticket_does_not_consume_the_leg() {
        // The ticket is minted while the answer webhook is being handled; the socket that
        // redeems it connects afterwards. If reading the room and peer claimed the leg,
        // the socket would arrive to find nothing and the call would be silent.
        let live = LiveCalls::new();
        let call = Uuid::new_v4();
        park_one(&live, call);

        let (room, peer) = live.peek(call).expect("parked");
        assert_eq!(room, "r");
        assert_eq!(peer, "p");
        assert_eq!(live.len(), 1, "peeking must leave the leg parked");
        assert!(
            live.take(call).is_some(),
            "and the socket can still claim it"
        );
    }

    #[test]
    fn a_call_that_never_answers_can_be_discarded() {
        // Otherwise a refused call leaks its receiver until the process restarts.
        let live = LiveCalls::new();
        let call = Uuid::new_v4();
        park_one(&live, call);
        live.discard(call);
        assert!(live.is_empty());
        // Discarding twice is harmless — teardown paths run more than once.
        live.discard(call);
    }

    #[test]
    fn the_call_session_is_the_room_session() {
        // The load-bearing one. The browser caller joins this same room by the ordinary
        // WebSocket path and takes the ROOM's session id for its transcript events. If the
        // call is filed under a different id, the caller's half of the conversation is
        // written somewhere the call detail page never looks.
        let rooms = RoomManager::new();
        let peer = create_phone_peer(&rooms, "ph-test-1", "standard", "zh").expect("new room");

        assert_eq!(
            Some(peer.session_id),
            rooms.session_id("ph-test-1"),
            "the call must be filed under the room's own session id, not one of its own"
        );
    }

    #[test]
    fn the_telephone_joins_as_an_ordinary_peer() {
        let rooms = RoomManager::new();
        let peer = create_phone_peer(&rooms, "ph-test-2", "standard", "zh").expect("new room");

        assert!(peer.peer_id.starts_with("phone-"));
        assert_eq!(rooms.active_peers(), 1);

        let snap = rooms
            .peer_snapshot("ph-test-2", &peer.peer_id)
            .expect("the peer is in the room");
        assert_eq!(
            snap.lang, "zh",
            "the phone speaks the RECIPIENT's language — that is what the engine translates out of"
        );
        assert_eq!(
            snap.name, PHONE_PEER_NAME,
            "the number is never the display name — the peer list reaches everyone in the room"
        );
    }

    #[test]
    fn the_room_is_private() {
        // A telephone call must not appear in `/rooms`. Public rooms are also account-only,
        // and a telephone has no account.
        let rooms = RoomManager::new();
        create_phone_peer(&rooms, "ph-test-3", "standard", "it").unwrap();
        assert!(
            rooms.public_rooms().is_empty(),
            "a phone room must never be advertised"
        );
    }

    #[test]
    fn a_refused_dial_leaves_no_peer_behind() {
        // `dial` fails in a dozen places after the peer exists — a database error, the
        // concurrency re-check inside the admission lock, an empty credit pool, a provider
        // refusal — and the guard is what covers all of them, including the one nobody
        // remembered. Dropping it must take the peer, and with it the room.
        let rooms = RoomManager::new();
        let peer = create_phone_peer(&rooms, "ph-test-4", "standard", "it").unwrap();
        assert_eq!(rooms.active_peers(), 1);

        drop(PeerGuard::new(&rooms, &peer));

        assert_eq!(rooms.active_peers(), 0);
        assert_eq!(rooms.active_rooms(), 0);
    }

    #[test]
    fn a_dial_that_succeeds_keeps_its_peer() {
        // The other half: disarming is what hands the room's lifetime over to the call.
        let rooms = RoomManager::new();
        let peer = create_phone_peer(&rooms, "ph-test-4b", "standard", "it").unwrap();

        PeerGuard::new(&rooms, &peer).disarm();

        assert_eq!(
            rooms.active_peers(),
            1,
            "a live call must keep its telephone"
        );
    }

    #[test]
    fn parking_a_created_peer_carries_the_room_session_id() {
        let rooms = RoomManager::new();
        let live = LiveCalls::new();
        let peer = create_phone_peer(&rooms, "ph-test-5", "standard", "zh").unwrap();
        let session_id = peer.session_id;
        let call_id = Uuid::new_v4();

        live.park_peer(
            peer,
            call_id,
            Uuid::new_v4(),
            Some(Uuid::new_v4()),
            "standard",
            "zh",
        );

        let parked = live.take(call_id).expect("parked under its call id");
        assert_eq!(parked.session_id, session_id);
        assert_eq!(parked.phone_language, "zh");
        assert!(
            parked.user_id.is_some(),
            "the CALLER is recorded on the leg even though the phone peer has no account"
        );
    }

    // ---- `open_engine_session` — the hang-up-on-Cartesia regression -------------------
    //
    // Production (2026-09-16, two outbound calls): the phone leg's engine was Cartesia
    // (Enhanced, client-direct) and `start_session` on it always returns `Failed` — so the
    // call hung up instantly, with no `<engine>: start_session` line ever logged, the
    // moment the recipient granted consent. These pin the fallback that now runs instead
    // of an immediate hang-up, with no database and no media socket involved.

    /// A [`TranslationEngine`] whose outcome and call count are fixed at construction —
    /// enough to prove which engine `open_engine_session` actually tried, and how often,
    /// without a real Qwen/OpenAI/Gemini session.
    struct FakeEngine {
        meta: crate::engine::EngineMetadata,
        outcome: fn() -> SessionOutcome,
        attempts: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl TranslationEngine for FakeEngine {
        fn metadata(&self) -> &crate::engine::EngineMetadata {
            &self.meta
        }

        async fn start_session(&self, _ctx: SpeakerCtx, _deps: SessionDeps) -> SessionOutcome {
            self.attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            (self.outcome)()
        }
    }

    fn fake_meta(id: &str) -> crate::engine::EngineMetadata {
        crate::engine::EngineMetadata {
            id: id.into(),
            display_name: id.into(),
            tier: "t".into(),
            description: String::new(),
            cost_per_minute: 0.0,
            markup: 0.0,
            input_languages: vec![],
            output_languages: vec![],
            capabilities: crate::engine::EngineCapabilities {
                translated_audio: false,
                cost_scales_per_language: false,
                client_direct: false,
                max_room_size: 4,
            },
        }
    }

    fn always_started() -> SessionOutcome {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        SessionOutcome::Started(tx)
    }

    /// A no-database `SpeakerCtx`/`SessionDeps` pair, freshly built on every call —
    /// exactly the shape `run_leg` hands `open_engine_session`.
    fn test_ctx_and_deps() -> (impl Fn() -> SpeakerCtx, impl Fn() -> SessionDeps) {
        let rooms = std::sync::Arc::new(RoomManager::new());
        let moderator = std::sync::Arc::new(crate::moderation::Moderator::from_env());
        let translator = crate::translator::Translator::new(crate::groq::Groq::new(
            "test-key".into(),
            "openai/gpt-oss-20b".into(),
        ));
        let build_ctx = move || SpeakerCtx {
            room: "r".into(),
            speaker_id: "p".into(),
            speaker_name: PHONE_PEER_NAME.to_string(),
            speaker_lang: "zh".into(),
            session_id: Uuid::new_v4(),
            speaker_user_id: None,
            glossary: None,
            segmentation: None,
        };
        let build_deps = move || SessionDeps {
            rooms: rooms.clone(),
            moderator: moderator.clone(),
            transcripts: None,
            participant_row: None,
            listener_pays: false,
            translator: translator.clone(),
            transcript_writer: Default::default(),
        };
        (build_ctx, build_deps)
    }

    #[tokio::test]
    async fn a_failed_engine_falls_back_to_the_default_instead_of_hanging_up() {
        let mut registry = EngineRegistry::new("standard");
        registry.register(std::sync::Arc::new(FakeEngine {
            meta: fake_meta("standard"),
            outcome: always_started,
            attempts: Default::default(),
        }));

        let requested = std::sync::Arc::new(FakeEngine {
            meta: fake_meta("premium"), // the Pro tier's persisted id
            outcome: || SessionOutcome::Failed,
            attempts: Default::default(),
        });
        let requested_for_asserts = requested.clone();
        let (build_ctx, build_deps) = test_ctx_and_deps();

        let (outcome, active) =
            open_engine_session(&registry, Uuid::new_v4(), requested, build_ctx, build_deps).await;

        assert!(
            matches!(outcome, SessionOutcome::Started(_)),
            "the call must recover on the default engine, not hang up"
        );
        assert_eq!(active.metadata().id, "standard");
        assert_eq!(
            requested_for_asserts
                .attempts
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the requested engine is tried exactly once before falling back"
        );
    }

    #[tokio::test]
    async fn an_at_capacity_engine_falls_back_to_the_default_instead_of_hanging_up() {
        // Same shape as the `Failed` case above, for the outcome Pro/Premium actually
        // report under real load (spec 0094) — proving the phone leg gets the identical
        // second chance the browser tiers already have in `lib.rs`.
        let mut registry = EngineRegistry::new("standard");
        registry.register(std::sync::Arc::new(FakeEngine {
            meta: fake_meta("standard"),
            outcome: always_started,
            attempts: Default::default(),
        }));

        let requested = std::sync::Arc::new(FakeEngine {
            meta: fake_meta("gemini_live_translate"), // the Premium tier's persisted id
            outcome: || SessionOutcome::AtCapacity,
            attempts: Default::default(),
        });
        let (build_ctx, build_deps) = test_ctx_and_deps();

        let (outcome, active) =
            open_engine_session(&registry, Uuid::new_v4(), requested, build_ctx, build_deps).await;

        assert!(matches!(outcome, SessionOutcome::Started(_)));
        assert_eq!(active.metadata().id, "standard");
    }

    #[tokio::test]
    async fn the_default_engine_is_not_retried_against_itself() {
        // If the default engine is the one that failed, retrying it with the same inputs
        // would just fail again — that case is what the caller's own teardown is for.
        let only = std::sync::Arc::new(FakeEngine {
            meta: fake_meta("standard"),
            outcome: || SessionOutcome::Failed,
            attempts: Default::default(),
        });
        let mut registry = EngineRegistry::new("standard");
        registry.register(only.clone());
        let (build_ctx, build_deps) = test_ctx_and_deps();

        let (outcome, active) = open_engine_session(
            &registry,
            Uuid::new_v4(),
            only.clone(),
            build_ctx,
            build_deps,
        )
        .await;

        assert!(matches!(outcome, SessionOutcome::Failed));
        assert_eq!(active.metadata().id, "standard");
        assert_eq!(
            only.attempts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no retry when the failed engine already is the default"
        );
    }

    // ---- `finish_after_pump` — the normal-hangup-vs-failed-call regression -----------
    //
    // The production incident this covers (2026-09-15): a call answered ~56s earlier had
    // its media socket close, `run_leg` immediately called `end_call`, and the carrier's
    // hangup webhooks — which landed under a second later — found the row already
    // `failed`/`media_lost` and could do nothing. The dashboard showed a failed call with
    // no duration and no charge, while the carrier billed us for it.

    /// DB-gated, same fixture shape as `voip::webhook`'s tests: a plain `postgres:16`
    /// (no pgvector) makes migrations fail and this silently returns `None`, which is why
    /// every test below prints its own "skipping" line rather than just returning.
    struct Fx {
        pool: crate::db::Pool,
        org: Uuid,
        session: Uuid,
        call: Uuid,
        tag: String,
    }

    impl Fx {
        /// A `state` wired to this fixture's own pool and nothing else — `finish_after_pump`
        /// and `end_call` only ever touch `state.pool` and `state.telephony`, and leaving
        /// `telephony` `None` is exactly what a config with no VoIP provider produces.
        fn state(&self) -> crate::AppState {
            let cfg = crate::config::Config::test_with_billing("", "x".repeat(32).as_str(), 0.0);
            let mut state = crate::AppState::new(cfg);
            state.pool = Some(self.pool.clone());
            state
        }
    }

    /// Builds a call already `answered` ~56s ago — the state `run_leg` finds itself in
    /// when the media pump returns, in both the regression and its fix.
    async fn setup_answered(credits: i32, price: &str) -> Option<Fx> {
        let tag = Uuid::new_v4().simple().to_string();
        let url = std::env::var("DATABASE_URL").ok()?;
        let pool = crate::db::connect(&url).await.ok()?;
        crate::db::migrate(&pool).await.ok()?;

        let user: Uuid = sqlx::query_scalar(
            "INSERT INTO users (google_id, email, name, balance)
             VALUES ($1, $2, 'Owner', 0) RETURNING id",
        )
        .bind(format!("g-{}", Uuid::new_v4()))
        .bind(format!("{}@example.test", Uuid::new_v4()))
        .fetch_one(&pool)
        .await
        .unwrap();

        let org: Uuid = sqlx::query_scalar(
            "INSERT INTO organizations (name, slug, owner_id, credits_balance)
             VALUES ('VoIP Co', $1, $2, $3) RETURNING id",
        )
        .bind(format!("voip-{}", Uuid::new_v4().simple()))
        .bind(user)
        .bind(credits)
        .fetch_one(&pool)
        .await
        .unwrap();

        let session = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO call_sessions (id, room, org_id, kind) VALUES ($1, $2, $3, 'phone')",
        )
        .bind(session)
        .bind(format!("ph-{}", Uuid::new_v4().simple()))
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();

        let answered_at = chrono::Utc::now() - chrono::Duration::seconds(56);
        let call: Uuid = sqlx::query_scalar(
            "INSERT INTO voip_calls
                (session_id, org_id, user_id, provider, direction, recipient_e164,
                 recipient_pseudonym, recipient_country, source_language, target_language,
                 engine_id, status, quoted_price_per_min, provider_leg_ids, answered_at,
                 started_at)
             VALUES ($1, $2, $3, 'mock', 'outbound', '+8613800138000', 'abc', 'CN', 'it',
                     'zh', 'standard', 'answered', $4, ARRAY[$5], $6, $6)
             RETURNING id",
        )
        .bind(session)
        .bind(org)
        .bind(user)
        .bind(price.parse::<rust_decimal::Decimal>().unwrap())
        .bind(format!("leg-{tag}"))
        .bind(answered_at)
        .fetch_one(&pool)
        .await
        .unwrap();

        Some(Fx {
            pool,
            org,
            session,
            call,
            tag,
        })
    }

    async fn call_status_and_reason(
        pool: &crate::db::Pool,
        call: Uuid,
    ) -> (String, Option<String>) {
        sqlx::query_as("SELECT status, failure_reason FROM voip_calls WHERE id = $1")
            .bind(call)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn org_balance(pool: &crate::db::Pool, org: Uuid) -> i32 {
        sqlx::query_scalar("SELECT credits_balance FROM organizations WHERE id = $1")
            .bind(org)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_hangup_webhook_that_lands_inside_the_grace_completes_the_call_not_fails_it() {
        // THE regression: a prompt hangup webhook (~0.7s behind the media socket closing,
        // in production) must be allowed to settle the call as a real, billable
        // conversation — not lose a race against an immediate `end_call`.
        let Some(f) = setup_answered(1000, "0.60").await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        crate::voip::reservation::reserve(&f.pool, f.org, f.call, f.session, None, 300)
            .await
            .unwrap();
        assert_eq!(org_balance(&f.pool, f.org).await, 700);

        let state = f.state();
        let grace_task = tokio::spawn({
            let state = state.clone();
            let call_id = f.call;
            async move {
                finish_after_pump(
                    &state,
                    call_id,
                    media::PumpExit::ProviderEnded,
                    std::time::Duration::from_millis(500),
                )
                .await;
            }
        });

        // Well inside the 500ms grace, and comfortably longer than the ~0.7s production
        // gap would need if this were milliseconds instead of the real seconds.
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let hangup = crate::telephony::ProviderEvent {
            provider: crate::telephony::mock::MOCK_ID,
            event_id: format!("{}-hangup", f.tag),
            leg_id: crate::telephony::LegId::new(format!("leg-{}", f.tag)),
            client_state: Some(f.call.to_string()),
            occurred_at: chrono::Utc::now(),
            kind: crate::telephony::ProviderEventKind::Hangup {
                cause: FailureReason::Unmapped,
            },
        };
        crate::voip::webhook::apply(&f.pool, &hangup).await.unwrap();

        grace_task.await.unwrap();

        let (status, reason) = call_status_and_reason(&f.pool, f.call).await;
        assert_eq!(
            status, "completed",
            "the webhook settled it, not the grace timeout"
        );
        assert_ne!(reason.as_deref(), Some("media_lost"));

        let duration: Option<i32> =
            sqlx::query_scalar("SELECT duration_seconds FROM voip_calls WHERE id = $1")
                .bind(f.call)
                .fetch_one(&f.pool)
                .await
                .unwrap();
        assert!(duration.is_some(), "a real call has a real duration");

        assert!(
            crate::voip::reservation::open_for_call(&f.pool, f.call)
                .await
                .unwrap()
                .is_none(),
            "the hold was settled, not just released"
        );
        assert_eq!(
            org_balance(&f.pool, f.org).await,
            1000 - duration.unwrap() as i32, // 0.60/min = 1 credit/sec at this price
            "the call was billed for what it actually used"
        );
    }

    #[tokio::test]
    async fn no_hangup_webhook_inside_the_grace_still_fails_the_call_and_refunds_the_hold() {
        // The complement: a media stream that dies for real, with no carrier webhook
        // coming at all, must still end up `failed/media_lost` with its hold released —
        // the grace window bounds the wait, it does not remove the fallback.
        let Some(f) = setup_answered(1000, "0.60").await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        crate::voip::reservation::reserve(&f.pool, f.org, f.call, f.session, None, 300)
            .await
            .unwrap();
        assert_eq!(org_balance(&f.pool, f.org).await, 700);

        let state = f.state();
        finish_after_pump(
            &state,
            f.call,
            media::PumpExit::ProviderEnded,
            std::time::Duration::from_millis(150),
        )
        .await;

        let (status, reason) = call_status_and_reason(&f.pool, f.call).await;
        assert_eq!(status, "failed");
        assert_eq!(reason.as_deref(), Some("media_lost"));

        assert!(
            crate::voip::reservation::open_for_call(&f.pool, f.call)
                .await
                .unwrap()
                .is_none(),
            "the hold must not stay open on a call nobody is settling"
        );
        assert_eq!(
            org_balance(&f.pool, f.org).await,
            1000,
            "nothing was billed — the hold came back in full"
        );
    }

    #[tokio::test]
    async fn a_room_ended_pump_fails_the_call_immediately_no_grace() {
        // The other exit reason: the room decided, not the carrier, so there is no
        // webhook to wait for. This must behave exactly as the old unconditional
        // `end_call` did — immediately, regardless of grace.
        let Some(f) = setup_answered(1000, "0.60").await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        crate::voip::reservation::reserve(&f.pool, f.org, f.call, f.session, None, 300)
            .await
            .unwrap();

        let state = f.state();
        let started = std::time::Instant::now();
        finish_after_pump(
            &state,
            f.call,
            media::PumpExit::RoomEnded,
            std::time::Duration::from_secs(10),
        )
        .await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "a room-ended pump must not wait out the provider-ended grace at all"
        );

        let (status, reason) = call_status_and_reason(&f.pool, f.call).await;
        assert_eq!(status, "failed");
        assert_eq!(reason.as_deref(), Some("media_lost"));
    }
}
