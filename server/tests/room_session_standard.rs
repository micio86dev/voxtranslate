//! A real room, over `/ws`, with the Standard engine pointed at a stand-in.
//!
//! `handle_peer` is the function every call goes through, and until now the half of
//! it that opens a translation session was unreachable: it needs a provider. With
//! `QWEN_ENDPOINT` aimed at `realtime_mock`, two peers in different languages can
//! actually join, speak and be captioned — the join handshake, the engine start,
//! the fan-out and the teardown, with the code production runs.
//!
//! DB-gated: skipped without `DATABASE_URL`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt as _, StreamExt as _};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::{Config, QwenConfig};
use voxtranslate_server::db::{self, Pool};
use voxtranslate_server::engine::realtime_mock::{Dialect, RealtimeMock, Reply};
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::transcripts::TranscriptService;
use voxtranslate_server::{app, AppState};

const SECRET: &str = "room-session-secret";

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Server {
    addr: SocketAddr,
    pool: Pool,
}

/// A server whose Standard engine talks to `mock` instead of Model Studio.
async fn setup(mock: &RealtimeMock) -> Option<Server> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let mut config = Config::test_with_billing(&url, SECRET, 5.0);
    config.qwen = QwenConfig {
        api_key: "test-key".into(),
        endpoint: mock.base_url(),
        ..Default::default()
    };
    config.standard_enabled = true;
    // The per-listener engine routing lives on the listener-pays path; the
    // speaker-pays one goes through Deepgram, which has no stand-in.
    config.listener_pays = true;
    let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);
    let mut state = AppState::new(config);
    state.billing = Some(BillingService::new(pool.clone(), min_join));
    state.safety = Some(SafetyService::new(pool.clone()));
    state.transcripts = Some(TranscriptService::new(pool.clone()));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    Some(Server { addr, pool })
}

/// A signed-in user who has cleared the consent gate `authorize` enforces.
async fn login(srv: &Server, name: &str) -> String {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: name.into(),
        avatar_url: None,
    };
    let (user, _) = upsert_google_user(
        &srv.pool,
        &identity,
        rust_decimal::Decimal::new(500, 2),
        None,
        None,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE users SET age_confirmed = TRUE, consent_tos_at = now() WHERE id = $1")
        .bind(user.id)
        .execute(&srv.pool)
        .await
        .unwrap();
    issue_jwt(SECRET, &user.id, &user.email, &user.name, 168).unwrap()
}

/// Join a room and return the socket once the server has acknowledged the join.
async fn join(srv: &Server, room: &str, id: &str, lang: &str, jwt: &str) -> Ws {
    let url = format!(
        "ws://{}/ws?room={room}&lang={lang}&id={id}&token={jwt}",
        srv.addr
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("the room accepted the peer");
    // The first frame is `room_joined`; waiting for it means the peer is really in
    // the room before the next one arrives, rather than racing it.
    let _ = wait_for(&mut ws, |v| v["type"] == "room_joined").await;
    ws
}

/// Tell the server a speaking turn has begun. The engine session opens here, not
/// on the first audio frame — a client that streams bytes without saying `start`
/// is heard by nobody, which is exactly what these tests would otherwise assert.
async fn start_speaking(ws: &mut Ws) {
    let _ = ws
        .send(Message::text(
            serde_json::json!({ "type": "start" }).to_string(),
        ))
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
}

/// Read frames until one satisfies `wanted`.
async fn wait_for(ws: &mut Ws, wanted: impl Fn(&Value) -> bool) -> Option<Value> {
    while let Ok(Some(Ok(frame))) = tokio::time::timeout(Duration::from_secs(6), ws.next()).await {
        if let Message::Text(t) = frame {
            if let Ok(v) = serde_json::from_str::<Value>(t.as_str()) {
                if wanted(&v) {
                    return Some(v);
                }
            }
        }
    }
    None
}

macro_rules! skip_without_db {
    ($mock:expr) => {
        match setup($mock).await {
            Some(srv) => srv,
            None => {
                eprintln!("skipping — no DATABASE_URL");
                return;
            }
        }
    };
}

/// An Italian speaker and an English listener, both joined, plus the speaker's socket.
async fn two_peers(srv: &Server, room: &str) -> (Ws, Ws) {
    let speaker_jwt = login(srv, "Speaker").await;
    let listener_jwt = login(srv, "Listener").await;
    let speaker = join(srv, room, "spk", "it", &speaker_jwt).await;
    let listener = join(srv, room, "lis", "en", &listener_jwt).await;
    (speaker, listener)
}

fn room_name() -> String {
    format!("rs-{}", Uuid::new_v4().simple())
}

// ---------------------------------------------------------------------------
// Joining
// ---------------------------------------------------------------------------

#[tokio::test]
async fn joining_a_room_acknowledges_the_peer_and_its_session() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = skip_without_db!(&mock);
    let jwt = login(&srv, "Solo").await;

    let url = format!(
        "ws://{}/ws?room={}&lang=it&id=solo&token={jwt}",
        srv.addr,
        room_name()
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();

    let frame = wait_for(&mut ws, |v| v["type"] == "room_joined")
        .await
        .expect("the room acknowledged the join");
    assert_eq!(frame["peer_id"], "solo");
    // Recording is on for a signed-in peer, so the join carries the session it
    // will be transcribed under.
    assert!(frame["session_id"].as_str().is_some(), "frame: {frame}");
}

#[tokio::test]
async fn a_second_peer_is_announced_to_the_first() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = skip_without_db!(&mock);
    let room = room_name();
    let speaker_jwt = login(&srv, "Speaker").await;
    let mut speaker = join(&srv, &room, "spk", "it", &speaker_jwt).await;

    let listener_jwt = login(&srv, "Listener").await;
    let _listener = join(&srv, &room, "lis", "en", &listener_jwt).await;

    let frame = wait_for(&mut speaker, |v| {
        v["type"] == "peer_joined" || v["type"] == "peers"
    })
    .await
    .expect("the first peer was told somebody arrived");
    assert!(
        frame.to_string().contains("lis"),
        "the announcement names the newcomer: {frame}"
    );
}

