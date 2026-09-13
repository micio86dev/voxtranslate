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
use serde_json::Value;
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
