//! Work Unit A (`voip-call-hangup-and-audio-regression`, spec: translated-voip "Phone Leg
//! Departure Notification"): today, `run_leg`'s teardown removes the phone leg from its
//! room (`server/src/voip/session.rs:506`) but discards the `LeaveOutcome` — the surviving
//! web peer learns of the hangup only from the 1500ms REST poll, not the room's own
//! WebSocket fan-out. This proves `remove_phone_leg_and_notify` turns that teardown into a
//! notification, DB-free, with no HTTP/socket/provider in the loop — the same harness shape
//! as `voip_late_web_joiner.rs`: a real `RoomManager` + real `Peer`/`PeerTx` is a complete
//! proof surface for room fan-out behavior.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use uuid::Uuid;
use voxtranslate_server::rooms::{Peer, PeerTx, RoomManager, Visibility, OUT_CHANNEL_CAP};
use voxtranslate_server::voip::session::remove_phone_leg_and_notify;
use voxtranslate_server::voip::state::FailureReason;

/// Join a web peer exactly as `handle_peer` builds one (`server/src/lib.rs`) — see
/// `voip_late_web_joiner.rs::join_web_peer`, whose shape this mirrors. Returns the peer's
/// `conn` and a `Receiver<String>` the test keeps to observe what the room broadcasts to it.
fn join_web_peer(
    rm: &RoomManager,
    room: &str,
    id: &str,
    lang: &str,
) -> tokio::sync::mpsc::Receiver<String> {
    let (tx, rx, _overflow) = PeerTx::channel(OUT_CHANNEL_CAP);
    let peer = Peer {
        id: id.into(),
        conn: Uuid::new_v4(),
        name: id.into(),
        lang: lang.into(),
        user_id: None,
        engine: "standard".into(),
        avatar_url: None,
        cartesia_voice_id: None,
        tx,
        speaking: Arc::new(AtomicBool::new(false)),
    };
    rm.join(room, peer, Visibility::Private).unwrap();
    rx
}

/// Join a phone peer directly through `RoomManager`, mirroring
/// `voip::session::create_phone_peer`'s own `Peer` construction (`phone-` id prefix,
/// no account, no avatar). Built here — rather than via `create_phone_peer` — so the test
/// keeps its OWN `Receiver<String>` for the phone leg's `from_room` channel: the production
/// `PhonePeer` struct's `from_room` field is private to `voip::session` (by design — nothing
/// outside `run_leg`'s own call site is supposed to read a phone leg's inbound room frames),
/// so an external integration test proves "the departing leg gets nothing" the same way
/// `join_web_peer` proves what a survivor gets: by holding a real receiver on a real `Peer`
/// joined through the same `RoomManager::join` call site production uses.
fn join_phone_peer(
    rm: &RoomManager,
    room: &str,
    conn: Uuid,
) -> (String, tokio::sync::mpsc::Receiver<String>) {
    let peer_id = format!(
        "{}{}",
        voxtranslate_server::rooms::PHONE_PEER_ID_PREFIX,
        Uuid::new_v4().simple()
    );
    let (tx, rx, _overflow) = PeerTx::channel(OUT_CHANNEL_CAP);
    let peer = Peer {
        id: peer_id.clone(),
        conn,
        name: "Phone".into(),
        lang: "es".into(),
        user_id: None,
        engine: "standard".into(),
        avatar_url: None,
        cartesia_voice_id: None,
        tx,
        speaking: Arc::new(AtomicBool::new(false)),
    };
    rm.join(room, peer, Visibility::Private).unwrap();
    (peer_id, rx)
}

#[tokio::test]
async fn a_phone_hangup_reaches_the_surviving_web_peer() {
    let rooms = RoomManager::new();
    let room = format!("ph-{}", Uuid::new_v4().simple());
    let call_id = Uuid::new_v4();
    let phone_conn = Uuid::new_v4();

    let (phone_id, _phone_rx) = join_phone_peer(&rooms, &room, phone_conn);
    let mut web_rx = join_web_peer(&rooms, &room, "web-1", "it");

    remove_phone_leg_and_notify(&rooms, &room, &phone_id, phone_conn, call_id, None);

    let first = web_rx
        .try_recv()
        .expect("the surviving web peer must receive a peer_left frame");
    assert!(
        first.contains("\"type\":\"peer_left\""),
        "expected peer_left first, got: {first}"
    );
    assert!(first.contains(&phone_id), "peer_left must name the phone leg: {first}");

    let second = web_rx
        .try_recv()
        .expect("the surviving web peer must also receive a phone_call_ended frame");
    assert!(
        second.contains("\"type\":\"phone_call_ended\""),
        "expected phone_call_ended second, got: {second}"
    );
    assert!(second.contains(&phone_id), "phone_call_ended must name the phone leg: {second}");
    assert!(
        second.contains(&call_id.to_string()),
        "phone_call_ended must carry the call id: {second}"
    );
    assert!(
        second.contains("\"reason\":\"remote_hangup\""),
        "a hangup with no explicit failure reason must report remote_hangup: {second}"
    );
}