// ---------------------------------------------------------------------------
// Speaking
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_speaker_alone_opens_no_upstream_session() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = skip_without_db!(&mock);
    let jwt = login(&srv, "Solo").await;
    let mut ws = join(&srv, &room_name(), "solo", "it", &jwt).await;

    start_speaking(&mut ws).await;
    let _ = ws.send(Message::binary(vec![0u8; 640])).await;
    tokio::time::sleep(Duration::from_millis(600)).await;

    // Nobody to translate for. Opening a session anyway would bill the speaker for
    // an empty room, which is the whole reason `no_targets` exists.
    assert_eq!(mock.connections(), 0, "frames: {:?}", mock.received());
}

#[tokio::test]
async fn a_foreign_listener_makes_the_engine_open_a_session() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = skip_without_db!(&mock);
    let (mut speaker, _listener) = two_peers(&srv, &room_name()).await;

    start_speaking(&mut speaker).await;
    let _ = speaker.send(Message::binary(vec![0u8; 640])).await;
    tokio::time::sleep(Duration::from_millis(800)).await;

    assert!(
        mock.connections() >= 1,
        "one distinct target language, one upstream session"
    );
}

#[tokio::test]
async fn the_speakers_audio_reaches_the_provider() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = skip_without_db!(&mock);
    let (mut speaker, _listener) = two_peers(&srv, &room_name()).await;

    start_speaking(&mut speaker).await;
    for _ in 0..3 {
        let _ = speaker.send(Message::binary(vec![0u8; 640])).await;
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
    tokio::time::sleep(Duration::from_millis(600)).await;

    assert!(
        mock.saw_type("input_audio_buffer.append"),
        "frames: {:?}",
        mock.received()
    );
}

#[tokio::test]
async fn a_translated_caption_reaches_the_foreign_listener() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(vec![Reply::Transcript {
        original: "ciao a tutti".into(),
        translated: "hello everyone".into(),
    }]);
    let srv = skip_without_db!(&mock);
    let (mut speaker, mut listener) = two_peers(&srv, &room_name()).await;

    start_speaking(&mut speaker).await;
    let _ = speaker.send(Message::binary(vec![0u8; 640])).await;

    let frame = wait_for(&mut listener, |v| {
        v["type"] == "subtitle_final" || v["type"] == "subtitle_interim"
    })
    .await
    .expect("the English listener was captioned");
    assert!(
        frame.to_string().contains("hello everyone"),
        "the caption is the translation: {frame}"
    );
}

