//! Reproduction harness for a production incident (v1.58.2, one call, 2026-09-15):
//! a phone speaker's Standard translation session never opened a Qwen upstream for a
//! web listener who joined the room after the phone started speaking.
//!
//! This exercises the real call site directly, with no HTTP/DB layer in the loop:
//! - the phone joins exactly as `voip::session::create_phone_peer` does in production
//! - the Standard engine is started with a `SessionDeps` shaped like `voip::session::run_leg`
//!   builds it
//! - audio is fed continuously into the speaker's channel, like `voip::media::pump` does
//! - the web listener joins the room exactly as `handle_peer` builds its `Peer`
//!   (`server/src/lib.rs`, the peer construction around the `active_engine`/`lang` fields)
//!
//! against a mock Qwen server, and asserts a translate session opens for the listener's
//! language within the reconcile window.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;
use voxtranslate_server::config::QwenConfig;
use voxtranslate_server::deepgram::SpeakerCtx;
use voxtranslate_server::engine::realtime_mock::{Dialect, RealtimeMock};
use voxtranslate_server::engine::{
    SessionDeps, SessionOutcome, StandardEngine, TranslationEngine, STANDARD_ID,
};
use voxtranslate_server::moderation::Moderator;
use voxtranslate_server::rooms::{Peer, PeerTx, RoomManager, Visibility, OUT_CHANNEL_CAP};
use voxtranslate_server::translator::Translator;
use voxtranslate_server::voip::session::create_phone_peer;

fn cfg(mock: &RealtimeMock) -> QwenConfig {
    QwenConfig {
        api_key: "k".into(),
        model: "qwen3.5-omni-flash-realtime".into(),
        asr_model: "qwen3-asr-flash-realtime".into(),
        endpoint: mock.base_url(),
        fallback: None,
        workspace_id: None,
        voice: None,
        turn_detection: "semantic_vad".into(),
        silence_duration_ms: 500,
        segment_idle_ms: 900,
        cost_per_minute: 0.0036,
        markup: 0.25,
        max_sessions: 8,
    }
}

fn deps(rooms: Arc<RoomManager>, listener_pays: bool) -> SessionDeps {
    SessionDeps {
        rooms,
        moderator: Arc::new(Moderator::from_env()),
        transcripts: None,
        participant_row: None,
        listener_pays,
        translator: Translator::new(voxtranslate_server::groq::Groq::new(
            "k".into(),
            "openai/gpt-oss-20b".into(),
        )),
        transcript_writer: Default::default(),
    }
}

/// Join a web peer exactly as `handle_peer` builds one (`server/src/lib.rs`, the `Peer`
/// literal around lines 1735-1750): `engine` is what THIS peer chose to RECEIVE.
fn join_web_peer(rm: &RoomManager, room: &str, id: &str, lang: &str, engine: &str) {
    let (tx, rx, _overflow) = PeerTx::channel(OUT_CHANNEL_CAP);
    std::mem::forget(rx); // keep the sender alive so the peer is never pruned as dead
    let peer = Peer {
        id: id.into(),
        conn: Uuid::new_v4(),
        name: id.into(),
        lang: lang.into(),
        user_id: None,
        engine: engine.into(),
        avatar_url: None,
        cartesia_voice_id: None,
        tx,
        speaking: Arc::new(AtomicBool::new(false)),
    };
    rm.join(room, peer, Visibility::Private).unwrap();
}

/// Run the scenario under one `listener_pays` setting (production value unknown, so both
/// are tested — see `server/CLAUDE.md`-adjacent investigation notes).
async fn late_web_joiner_gets_translated(listener_pays: bool) {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let rooms = Arc::new(RoomManager::new());
    let room = format!("ph-{}", Uuid::new_v4().simple());

    // The phone joins first, exactly as `create_phone_peer` does when a call is dialled.
    let phone =
        create_phone_peer(&rooms, &room, STANDARD_ID, "es").expect("the room accepted the phone");

    let engine = StandardEngine::new(&cfg(&mock));
    let ctx = SpeakerCtx {
        room: room.clone(),
        speaker_id: phone.peer_id.clone(),
        speaker_name: "Phone".into(),
        speaker_lang: "es".into(),
        session_id: phone.session_id,
        speaker_user_id: None,
        glossary: None,
        segmentation: None,
    };

    let outcome = engine
        .start_session(ctx, deps(rooms.clone(), listener_pays))
        .await;
    let SessionOutcome::Started(audio_tx) = outcome else {
        panic!("Standard must always start the phone leg (it never reports AtCapacity)");
    };

    // Feed continuous PCM16 audio for the whole test, exactly like `voip::media::pump`
    // keeps doing for the life of the call (20ms chunks @ 24kHz mono, little-endian).
    let feeder = {
        let audio_tx = audio_tx.clone();
        tokio::spawn(async move {
            let chunk = vec![0u8; 960];
            loop {
                if audio_tx.send(chunk.clone()).await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
    };

    // The phone is alone: nothing to translate for yet.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        mock.connections(),
        0,
        "listener_pays={listener_pays}: the phone alone must open no upstream session"
    );

    // A web user joins the room mid-call, exactly as `handle_peer` builds the peer:
    // lang "it", chosen receive engine "standard".
    join_web_peer(&rooms, &room, "web-1", "it", STANDARD_ID);

    // Give the reconcile tick (RECONCILE_MS = 1000ms in `engine::standard`) up to 2.5s.
    let mut opened = false;
    for _ in 0..25 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if mock.connections() >= 1 {
            opened = true;
            break;
        }
    }
    feeder.abort();

    assert!(
        opened,
        "listener_pays={listener_pays}: no Qwen session opened for the late web joiner \
         within 2.5s (connections={}, frames={:?})",
        mock.connections(),
        mock.received()
    );
}

#[tokio::test]
async fn late_web_joiner_gets_translated_under_listener_pays() {
    late_web_joiner_gets_translated(true).await;
}

#[tokio::test]
async fn late_web_joiner_gets_translated_under_speaker_pays() {
    late_web_joiner_gets_translated(false).await;
}
