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

use tokio::sync::mpsc::Receiver;
use uuid::Uuid;

use crate::deepgram::SpeakerCtx;
use crate::engine::{SessionDeps, SessionOutcome};
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
    let engine = state.engines.resolve(Some(&leg.engine_id));

    let ctx = SpeakerCtx {
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
    let deps = SessionDeps {
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
        transcript_writer: Default::default(),
    };

    let to_engine = match engine.start_session(ctx, deps).await {
        SessionOutcome::Started(tx) => tx,
        // Standard never reports AtCapacity, so reaching either arm means the upstream is
        // genuinely unavailable. Tearing the room down is right: a call whose audio cannot
        // be translated is not a call this product is selling.
        SessionOutcome::AtCapacity | SessionOutcome::Failed => {
            crate::metrics::record_voip_provider_error();
            tracing::error!(call_id = %leg.call_id, "could not open a translation session for the phone leg");
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

    // End the call. Without the bridge the two parties cannot understand each other, and
    // the carrier does not re-open a stream on its own — so the alternative is a call that
    // keeps billing, on both sides, until the maximum-duration reaper notices up to an
    // hour later. `end_call` is a no-op on a call that is already terminal, which is the
    // ordinary case: a normal hangup closes this socket too.
    end_call(state, leg.call_id, FailureReason::MediaLost).await;
    result
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
}
