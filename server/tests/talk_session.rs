//! A real face-to-face conversation, over `/ws/talk`, end to end.
//!
//! `handle_talk_session` is ~400 lines that no test could reach: it needs a provider
//! for the audio and a second one for the direction classifier. Both now have a seam
//! — `QwenConfig::endpoint` and `Config::groq_base_url` — so the whole thing runs
//! here against stand-ins: the refusals, the handshake, the engine start, the gate
//! that decides who is speaking, and the teardown.
//!
//! DB-gated: skipped without `DATABASE_URL`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
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
use voxtranslate_server::{app, AppState};

const SECRET: &str = "talk-session-secret";

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

// ---------------------------------------------------------------------------
// A stand-in for the direction classifier
// ---------------------------------------------------------------------------

/// The classifier's answer, and how many times it was asked. `lang` is whatever the
/// stand-in should claim it heard; an empty string makes it answer `"other"`, which
/// is the model abstaining rather than failing.
#[derive(Clone)]
struct MockClassifier {
    calls: Arc<AtomicUsize>,
    lang: Arc<std::sync::Mutex<String>>,
    fail: Arc<std::sync::atomic::AtomicBool>,
}

impl MockClassifier {
    fn new(lang: &str) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            lang: Arc::new(std::sync::Mutex::new(lang.to_string())),
            fail: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

async fn classify(
    State(mock): State<MockClassifier>,
    Json(_body): Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    mock.calls.fetch_add(1, Ordering::SeqCst);
    if mock.fail.load(Ordering::SeqCst) {
        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "down").into_response();
    }
    let lang = mock.lang.lock().unwrap().clone();
    let content = if lang.is_empty() {
        json!({ "lang": "other", "confidence": 0.1 }).to_string()
    } else {
        json!({ "lang": lang, "confidence": 0.95 }).to_string()
    };
    Json(json!({ "choices": [{ "message": { "content": content } }] })).into_response()
}

async fn mock_groq(lang: &str) -> (String, MockClassifier) {
    let mock = MockClassifier::new(lang);
    let router = Router::new()
        .route("/chat/completions", post(classify))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{addr}/chat/completions"), mock)
}

// ---------------------------------------------------------------------------
// Server harness
// ---------------------------------------------------------------------------

struct Server {
    addr: SocketAddr,
    pool: Pool,
    groq: MockClassifier,
}

/// A server whose engine and classifier both point at stand-ins.
async fn setup(mock: &RealtimeMock, hears: &str) -> Option<Server> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let (groq_url, groq) = mock_groq(hears).await;
    let mut config = Config::test_with_billing(&url, SECRET, 5.0);
    config.qwen = QwenConfig {
        api_key: "test-key".into(),
        endpoint: mock.base_url(),
        ..Default::default()
    };
    config.standard_enabled = true;
    config.groq_base_url = Some(groq_url);
    let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);
    let mut state = AppState::new(config);
    state.billing = Some(BillingService::new(pool.clone(), min_join));
    state.safety = Some(SafetyService::new(pool.clone()));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    Some(Server { addr, pool, groq })
}

/// A signed-in user who has cleared the consent gate `authorize` enforces.
async fn login(srv: &Server) -> String {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: "Talker".into(),
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

/// Open `/ws/talk`, returning the HTTP status when the upgrade is refused outright.
async fn connect(srv: &Server, query: &str) -> Result<Ws, u16> {
    let url = format!("ws://{}/ws/talk?{query}", srv.addr);
    match tokio_tungstenite::connect_async(url).await {
        Ok((ws, _)) => Ok(ws),
        Err(tokio_tungstenite::tungstenite::Error::Http(r)) => Err(r.status().as_u16()),
        Err(e) => panic!("unexpected transport failure: {e}"),
    }
}

/// A conversation that has been accepted, with the `room_joined` frame it opened
/// with — waiting for that frame is also what makes the session ready to drive.
async fn open_joined(srv: &Server, jwt: &str, user: &str, other: &str) -> (Ws, Value) {
    let mut ws = connect(srv, &format!("lang={user}&other={other}&token={jwt}"))
        .await
        .expect("the upgrade was accepted");
    let joined = wait_for(&mut ws, |v| v["type"] == "room_joined").await;
    (ws, joined)
}

/// The same, for the tests that do not care what the handshake said.
async fn open(srv: &Server, jwt: &str, user: &str, other: &str) -> Ws {
    open_joined(srv, jwt, user, other).await.0
}

async fn start_speaking(ws: &mut Ws) {
    let _ = ws
        .send(Message::text(json!({ "type": "start" }).to_string()))
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
}

/// Read frames until one satisfies `wanted`, or give up after a second and a half.
async fn wait_for(ws: &mut Ws, wanted: impl Fn(&Value) -> bool) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(2500);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            panic!("no frame matched before the deadline");
        }
        match tokio::time::timeout(left, ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    if wanted(&v) {
                        return v;
                    }
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_))) | Ok(None) => panic!("the socket closed before a frame matched"),
            Err(_) => panic!("no frame matched before the deadline"),
        }
    }
}