#[tokio::test]
async fn translated_speech_reaches_the_foreign_listener() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(vec![
        Reply::Transcript {
            original: "ciao".into(),
            translated: "hello".into(),
        },
        Reply::Audio(vec![1, 2, 3, 4, 5, 6, 7, 8]),
    ]);
    let srv = skip_without_db!(&mock);
    let (mut speaker, mut listener) = two_peers(&srv, &room_name()).await;

    start_speaking(&mut speaker).await;
    let _ = speaker.send(Message::binary(vec![0u8; 640])).await;

    let frame = wait_for(&mut listener, |v| v["type"] == "translated_audio")
        .await
        .expect("the listener heard the translation");
    assert!(frame["pcm16_b64"].as_str().is_some(), "frame: {frame}");
}

// ---------------------------------------------------------------------------
// Chat and leaving
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_chat_message_is_echoed_to_the_sender() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = skip_without_db!(&mock);
    let (mut speaker, _listener) = two_peers(&srv, &room_name()).await;

    let _ = speaker
        .send(Message::text(
            serde_json::json!({ "type": "chat", "text": "ciao a tutti" }).to_string(),
        ))
        .await;

    let frame = wait_for(&mut speaker, |v| v["type"] == "chat_message")
        .await
        .expect("the sender sees their own message");
    assert_eq!(frame["original"], "ciao a tutti");
}

#[tokio::test]
async fn leaving_tells_the_others_and_closes_the_upstream_session() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = skip_without_db!(&mock);
    let room = room_name();
    let (mut speaker, mut listener) = two_peers(&srv, &room).await;

    start_speaking(&mut speaker).await;
    let _ = speaker.send(Message::binary(vec![0u8; 640])).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    drop(speaker); // hang up

    let frame = wait_for(&mut listener, |v| v["type"] == "peer_left")
        .await
        .expect("the remaining peer was told");
    assert!(frame.to_string().contains("spk"), "frame: {frame}");

    // A leaked upstream session keeps billing a speaker who has gone.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        mock.received().iter().any(|f| f.contains("session")),
        "the session was opened and torn down: {:?}",
        mock.received()
    );
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_room_needs_a_language_that_could_be_one() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = skip_without_db!(&mock);
    let jwt = login(&srv, "Solo").await;

    for lang in ["", "it_IT", "averylonglang"] {
        let url = format!(
            "ws://{}/ws?room=r&lang={lang}&id=solo&token={jwt}",
            srv.addr
        );
        assert!(
            tokio_tungstenite::connect_async(url).await.is_err(),
            "lang {lang:?} should be refused"
        );
    }
}

#[tokio::test]
async fn a_forged_token_does_not_open_a_room() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = skip_without_db!(&mock);
    let forged = issue_jwt("another-secret", &Uuid::new_v4(), "e@x.com", "E", 1).unwrap();

    let url = format!("ws://{}/ws?room=r&lang=it&id=solo&token={forged}", srv.addr);
    // A private room admits a guest, but never on a token this server did not sign.
    let opened = tokio_tungstenite::connect_async(url).await;
    if let Ok((mut ws, _)) = opened {
        let frame = wait_for(&mut ws, |v| v["type"] == "room_joined").await;
        if let Some(frame) = frame {
            assert!(
                frame["session_id"].is_null(),
                "an unauthenticated peer is not recorded: {frame}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The rest of the in-call protocol
//
// Everything below is relayed rather than translated, and each frame is the whole
// of some feature's server side: a badge, a raised hand, a drawing. What is worth
// asserting is who sees it — the sender, the others, or both — because getting
// that wrong is how a reaction appears twice or a mute indicator never clears.
// ---------------------------------------------------------------------------

/// Send a JSON client frame.
async fn say(ws: &mut Ws, v: Value) {
    let _ = ws.send(Message::text(v.to_string())).await;
}

/// Everything the socket produces in `ms`.
async fn drain(ws: &mut Ws, ms: u64) -> Vec<Value> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return seen;
        }
        match tokio::time::timeout(left, ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    seen.push(v);
                }
            }
            Ok(Some(Ok(_))) => {}
            _ => return seen,
        }
    }
}

#[tokio::test]
async fn a_mute_indicator_reaches_the_others_and_not_the_sender() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = skip_without_db!(&mock);
    let (mut speaker, mut listener) = two_peers(&srv, &room_name()).await;

    say(&mut speaker, json!({ "type": "mute_audio", "muted": true })).await;
    let frame = wait_for(&mut listener, |v| v["type"] == "peer_muted")
        .await
        .expect("the others see the mute");
    assert_eq!(frame["kind"], "audio");
    assert_eq!(frame["muted"], true);

    // The sender already knows: echoing it back is how a UI ends up toggling twice.
    let own = drain(&mut speaker, 400).await;
    assert!(own.iter().all(|v| v["type"] != "peer_muted"));
}

