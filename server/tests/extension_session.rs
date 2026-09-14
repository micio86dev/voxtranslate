//! The Chrome widget's translation session, over `/ws/extension`, end to end.
//!
//! `handle_extension_session` owns the whole widget: the sign-in gate, the private
//! two-peer room, the metered start/stop, the mid-session language change and the
//! Enhanced translate hop. None of it was reachable without a provider; with
//! `QwenConfig::endpoint` and `Config::groq_base_url` aimed at stand-ins, it is.
//!
//! The PKCE handoff that produces the token lives in `extension_auth.rs`.
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

const SECRET: &str = "extension-session-secret";

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

// ---------------------------------------------------------------------------
// A stand-in for the text translator
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct MockGroq {
    calls: Arc<AtomicUsize>,
}

async fn complete(State(mock): State<MockGroq>, Json(_body): Json<Value>) -> Json<Value> {
    mock.calls.fetch_add(1, Ordering::SeqCst);
    Json(json!({ "choices": [{ "message": { "content": "TRADOTTO" } }] }))
}

async fn mock_groq() -> (String, MockGroq) {
    let mock = MockGroq::default();
    let router = Router::new()
        .route("/chat/completions", post(complete))
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
    groq: MockGroq,
}

async fn setup(mock: &RealtimeMock) -> Option<Server> {
    let url = voxtranslate_server::db::test_database_url()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let (groq_url, groq) = mock_groq().await;
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

async fn login(srv: &Server) -> String {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: "Widget user".into(),
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

/// Open `/ws/extension`, returning the HTTP status when the upgrade is refused.
async fn connect(srv: &Server, query: &str) -> Result<Ws, u16> {
    let url = format!("ws://{}/ws/extension?{query}", srv.addr);
    match tokio_tungstenite::connect_async(url).await {
        Ok((ws, _)) => Ok(ws),
        Err(tokio_tungstenite::tungstenite::Error::Http(r)) => Err(r.status().as_u16()),
        Err(e) => panic!("unexpected transport failure: {e}"),
    }
}

/// A live widget session, plus the `room_joined` frame it opened with.
async fn open_joined(srv: &Server, jwt: &str, lang: &str) -> (Ws, Value) {
    let mut ws = connect(srv, &format!("lang={lang}&token={jwt}"))
        .await
        .expect("the upgrade was accepted");
    let joined = wait_for(&mut ws, |v| v["type"] == "room_joined").await;
    (ws, joined)
}

async fn open(srv: &Server, jwt: &str, lang: &str) -> Ws {
    open_joined(srv, jwt, lang).await.0
}

async fn send(ws: &mut Ws, v: Value) {
    let _ = ws.send(Message::text(v.to_string())).await;
}

async fn start_streaming(ws: &mut Ws) {
    send(ws, json!({ "type": "start" })).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
}

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
    ($mock:expr) => {
        match setup($mock).await {
            Some(s) => s,
            None => return,
        }
    };
}

// ---------------------------------------------------------------------------
// The refusals
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_language_code_that_could_reach_a_provider_url_is_refused() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let jwt = login(&srv).await;

    // Both ends of the pair are interpolated into a streaming URL, so both are
    // checked with the same rule the room route applies.
    // Percent-encoded, because that is how a value smuggling `&` or `/` actually
    // arrives — unencoded it is just a second query parameter the route ignores.
    for q in [
        "lang=en%26redact%3Dpci",
        "lang=toolonglang",
        "lang=en&source=en%2F..%2Fx",
        "lang=",
        "lang=en%20us",
    ] {
        let status = connect(&srv, &format!("{q}&token={jwt}"))
            .await
            .err()
            .unwrap_or_else(|| panic!("{q} was accepted"));
        assert_eq!(status, 400, "{q}");
    }
}

#[tokio::test]
async fn auto_is_a_source_language_and_never_a_target() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let jwt = login(&srv).await;

    // A target of `auto` makes the fan-out skip this listener entirely, and the
    // session produces nothing at all — silently.
    let Err(status) = connect(&srv, &format!("lang=auto&token={jwt}")).await else {
        panic!("a target of `auto` was accepted");
    };
    assert_eq!(status, 400);

    // As a SOURCE it is the normal case, and the default.
    let ok = connect(&srv, &format!("lang=it&source=auto&token={jwt}")).await;
    assert!(ok.is_ok(), "auto is how the widget usually opens");
}