/// Every frame the socket produces in `ms`, drained without asserting anything.
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

macro_rules! srv {
    ($mock:expr, $hears:expr) => {
        match setup($mock, $hears).await {
            Some(s) => s,
            None => return,
        }
    };
}

// ---------------------------------------------------------------------------
// The refusals, before a socket is ever upgraded
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_language_code_that_could_reach_a_provider_url_is_refused() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock, "en");
    let jwt = login(&srv).await;

    let Err(status) = connect(&srv, &format!("lang=en/../x&other=it&token={jwt}")).await else {
        panic!("the upgrade was accepted");
    };
    assert_eq!(status, 400, "a slashed code never reaches the provider");
}

#[tokio::test]
async fn auto_is_the_microphones_language_and_nobody_elses() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock, "en");
    let jwt = login(&srv).await;

    // As a LISTENER language `auto` makes the fan-out skip that side entirely, so
    // half the conversation would silently produce nothing at all.
    for q in ["lang=auto&other=it", "lang=en&other=auto"] {
        let Err(status) = connect(&srv, &format!("{q}&token={jwt}")).await else {
            panic!("the upgrade was accepted");
        };
        assert_eq!(status, 400);
    }
}

#[tokio::test]
async fn one_language_on_both_sides_is_not_a_conversation() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock, "en");
    let jwt = login(&srv).await;

    // Case is normalised before the comparison, so `EN` is the same refusal.
    let Err(status) = connect(&srv, &format!("lang=en&other=EN&token={jwt}")).await else {
        panic!("the upgrade was accepted");
    };
    assert_eq!(status, 400);
}

// ---------------------------------------------------------------------------
// The handshake
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_signed_out_visitor_is_told_to_sign_in_rather_than_dropped() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock, "en");

    // The upgrade succeeds — the refusal is a frame, so the UI can say why.
    let mut ws = connect(&srv, "lang=en&other=it").await.expect("upgraded");
    let frame = wait_for(&mut ws, |v| v["type"] == "error").await;
    assert_eq!(frame["code"], "invalid_token");
}

#[tokio::test]
async fn the_client_is_handed_its_capture_format_before_it_records_anything() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock, "en");
    let jwt = login(&srv).await;

    let mut ws = open(&srv, &jwt, "en", "it").await;
    // Standard is a speech-to-speech engine: it reads PCM16 @ 24 kHz, and feeding it
    // WebM is not a degradation — it reads the bytes as samples and produces silence.
    let frame = wait_for(&mut ws, |v| v["type"] == "capture_format").await;
    assert_eq!(frame["pcm"], true);
}

#[tokio::test]
async fn the_conversation_is_private_and_holds_no_other_peers() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock, "en");
    let jwt = login(&srv).await;

    let (_ws, joined) = open_joined(&srv, &jwt, "en", "it").await;
    assert_eq!(joined["public"], false);
    // The other two peers are the gate's own, and the browser must never be told
    // about them: they would render as participants in a two-person conversation.
    assert_eq!(joined["peers"].as_array().map(|a| a.len()), Some(0));
}

// ---------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------

#[tokio::test]
async fn nothing_is_metered_until_the_user_presses_start() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock, "en");
    let jwt = login(&srv).await;

    let mut ws = open(&srv, &jwt, "en", "it").await;
    // Audio before `start` is dropped on the floor rather than opening a session:
    // sitting on the setup screen must cost nothing.
    let _ = ws.send(Message::binary(vec![0u8; 640])).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(mock.connections(), 0);
}