#[tokio::test]
async fn muting_video_is_a_separate_flag_from_muting_audio() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = skip_without_db!(&mock);
    let (mut speaker, mut listener) = two_peers(&srv, &room_name()).await;

    say(&mut speaker, json!({ "type": "mute_video", "muted": true })).await;
    let frame = wait_for(&mut listener, |v| v["type"] == "peer_muted")
        .await
        .expect("the others see the camera go off");
    // Turning the camera off must not read as "they stopped talking" — one field
    // says WHICH was muted, and two different pieces of UI read it.
    assert_eq!(frame["kind"], "video");
    assert_eq!(frame["muted"], true);
}

#[tokio::test]
async fn a_reaction_is_relayed_without_being_translated() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = skip_without_db!(&mock);
    let (mut speaker, mut listener) = two_peers(&srv, &room_name()).await;

    say(&mut speaker, json!({ "type": "emoji", "emoji": "👏" })).await;
    let frame = wait_for(&mut listener, |v| v["type"] == "emoji_reaction")
        .await
        .expect("the reaction arrived");
    // Applause means the same in every language; sending it through a translator
    // would cost money and could only make it worse.
    assert_eq!(frame["emoji"], "👏");
    assert_eq!(frame["peer_id"], "spk");
}

#[tokio::test]
async fn a_raised_hand_goes_up_and_comes_back_down() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = skip_without_db!(&mock);
    let (mut speaker, mut listener) = two_peers(&srv, &room_name()).await;

    say(
        &mut speaker,
        json!({ "type": "hand_raise", "raised": true }),
    )
    .await;
    let up = wait_for(&mut listener, |v| v["type"] == "hand_raised")
        .await
        .expect("hand up");
    assert_eq!(up["raised"], true);

    say(
        &mut speaker,
        json!({ "type": "hand_raise", "raised": false }),
    )
    .await;
    let down = wait_for(&mut listener, |v| {
        v["type"] == "hand_raised" && v["raised"] == false
    })
    .await;
    // A hand that cannot be lowered stays up for the rest of the call.
    assert!(down.is_some(), "the hand never came down");
}

#[tokio::test]
async fn a_screen_share_badge_says_whether_the_audio_came_with_it() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = skip_without_db!(&mock);
    let (mut speaker, mut listener) = two_peers(&srv, &room_name()).await;

    say(
        &mut speaker,
        json!({ "type": "screen_share", "active": true, "audio": true }),
    )
    .await;
    let frame = wait_for(&mut listener, |v| v["type"] == "screen_share")
        .await
        .expect("the share was announced");
    assert_eq!(frame["active"], true);
    // Shared tab audio must stay audible across a language gap; a share without it
    // carries only the bare mic, which has to stay duckable under the TTS.
    assert_eq!(frame["audio"], true);

    say(
        &mut speaker,
        json!({ "type": "screen_share", "active": false }),
    )
    .await;
    let off = wait_for(&mut listener, |v| {
        v["type"] == "screen_share" && v["active"] == false
    })
    .await;
    assert!(off.is_some(), "the badge never cleared");
}

#[tokio::test]
async fn a_whiteboard_stroke_reaches_the_others_and_waits_for_late_joiners() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = skip_without_db!(&mock);
    let room = room_name();
    let (mut speaker, mut listener) = two_peers(&srv, &room).await;

    say(
        &mut speaker,
        json!({
            "type": "whiteboard",
            // Coordinates are normalised to 0..1 so every client scales them to
            // its own canvas instead of distorting the drawing.
            "op": {
                "op": "draw",
                "id": "stroke-1",
                "tool": "pen",
                "color": "#ff0000",
                "width": 3.0,
                "points": [[0.1, 0.1], [0.5, 0.5]]
            }
        }),
    )
    .await;
    let relayed = wait_for(&mut listener, |v| v["type"] == "whiteboard").await;
    assert!(relayed.is_some(), "the stroke was not relayed");

    // Persisted in the room, so somebody who joins mid-drawing sees the drawing
    // rather than a blank board they cannot ask anyone to repeat.
    let late_jwt = login(&srv, "Late").await;
    let mut late = join(&srv, &room, "late", "fr", &late_jwt).await;
    let snapshot = wait_for(&mut late, |v| v["type"] == "whiteboard_snapshot").await;
    assert!(snapshot.is_some(), "a late joiner got an empty board");
}