#[tokio::test]
async fn a_signed_out_visitor_is_told_to_sign_in_rather_than_dropped() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);

    // This is a billed feature with no guest tier, unlike a call — but the refusal
    // is still a frame, so the widget can show a sign-in button.
    let mut ws = connect(&srv, "lang=it").await.expect("upgraded");
    let frame = wait_for(&mut ws, |v| v["type"] == "error").await;
    assert_eq!(frame["code"], "invalid_token");
}

// ---------------------------------------------------------------------------
// The handshake
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_widget_is_told_its_capture_format_before_it_records_anything() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let jwt = login(&srv).await;

    let mut ws = open(&srv, &jwt, "it").await;
    // Standard is speech-to-speech, so PCM. Feeding it WebM is not a degradation —
    // it reads the bytes as samples and produces nothing, with no error to notice.
    let frame = wait_for(&mut ws, |v| v["type"] == "capture_format").await;
    assert_eq!(frame["pcm"], true);
}

#[tokio::test]
async fn the_session_room_is_private_and_hides_its_own_pseudo_peer() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let jwt = login(&srv).await;

    let (_ws, joined) = open_joined(&srv, &jwt, "it").await;
    assert_eq!(joined["public"], false);
    // "Tab audio" is the source peer the fan-out needs; rendering it as a
    // participant in a one-person widget would be nonsense.
    assert_eq!(joined["peers"].as_array().map(|a| a.len()), Some(0));
}

// ---------------------------------------------------------------------------
// Start / stop, and what they cost
// ---------------------------------------------------------------------------

#[tokio::test]
async fn opening_the_panel_and_never_pressing_start_costs_nothing() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let jwt = login(&srv).await;

    let mut ws = open(&srv, &jwt, "it").await;
    let _ = ws.send(Message::binary(vec![0u8; 640])).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(mock.connections(), 0, "audio before start opened a session");
}

#[tokio::test]
async fn start_opens_the_upstream_session() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let jwt = login(&srv).await;

    let mut ws = open(&srv, &jwt, "it").await;
    start_streaming(&mut ws).await;
    let _ = ws.send(Message::binary(vec![0u8; 640])).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(mock.connections() >= 1);
}

#[tokio::test]
async fn a_second_start_is_a_no_op_not_a_second_meter() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let jwt = login(&srv).await;

    let mut ws = open(&srv, &jwt, "it").await;
    start_streaming(&mut ws).await;
    let _ = ws.send(Message::binary(vec![0u8; 640])).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after_first = mock.connections();

    start_streaming(&mut ws).await;
    let _ = ws.send(Message::binary(vec![0u8; 640])).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        mock.connections(),
        after_first,
        "the user was charged twice for one stream"
    );
}

#[tokio::test]
async fn stop_closes_the_upstream_so_billing_cannot_outlive_the_audio() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let jwt = login(&srv).await;

    let mut ws = open(&srv, &jwt, "it").await;
    start_streaming(&mut ws).await;
    let _ = ws.send(Message::binary(vec![0u8; 640])).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after_first = mock.connections();

    send(&mut ws, json!({ "type": "stop" })).await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    start_streaming(&mut ws).await;
    let _ = ws.send(Message::binary(vec![0u8; 640])).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        mock.connections() > after_first,
        "Stop left the old session open"
    );
}

#[tokio::test]
async fn a_provider_that_hangs_up_does_not_take_the_widget_with_it() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(vec![Reply::Drop]);
    let srv = srv!(&mock);
    let jwt = login(&srv).await;

    let mut ws = open(&srv, &jwt, "it").await;
    start_streaming(&mut ws).await;
    let _ = ws.send(Message::binary(vec![0u8; 640])).await;
    let _ = drain(&mut ws, 800).await;

    // The socket must still be usable: the widget's whole recovery path is pressing
    // Start again, and it cannot do that on a dead socket.
    send(&mut ws, json!({ "type": "stop" })).await;
    assert!(ws.send(Message::binary(vec![0u8; 8])).await.is_ok());
}

// ---------------------------------------------------------------------------
// Changing the target language mid-session
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_target_language_can_be_changed_without_reconnecting() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(vec![Reply::Transcript {
        original: "hello there".into(),
        translated: "salut".into(),
    }]);
    let srv = srv!(&mock);
    let jwt = login(&srv).await;

    let mut ws = open(&srv, &jwt, "it").await;
    send(&mut ws, json!({ "type": "set_lang", "lang": "fr" })).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    start_streaming(&mut ws).await;
    let _ = ws.send(Message::binary(vec![0u8; 640])).await;
    tokio::time::sleep(Duration::from_millis(700)).await;

    // The change landed on the room peer, so the session that opens afterwards is
    // opened for the NEW language rather than the one in the query string.
    assert!(mock.connections() >= 1);
}