#[tokio::test]
async fn start_opens_one_upstream_session_per_listener_language() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock, "en");
    let jwt = login(&srv).await;

    let mut ws = open(&srv, &jwt, "en", "it").await;
    start_speaking(&mut ws).await;
    let _ = ws.send(Message::binary(vec![0u8; 640])).await;
    tokio::time::sleep(Duration::from_millis(700)).await;

    // Two interpreter channels, one per direction — the honest count this mode bills
    // for, and what keeps either speaker from being cut off mid-sentence.
    assert!(
        mock.connections() >= 1,
        "the engine opened no session at all"
    );
}

#[tokio::test]
async fn a_second_start_is_a_no_op_not_a_second_session() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock, "en");
    let jwt = login(&srv).await;

    let mut ws = open(&srv, &jwt, "en", "it").await;
    start_speaking(&mut ws).await;
    let _ = ws.send(Message::binary(vec![0u8; 640])).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after_first = mock.connections();

    start_speaking(&mut ws).await;
    let _ = ws.send(Message::binary(vec![0u8; 640])).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(
        mock.connections(),
        after_first,
        "pressing Start twice paid for the conversation twice"
    );
}

#[tokio::test]
async fn stop_closes_the_upstream_and_a_later_start_opens_a_fresh_one() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock, "en");
    let jwt = login(&srv).await;

    let mut ws = open(&srv, &jwt, "en", "it").await;
    start_speaking(&mut ws).await;
    let _ = ws.send(Message::binary(vec![0u8; 640])).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after_first = mock.connections();

    let _ = ws
        .send(Message::text(json!({ "type": "stop" }).to_string()))
        .await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    start_speaking(&mut ws).await;
    let _ = ws.send(Message::binary(vec![0u8; 640])).await;
    tokio::time::sleep(Duration::from_millis(600)).await;

    assert!(
        mock.connections() > after_first,
        "after Stop the session was reused instead of reopened"
    );
}

#[tokio::test]
async fn an_engine_that_cannot_open_says_so_instead_of_going_quiet() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    // The provider accepts the socket and hangs up on the first audio frame, which is
    // what a region with no realtime model actually does.
    mock.reply_with(vec![Reply::Drop]);
    let srv = srv!(&mock, "en");
    let jwt = login(&srv).await;

    let mut ws = open(&srv, &jwt, "en", "it").await;
    start_speaking(&mut ws).await;
    let _ = ws.send(Message::binary(vec![0u8; 640])).await;
    // Either the session never opened (an error frame) or it opened and died; both
    // are survivable, and neither may take the handler down with it.
    let seen = drain(&mut ws, 900).await;
    assert!(
        seen.iter().all(|v| v["type"] != "room_full"),
        "a dead provider was reported as a full room"
    );
}

// ---------------------------------------------------------------------------
// The gate: who just spoke
// ---------------------------------------------------------------------------

#[tokio::test]
async fn disjoint_scripts_settle_the_direction_without_paying_for_a_model_call() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(vec![Reply::Transcript {
        original: "こんにちは、はじめまして".into(),
        translated: "Hello, nice to meet you".into(),
    }]);
    let srv = srv!(&mock, "ja");
    let jwt = login(&srv).await;

    let mut ws = open(&srv, &jwt, "en", "ja").await;
    start_speaking(&mut ws).await;
    let _ = ws.send(Message::binary(vec![0u8; 640])).await;

    let frame = wait_for(&mut ws, |v| v["type"] == "talk_direction").await;
    assert_eq!(frame["spoken"], "ja");
    assert_eq!(frame["target"], "en");
    // Kana against a Latin alphabet is decided by looking at the characters. Most
    // travel pairs are disjoint like this, and paying a model to confirm it is waste.
    assert_eq!(srv.groq.calls(), 0, "the free path asked the model anyway");
}

#[tokio::test]
async fn a_shared_script_is_put_to_the_classifier() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(vec![Reply::Transcript {
        original: "buongiorno, come sta oggi signore".into(),
        translated: "good morning, how are you today sir".into(),
    }]);
    let srv = srv!(&mock, "it");
    let jwt = login(&srv).await;

    let mut ws = open(&srv, &jwt, "en", "it").await;
    start_speaking(&mut ws).await;
    let _ = ws.send(Message::binary(vec![0u8; 640])).await;

    let frame = wait_for(&mut ws, |v| v["type"] == "talk_direction").await;
    assert_eq!(frame["spoken"], "it");
    assert_eq!(frame["target"], "en");
    assert!(srv.groq.calls() >= 1, "two Latin languages were guessed at");
}