#[tokio::test]
async fn a_superseded_phone_leg_produces_no_end_of_call_event() {
    let rooms = RoomManager::new();
    let room = format!("ph-{}", Uuid::new_v4().simple());
    let call_id = Uuid::new_v4();
    let stale_conn = Uuid::new_v4();

    // The phone reconnects under the SAME peer id: `RoomManager::join` evicts the stale
    // entry (production reconnect path), so `phone_id` now lives under a NEW conn and the
    // stale one is gone from the room already.
    let (phone_id, _stale_rx) = join_phone_peer(&rooms, &room, stale_conn);
    let (_live_id, _live_rx) = {
        // Reuse the same generated id by joining again with an explicit id — mirrors a
        // reconnect, which always keeps the peer's user-facing id stable across sockets.
        let (tx, rx, _overflow) = PeerTx::channel(OUT_CHANNEL_CAP);
        let peer = Peer {
            id: phone_id.clone(),
            conn: Uuid::new_v4(),
            name: "Phone".into(),
            lang: "es".into(),
            user_id: None,
            engine: "standard".into(),
            avatar_url: None,
            cartesia_voice_id: None,
            tx,
            speaking: Arc::new(AtomicBool::new(false)),
        };
        rooms.join(&room, peer, Visibility::Private).unwrap();
        (phone_id.clone(), rx)
    };
    let mut web_rx = join_web_peer(&rooms, &room, "web-1", "it");

    // Remove the STALE conn, which the reconnect already superseded.
    let outcome =
        remove_phone_leg_and_notify(&rooms, &room, &phone_id, stale_conn, call_id, None);

    assert!(
        matches!(outcome, voxtranslate_server::rooms::LeaveOutcome::Superseded),
        "removing an already-superseded conn must report Superseded"
    );
    assert!(
        web_rx.try_recv().is_err(),
        "a superseded leg must broadcast nothing — the room continues uninterrupted"
    );
}

#[tokio::test]
async fn a_media_error_teardown_reports_media_lost() {
    let rooms = RoomManager::new();
    let room = format!("ph-{}", Uuid::new_v4().simple());
    let call_id = Uuid::new_v4();
    let phone_conn = Uuid::new_v4();

    let (phone_id, _phone_rx) = join_phone_peer(&rooms, &room, phone_conn);
    let mut web_rx = join_web_peer(&rooms, &room, "web-1", "it");

    remove_phone_leg_and_notify(
        &rooms,
        &room,
        &phone_id,
        phone_conn,
        call_id,
        Some(FailureReason::MediaLost),
    );

    let _peer_left = web_rx.try_recv().expect("peer_left must still fire");
    let ended = web_rx
        .try_recv()
        .expect("phone_call_ended must still fire on a media-error teardown");
    assert!(
        ended.contains("\"reason\":\"media_lost\""),
        "a media-error teardown must report media_lost, not remote_hangup: {ended}"
    );
}

#[tokio::test]
async fn the_departing_phone_leg_is_not_sent_its_own_end_event() {
    let rooms = RoomManager::new();
    let room = format!("ph-{}", Uuid::new_v4().simple());
    let call_id = Uuid::new_v4();
    let phone_conn = Uuid::new_v4();

    // `phone_rx` is the departing leg's OWN inbound channel — the thing production code
    // calls `from_room` (`BridgeHandles::from_room`, `voip/media.rs:562`).
    let (phone_id, mut phone_rx) = join_phone_peer(&rooms, &room, phone_conn);
    let _web_rx = join_web_peer(&rooms, &room, "web-1", "it");

    remove_phone_leg_and_notify(&rooms, &room, &phone_id, phone_conn, call_id, None);

    assert!(
        phone_rx.try_recv().is_err(),
        "the departing phone leg must not receive its own peer_left/phone_call_ended frames \
         (it is removed from room.peers before either broadcast)"
    );
}