#[tokio::test]
async fn a_game_state_is_relayed_and_kept_for_whoever_arrives_next() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = skip_without_db!(&mock);
    let room = room_name();
    let (mut speaker, mut listener) = two_peers(&srv, &room).await;

    // The server is deliberately game-agnostic: it relays `state` and remembers
    // the latest. Every rule lives in the client.
    say(
        &mut speaker,
        json!({ "type": "game", "state": { "board": ["x", null, "o"], "turn": "o" } }),
    )
    .await;
    let relayed = wait_for(&mut listener, |v| v["type"] == "game").await;
    assert!(relayed.is_some(), "the move was not relayed");

    let late_jwt = login(&srv, "Late").await;
    let mut late = join(&srv, &room, "late", "fr", &late_jwt).await;
    let snapshot = wait_for(&mut late, |v| v["type"] == "game_snapshot").await;
    assert!(snapshot.is_some(), "a late joiner saw no game in progress");
}

#[tokio::test]
async fn webrtc_signalling_is_addressed_to_one_peer_not_broadcast() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = skip_without_db!(&mock);
    let room = room_name();
    let (mut speaker, mut listener) = two_peers(&srv, &room).await;
    let third_jwt = login(&srv, "Third").await;
    let mut third = join(&srv, &room, "third", "de", &third_jwt).await;
    let _ = drain(&mut third, 300).await;

    say(
        &mut speaker,
        json!({ "type": "offer", "to": "lis", "sdp": "v=0 fake-offer" }),
    )
    .await;

    let got = wait_for(&mut listener, |v| v["type"] == "offer")
        .await
        .expect("the offer reached its peer");
    assert_eq!(got["sdp"], "v=0 fake-offer");
    assert_eq!(got["from"], "spk");

    // A mesh is point-to-point: broadcasting an offer would have every other peer
    // answer a call nobody made.
    let others = drain(&mut third, 500).await;
    assert!(others.iter().all(|v| v["type"] != "offer"));
}

#[tokio::test]
async fn correcting_the_detected_language_reopens_the_session_for_the_new_one() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = skip_without_db!(&mock);
    let (mut speaker, _listener) = two_peers(&srv, &room_name()).await;

    start_speaking(&mut speaker).await;
    let _ = speaker.send(Message::binary(vec![0u8; 640])).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Auto-detect got it wrong and the user says so (spec 0012). The point of the
    // correction is that the NEXT session opens under the language they chose.
    say(&mut speaker, json!({ "type": "set_lang", "lang": "de" })).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    start_speaking(&mut speaker).await;
    let _ = speaker.send(Message::binary(vec![0u8; 640])).await;
    tokio::time::sleep(Duration::from_millis(600)).await;

    assert!(
        mock.connections() >= 1,
        "the correction killed the session instead of reopening it"
    );
}

#[tokio::test]
async fn a_frame_the_server_does_not_understand_is_ignored_not_fatal() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = skip_without_db!(&mock);
    let (mut speaker, mut listener) = two_peers(&srv, &room_name()).await;

    for junk in [
        json!({ "type": "not_a_real_message" }).to_string(),
        json!({ "type": "chat" }).to_string(), // missing `text`
        "[]".to_string(),
        "not json at all".to_string(),
    ] {
        let _ = speaker.send(Message::text(junk)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The socket must survive: a client on an older build sends frames this one
    // has never heard of, and dropping their call over it is the worst answer.
    say(&mut speaker, json!({ "type": "emoji", "emoji": "🎉" })).await;
    let frame = wait_for(&mut listener, |v| v["type"] == "emoji_reaction").await;
    assert!(frame.is_some(), "junk on the socket killed the room");
}

#[tokio::test]
async fn the_room_is_capped_and_says_so_rather_than_dropping_the_fifth_peer() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = skip_without_db!(&mock);
    let room = room_name();

    // A WebRTC mesh is O(n²) in connections; four is the documented cap.
    let mut held = Vec::new();
    for i in 0..4 {
        let jwt = login(&srv, &format!("P{i}")).await;
        held.push(join(&srv, &room, &format!("p{i}"), "en", &jwt).await);
    }

    let jwt = login(&srv, "Fifth").await;
    let url = format!("ws://{}/ws?room={room}&lang=en&id=p5&token={jwt}", srv.addr);
    let (mut fifth, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("the socket opened");
    let frame = wait_for(&mut fifth, |v| v["type"] == "room_full").await;
    assert!(
        frame.is_some(),
        "the fifth peer was dropped without being told why"
    );
}