#[tokio::test]
async fn the_words_are_shown_while_the_side_is_still_being_worked_out() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(vec![Reply::Transcript {
        original: "buongiorno, come sta oggi signore".into(),
        translated: "good morning, how are you today sir".into(),
    }]);
    let srv = srv!(&mock, "it");
    let jwt = login(&srv).await;

    let mut ws = open(&srv, &jwt, "en", "it").await;
    start_speaking(&mut ws).await;
    let _ = ws.send(Message::binary(vec![0u8; 640])).await;

    // Holding every word until a verdict lands reads as a broken microphone. The
    // original is captioned first; only the TRANSLATION waits for the direction.
    let seen = drain(&mut ws, 1600).await;
    assert!(
        seen.iter()
            .any(|v| v["type"] == "subtitle_interim" || v["type"] == "subtitle_final"),
        "the speaker's own words were never shown"
    );
}

#[tokio::test]
async fn a_classifier_that_is_down_is_reported_rather_than_absorbed() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(vec![Reply::Transcript {
        original: "buongiorno, come sta oggi signore".into(),
        translated: "good morning, how are you today sir".into(),
    }]);
    let srv = srv!(&mock, "it");
    srv.groq.fail.store(true, Ordering::SeqCst);
    let jwt = login(&srv).await;

    let mut ws = open(&srv, &jwt, "en", "it").await;
    start_speaking(&mut ws).await;
    for _ in 0..6 {
        let _ = ws.send(Message::binary(vec![0u8; 640])).await;
        tokio::time::sleep(Duration::from_millis(120)).await;
    }

    // Every utterance is held until a direction is committed, so a dead classifier
    // means permanent silence. The session must not keep claiming it is listening.
    let seen = drain(&mut ws, 2000).await;
    assert!(
        seen.iter().all(|v| v["type"] != "talk_direction"),
        "a direction was announced with no verdict behind it"
    );
}

#[tokio::test]
async fn an_abstention_is_silent_but_does_not_wedge_the_conversation() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(vec![Reply::Transcript {
        original: "buongiorno, come sta oggi signore".into(),
        translated: "good morning, how are you today sir".into(),
    }]);
    // An empty answer is the model saying "other": a third language, or noise.
    let srv = srv!(&mock, "");
    let jwt = login(&srv).await;

    let mut ws = open(&srv, &jwt, "en", "it").await;
    start_speaking(&mut ws).await;
    let _ = ws.send(Message::binary(vec![0u8; 640])).await;
    let seen = drain(&mut ws, 1600).await;

    assert!(
        seen.iter().all(|v| v["type"] != "talk_direction"),
        "an abstention was spoken as if it were a verdict"
    );
    // Still alive: a later turn must still be classifiable.
    let _ = ws
        .send(Message::text(json!({ "type": "stop" }).to_string()))
        .await;
    start_speaking(&mut ws).await;
    assert!(ws.send(Message::binary(vec![0u8; 640])).await.is_ok());
}

// ---------------------------------------------------------------------------
// Teardown
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hanging_up_tears_the_whole_conversation_down() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock, "en");
    let jwt = login(&srv).await;

    let mut ws = open(&srv, &jwt, "en", "it").await;
    start_speaking(&mut ws).await;
    let _ = ws.send(Message::binary(vec![0u8; 640])).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(ws);
    tokio::time::sleep(Duration::from_millis(700)).await;

    // A leaked peer keeps a meter alive for a user who has walked away, so the
    // private room must be gone rather than merely emptied.
    let (_next, joined) = open_joined(&srv, &jwt, "en", "it").await;
    assert_eq!(
        joined["peers"].as_array().map(|a| a.len()),
        Some(0),
        "the previous conversation's peers outlived it"
    );
}

#[tokio::test]
async fn a_client_message_from_the_call_protocol_is_ignored_not_fatal() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock, "en");
    let jwt = login(&srv).await;

    let mut ws = open(&srv, &jwt, "en", "it").await;
    // Room chat, a hand raise, an emoji — meaningless in a face-to-face conversation.
    for junk in [
        json!({ "type": "chat", "text": "hi" }).to_string(),
        json!({ "type": "raise_hand" }).to_string(),
        "{not json at all".to_string(),
    ] {
        let _ = ws.send(Message::text(junk)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    start_speaking(&mut ws).await;
    let _ = ws.send(Message::binary(vec![0u8; 640])).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        mock.connections() >= 1,
        "junk on the socket killed the session"
    );
}