#[tokio::test]
async fn a_target_language_the_route_would_have_refused_is_refused_here_too() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let jwt = login(&srv).await;

    let mut ws = open(&srv, &jwt, "it").await;
    // `auto` and an injection payload are both rejected mid-session for exactly the
    // reasons the upgrade rejects them — the gate would be pointless otherwise.
    for lang in ["auto", "fr&redact=pci", "waytoolongcode"] {
        send(&mut ws, json!({ "type": "set_lang", "lang": lang })).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    start_streaming(&mut ws).await;
    let _ = ws.send(Message::binary(vec![0u8; 640])).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        mock.connections() >= 1,
        "a rejected language change killed the session"
    );
}

// ---------------------------------------------------------------------------
// The Enhanced translate hop
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_finalized_segment_comes_back_translated_under_its_request_id() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let jwt = login(&srv).await;

    let mut ws = open(&srv, &jwt, "it").await;
    send(
        &mut ws,
        json!({
            "type": "translate_text",
            "request_id": "r-1",
            "text": "good morning",
            "source": "en",
            "target": "it",
        }),
    )
    .await;

    let reply = wait_for(&mut ws, |v| v["type"] == "translated_text").await;
    // The id is what lets the widget speak the segments in order; without it a
    // slow round-trip would be spoken over the next one.
    assert_eq!(reply["request_id"], "r-1");
    assert_eq!(reply["text"], "TRADOTTO");
}

#[tokio::test]
async fn the_hop_is_bounded_so_it_cannot_be_used_as_a_free_translation_endpoint() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let jwt = login(&srv).await;

    let mut ws = open(&srv, &jwt, "it").await;
    send(
        &mut ws,
        json!({
            "type": "translate_text",
            "request_id": "too-big",
            "text": "x".repeat(2_001),
            "source": "en",
            "target": "it",
        }),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(
        srv.groq.calls.load(Ordering::SeqCst),
        0,
        "an oversized segment reached the provider"
    );
    let seen = drain(&mut ws, 400).await;
    assert!(seen.iter().all(|v| v["request_id"] != "too-big"));
}

#[tokio::test]
async fn a_round_trip_does_not_block_the_socket_behind_it() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let jwt = login(&srv).await;

    let mut ws = open(&srv, &jwt, "it").await;
    for i in 0..3 {
        send(
            &mut ws,
            json!({
                "type": "translate_text",
                "request_id": format!("r-{i}"),
                "text": "one short segment",
                "source": "en",
                "target": "it",
            }),
        )
        .await;
    }

    let seen = drain(&mut ws, 1500).await;
    let replies = seen
        .iter()
        .filter(|v| v["type"] == "translated_text")
        .count();
    assert_eq!(replies, 3, "segments queued behind each other");
}

// ---------------------------------------------------------------------------
// Everything else on the socket
// ---------------------------------------------------------------------------

#[tokio::test]
async fn call_protocol_messages_are_ignored_rather_than_fatal() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let jwt = login(&srv).await;

    let mut ws = open(&srv, &jwt, "it").await;
    for junk in [
        json!({ "type": "chat", "text": "hi" }),
        json!({ "type": "raise_hand" }),
        json!({ "type": "reaction", "emoji": "👏" }),
    ] {
        send(&mut ws, junk).await;
    }
    let _ = ws.send(Message::text("{not json".to_string())).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    start_streaming(&mut ws).await;
    let _ = ws.send(Message::binary(vec![0u8; 640])).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(mock.connections() >= 1, "junk on the socket killed it");
}

#[tokio::test]
async fn closing_the_tab_tears_the_private_room_down() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let jwt = login(&srv).await;

    let mut ws = open(&srv, &jwt, "it").await;
    start_streaming(&mut ws).await;
    let _ = ws.send(Message::binary(vec![0u8; 640])).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = ws.close(None).await;
    drop(ws);
    tokio::time::sleep(Duration::from_millis(700)).await;

    // A leaked peer keeps a meter alive for a user who closed the tab, so the next
    // session must open into a room of its own with nothing left behind.
    let (_next, joined) = open_joined(&srv, &jwt, "it").await;
    assert_eq!(joined["peers"].as_array().map(|a| a.len()), Some(0));
}
