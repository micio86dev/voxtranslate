//! `/ws/live` (spec 0121, S2): two phones, two sockets, two independent translation streams.
//!
//! The engine is the real Standard engine pointed at the realtime mock, so these exercise the
//! genuine room, reconcile and fan-out code — only the upstream provider is a stand-in.
//!
//! DB-gated like the other integration suites. Run against the local pgvector Postgres:
//! `DATABASE_URL=postgres://vox:vox_local_dev@127.0.0.1:55432/voxtranslate_voip_test \
//!   cargo test --test live_ws`. A run of a few milliseconds skipped everything.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt as _, StreamExt as _};
use reqwest::StatusCode;
use rust_decimal::Decimal;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::{Config, GeminiConfig, QwenConfig};
use voxtranslate_server::db::{self, Pool};
use voxtranslate_server::engine::realtime_mock::{Dialect, RealtimeMock, Reply};
use voxtranslate_server::live::config::LiveConfig;
use voxtranslate_server::live::sweeper::sweep_sessions;
use voxtranslate_server::live::LiveState;
use voxtranslate_server::protocol::ServerMessage;
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::transcripts::TranscriptService;
use voxtranslate_server::{app, AppState};

const SECRET: &str = "live-ws-secret";
const LIVE_KEY: &str = "0123456789abcdef0123456789abcdef-live-ws";

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The latency histogram on `/metrics` is process-wide, so two tests that both feed it voiced
/// audio would count each other's utterances. Whoever measures it, or makes voiced speech, holds
/// this for its whole run.
static VOICED_SPEECH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Srv {
    addr: SocketAddr,
    pool: Pool,
    http: reqwest::Client,
    state: AppState,
}

async fn setup(mock: &RealtimeMock) -> Option<Srv> {
    setup_with(mock, None).await
}

/// `min_join` overrides the balance a user needs to start; `Some(0.0)` lets a test hand an
/// owner an empty wallet and watch the conversation end.
async fn setup_with(mock: &RealtimeMock, min_join: Option<f64>) -> Option<Srv> {
    setup_full(mock, min_join, None).await
}

/// Standard on `mock`, and Premium (Gemini Live) on `premium.0` with at most `premium.1`
/// concurrent upstream sessions. Premium is $0.06 x 1.5 = $0.09 a minute, twenty times Standard.
async fn setup_tiers(
    standard: &RealtimeMock,
    premium: &RealtimeMock,
    premium_sessions: usize,
) -> Option<Srv> {
    setup_full(standard, None, Some((premium, premium_sessions))).await
}

async fn setup_full(
    mock: &RealtimeMock,
    min_join: Option<f64>,
    premium: Option<(&RealtimeMock, usize)>,
) -> Option<Srv> {
    let url = db::test_database_url()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let mut config = Config::test_with_billing(&url, SECRET, 5.0);
    config.qwen = QwenConfig {
        api_key: "test-key".into(),
        endpoint: mock.base_url(),
        ..Default::default()
    };
    config.standard_enabled = true;
    if let Some((gem, max_sessions)) = premium {
        config.google = Some(GeminiConfig {
            api_key: "test-key".into(),
            model: "gemini-test".into(),
            cost_per_minute: 0.06,
            markup: 0.5,
            max_sessions,
            live_base_url: gem.base_url(),
            voice: None,
        });
    }
    {
        // Tick every second, so a few seconds of speech produce several charges.
        let pricing = &mut config.billing.as_mut().unwrap().pricing;
        pricing.usage_update_interval = 1;
        if let Some(m) = min_join {
            pricing.min_balance_to_join = m;
        }
    }
    let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);
    let mut state = AppState::new(config);
    state.billing = Some(BillingService::new(pool.clone(), min_join));
    state.safety = Some(SafetyService::new(pool.clone()));
    state.transcripts = Some(TranscriptService::new(pool.clone()));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);
    state.live = Arc::new(LiveState::new(
        LiveConfig::load(|k| match k {
            "LIVE_CONNECT_ENABLED" => Some("1".into()),
            "LIVE_TOKEN_KEY" => Some(LIVE_KEY.into()),
            // The shortest grace the config allows, so the sweeper tests do not take a minute.
            "LIVE_RECONNECT_GRACE_SECS" => Some("5".into()),
            _ => None,
        })
        .unwrap(),
    ));
    let served = state.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(served)).await;
    });
    Some(Srv {
        addr,
        pool,
        http: reqwest::Client::new(),
        state,
    })
}

macro_rules! srv {
    ($mock:expr) => {
        match setup($mock).await {
            Some(s) => s,
            None => {
                eprintln!("skipping — no DATABASE_URL");
                return;
            }
        }
    };
}

fn speech() -> Vec<Reply> {
    vec![
        Reply::Transcript {
            original: "ciao a tutti".into(),
            translated: "hola a todos".into(),
        },
        Reply::Audio(vec![7u8; 960]),
    ]
}

impl Srv {
    async fn post(&self, path: &str, bearer: Option<&str>, body: Value) -> (StatusCode, Value) {
        let mut req = self
            .http
            .post(format!("http://{}{path}", self.addr))
            .json(&body);
        if let Some(t) = bearer {
            req = req.bearer_auth(t);
        }
        let res = req.send().await.unwrap();
        let status = res.status();
        (status, res.json().await.unwrap_or(Value::Null))
    }
}

async fn login(srv: &Srv) -> (String, Uuid) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: "Marco".into(),
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
    (
        issue_jwt(SECRET, &user.id, &user.email, &user.name, 168).unwrap(),
        user.id,
    )
}

/// One conversation: a host (`it`) and a guest (`es`), each with a token, no sockets yet.
struct Convo {
    session: Uuid,
    host_token: String,
    host_peer: String,
    guest_token: String,
    guest_peer: String,
    host_user: Uuid,
}

async fn convo(srv: &Srv, save_transcript: bool) -> Convo {
    let (jwt, host_user) = login(srv).await;
    let (s, created) = srv
        .post(
            "/api/live/sessions",
            Some(&jwt),
            json!({ "lang": "it", "save_transcript": save_transcript }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{created}");
    let code = created["invite_url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_string();
    let (s, joined) = srv
        .post(
            "/api/live/join",
            None,
            json!({ "code": code, "name": "Lucia", "lang": "es", "consent": true }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{joined}");
    Convo {
        session: created["session_id"].as_str().unwrap().parse().unwrap(),
        host_token: created["token"].as_str().unwrap().into(),
        host_peer: created["peer_id"].as_str().unwrap().into(),
        guest_token: joined["token"].as_str().unwrap().into(),
        guest_peer: joined["peer_id"].as_str().unwrap().into(),
        host_user,
    }
}

async fn connect(srv: &Srv, token: &str) -> Result<Ws, StatusCode> {
    match tokio_tungstenite::connect_async(format!("ws://{}/ws/live?token={token}", srv.addr)).await
    {
        Ok((ws, _)) => Ok(ws),
        Err(WsError::Http(res)) => Err(StatusCode::from_u16(res.status().as_u16()).unwrap()),
        Err(e) => panic!("unexpected connect error: {e}"),
    }
}

/// Connect and wait for the server's `room_joined`.
async fn open(srv: &Srv, token: &str) -> Ws {
    let mut ws = connect(srv, token).await.expect("upgrade");
    wait_for(&mut ws, |v| v["type"] == "room_joined")
        .await
        .expect("room_joined");
    ws
}

async fn wait_for(ws: &mut Ws, wanted: impl Fn(&Value) -> bool) -> Option<Value> {
    while let Ok(Some(Ok(frame))) = tokio::time::timeout(Duration::from_secs(8), ws.next()).await {
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

async fn say(ws: &mut Ws, v: Value) {
    let _ = ws.send(Message::text(v.to_string())).await;
}

/// Both phones send a 100 ms frame every tick for `ms`, and everything each one receives is
/// returned. Starting speech opens the upstream sessions; the room reconciles them within a
/// second, so the audio has to keep flowing while we listen.
async fn both_speak(a: &mut Ws, b: &mut Ws, ms: u64) -> (Vec<Value>, Vec<Value>) {
    let (mut got_a, mut got_b) = (Vec::new(), Vec::new());
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let _ = a.send(Message::binary(vec![0u8; 640])).await;
                let _ = b.send(Message::binary(vec![0u8; 640])).await;
            }
            Some(Ok(Message::Text(t))) = a.next() => {
                if let Ok(v) = serde_json::from_str(t.as_str()) { got_a.push(v); }
            }
            Some(Ok(Message::Text(t))) = b.next() => {
                if let Ok(v) = serde_json::from_str(t.as_str()) { got_b.push(v); }
            }
            _ = tokio::time::sleep_until(deadline) => return (got_a, got_b),
        }
    }
}

fn audio_from<'a>(frames: &'a [Value], speaker: &str) -> Vec<&'a Value> {
    frames
        .iter()
        .filter(|v| v["type"] == "translated_audio" && v["speaker_id"] == speaker)
        .collect()
}

// ---- the point of the feature ----------------------------------------------------------

#[tokio::test]
async fn both_phones_are_translated_at_the_same_time_in_opposite_directions() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(speech());
    let srv = srv!(&mock);
    let c = convo(&srv, false).await;

    let mut host = open(&srv, &c.host_token).await;
    let mut guest = open(&srv, &c.guest_token).await;
    say(&mut host, json!({ "type": "start" })).await;
    say(&mut guest, json!({ "type": "start" })).await;

    // No turn-taking: both start before either has been heard, and both keep talking.
    let (host_got, guest_got) = both_speak(&mut host, &mut guest, 4000).await;

    let to_host = audio_from(&host_got, &c.guest_peer);
    let to_guest = audio_from(&guest_got, &c.host_peer);
    assert!(
        !to_host.is_empty(),
        "the host never heard the guest: {host_got:?}"
    );
    assert!(
        !to_guest.is_empty(),
        "the guest never heard the host: {guest_got:?}"
    );
    assert!(
        to_host.iter().all(|v| v["lang"] == "it"),
        "the host hears Italian"
    );
    assert!(
        to_guest.iter().all(|v| v["lang"] == "es"),
        "the guest hears Spanish"
    );

    // Nobody is played their own voice back.
    assert!(audio_from(&host_got, &c.host_peer).is_empty());
    assert!(audio_from(&guest_got, &c.guest_peer).is_empty());
}

#[tokio::test]
async fn a_muted_phone_sends_nothing_to_the_engine() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(speech());
    let srv = srv!(&mock);
    let c = convo(&srv, false).await;
    let mut host = open(&srv, &c.host_token).await;
    let mut guest = open(&srv, &c.guest_token).await;

    say(&mut host, json!({ "type": "start" })).await;
    say(&mut host, json!({ "type": "mute_audio", "muted": true })).await;
    // The guest is told, so the UI can show it.
    let seen = wait_for(&mut guest, |v| v["type"] == "peer_muted")
        .await
        .expect("peer_muted");
    assert_eq!(seen["muted"], true);

    // Muted: however much the client keeps capturing, the engine hears none of it.
    let (_, _) = both_speak(&mut host, &mut guest, 2500).await;
    assert!(
        !mock.saw_type("input_audio_buffer.append"),
        "audio reached the engine while muted"
    );

    // Unmuted: it flows again.
    say(&mut host, json!({ "type": "mute_audio", "muted": false })).await;
    let (_, _) = both_speak(&mut host, &mut guest, 2500).await;
    assert!(
        mock.saw_type("input_audio_buffer.append"),
        "audio never resumed"
    );
}

// ---- seats and credentials -------------------------------------------------------------

#[tokio::test]
async fn a_socket_needs_a_live_token_for_a_live_session() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let c = convo(&srv, false).await;

    assert_eq!(
        connect(&srv, "nonsense").await.err(),
        Some(StatusCode::UNAUTHORIZED)
    );
    // The account JWT is not a Live Connect token.
    let (jwt, _) = login(&srv).await;
    assert_eq!(
        connect(&srv, &jwt).await.err(),
        Some(StatusCode::UNAUTHORIZED)
    );

    // An ended conversation admits nobody, however valid the token was.
    let (s, _) = srv
        .post(
            &format!("/api/live/sessions/{}/end", c.session),
            Some(&c.host_token),
            json!({}),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        connect(&srv, &c.guest_token).await.err(),
        Some(StatusCode::GONE)
    );
}

#[tokio::test]
async fn a_reconnecting_phone_takes_over_its_own_seat_without_disturbing_the_other() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let c = convo(&srv, false).await;
    let mut host = open(&srv, &c.host_token).await;
    let mut guest = open(&srv, &c.guest_token).await;

    // The same token again: the phone dropped and came back before the old socket timed out.
    let mut host_again = open(&srv, &c.host_token).await;

    // The stale socket is closed...
    let closed = tokio::time::timeout(Duration::from_secs(8), async {
        while let Some(frame) = host.next().await {
            if matches!(frame, Ok(Message::Close(_)) | Err(_)) {
                return true;
            }
        }
        true
    })
    .await;
    assert_eq!(closed, Ok(true), "the stale socket was never closed");

    // ...and the guest never saw the host leave: it is still the same seat.
    let left = tokio::time::timeout(Duration::from_millis(1500), async {
        wait_for(&mut guest, |v| v["type"] == "peer_left").await
    })
    .await;
    assert!(
        left.is_err() || left.unwrap().is_none(),
        "the guest was told the host left"
    );
    drop(host_again.close(None).await);
}

#[tokio::test]
async fn ending_the_conversation_tells_the_other_phone_and_closes_its_socket() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let c = convo(&srv, false).await;
    let mut host = open(&srv, &c.host_token).await;
    let _guest = open(&srv, &c.guest_token).await;

    let (s, _) = srv
        .post(
            &format!("/api/live/sessions/{}/end", c.session),
            Some(&c.guest_token),
            json!({}),
        )
        .await;
    assert_eq!(s, StatusCode::OK);

    // The host's socket learns it from the database within a few seconds, with the reason.
    let notice = wait_for(&mut host, |v| v["type"] == "session_state")
        .await
        .expect("session_state");
    assert_eq!(notice["status"], "ended");
    assert_eq!(notice["reason"], "ended_by_guest");
}

// ---- who pays --------------------------------------------------------------------------

#[tokio::test]
async fn a_phone_cannot_start_translating_when_the_owner_cannot_pay() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(speech());
    let srv = srv!(&mock);
    let c = convo(&srv, false).await;
    let mut guest = open(&srv, &c.guest_token).await;
    let _host = open(&srv, &c.host_token).await;

    // The owner spends everything between creating the session and the guest starting to talk.
    sqlx::query("UPDATE users SET balance = 0 WHERE id = $1")
        .bind(c.host_user)
        .execute(&srv.pool)
        .await
        .unwrap();
    say(&mut guest, json!({ "type": "start" })).await;
    let err = wait_for(&mut guest, |v| v["type"] == "error")
        .await
        .expect("error frame");
    assert_eq!(err["code"], "insufficient_balance");
    assert!(
        !mock.saw_type("input_audio_buffer.append"),
        "free translation"
    );
}

// ---- persistence policy ----------------------------------------------------------------

#[tokio::test]
async fn a_consumer_conversation_stores_nothing_unless_the_owner_opted_in() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(speech());
    let srv = srv!(&mock);
    let c = convo(&srv, false).await;
    let mut host = open(&srv, &c.host_token).await;
    let mut guest = open(&srv, &c.guest_token).await;
    say(&mut host, json!({ "type": "start" })).await;
    let (_, _) = both_speak(&mut host, &mut guest, 3000).await;

    let linked: Option<Uuid> =
        sqlx::query_scalar("SELECT call_session_id FROM live_sessions WHERE id = $1")
            .bind(c.session)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(linked, None, "no meeting row without an opt-in");
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM transcript_events te JOIN live_sessions ls ON ls.call_session_id = te.session_id WHERE ls.id = $1",
    )
    .bind(c.session)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(events, 0);
}

#[tokio::test]
async fn an_opted_in_conversation_is_recorded_with_both_speakers_kept_apart() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(speech());
    let srv = srv!(&mock);
    let c = convo(&srv, true).await;
    let mut host = open(&srv, &c.host_token).await;
    let mut guest = open(&srv, &c.guest_token).await;
    say(&mut host, json!({ "type": "start" })).await;
    say(&mut guest, json!({ "type": "start" })).await;
    let (_, _) = both_speak(&mut host, &mut guest, 4000).await;

    // The live row points at a meeting-side row tagged for Live Connect.
    let (call, kind): (Uuid, String) = sqlx::query_as(
        "SELECT cs.id, cs.kind FROM live_sessions ls JOIN call_sessions cs ON cs.id = ls.call_session_id WHERE ls.id = $1",
    )
    .bind(c.session)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(kind, "live_connect");

    // Events are written asynchronously; give the recorder a moment.
    let mut speakers: Vec<(String, String, String)> = Vec::new();
    for _ in 0..30 {
        speakers = sqlx::query_as(
            "SELECT speaker_peer_id, speaker_name, original_text FROM transcript_events
             WHERE session_id = $1 AND event_type = 'speech'",
        )
        .bind(call)
        .fetch_all(&srv.pool)
        .await
        .unwrap();
        let who: std::collections::HashSet<_> = speakers.iter().map(|s| s.0.clone()).collect();
        if who.contains(&c.host_peer) && who.contains(&c.guest_peer) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let by = |peer: &str| speakers.iter().find(|s| s.0 == peer).cloned();
    let h = by(&c.host_peer).expect("host speech recorded");
    let g = by(&c.guest_peer).expect("guest speech recorded");
    assert_eq!(
        (h.1.as_str(), g.1.as_str()),
        ("Marco", "Lucia"),
        "speakers stay distinguishable"
    );
    // The original is stored as spoken, not replaced by a translation.
    assert_eq!(h.2, "ciao a tutti");
}

// ---- housekeeping ----------------------------------------------------------------------

async fn status_of(srv: &Srv, session: Uuid) -> (String, Option<String>) {
    sqlx::query_as("SELECT status, end_reason FROM live_sessions WHERE id = $1")
        .bind(session)
        .fetch_one(&srv.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn an_invitation_nobody_used_is_expired_by_the_sweep() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let (jwt, _) = login(&srv).await;
    let (_, created) = srv
        .post("/api/live/sessions", Some(&jwt), json!({ "lang": "it" }))
        .await;
    let session: Uuid = created["session_id"].as_str().unwrap().parse().unwrap();
    sqlx::query("UPDATE live_sessions SET expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(session)
        .execute(&srv.pool)
        .await
        .unwrap();

    sweep_sessions(&srv.state, Some(&[session])).await.unwrap();
    assert_eq!(
        status_of(&srv, session).await,
        ("expired".into(), Some("expired".into()))
    );
}

#[tokio::test]
async fn a_conversation_both_phones_left_is_ended_only_after_the_reconnect_grace() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let c = convo(&srv, false).await;

    // Nobody is connected. The first pass only notices; it must not end a conversation
    // whose phones may be about to reconnect.
    sweep_sessions(&srv.state, Some(&[c.session]))
        .await
        .unwrap();
    assert_eq!(status_of(&srv, c.session).await.0, "active");

    tokio::time::sleep(Duration::from_millis(5300)).await;
    sweep_sessions(&srv.state, Some(&[c.session]))
        .await
        .unwrap();
    assert_eq!(
        status_of(&srv, c.session).await,
        ("ended".into(), Some("abandoned".into()))
    );
}

#[tokio::test]
async fn a_conversation_with_a_connected_phone_is_never_swept() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let c = convo(&srv, false).await;
    let mut host = open(&srv, &c.host_token).await;

    for _ in 0..2 {
        sweep_sessions(&srv.state, Some(&[c.session]))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(5300)).await;
    }
    sweep_sessions(&srv.state, Some(&[c.session]))
        .await
        .unwrap();
    assert_eq!(status_of(&srv, c.session).await.0, "active");
    drop(host.close(None).await);
}

// ---- billing over the socket -----------------------------------------------------------

#[tokio::test]
async fn the_owner_sees_the_balance_the_guest_does_not_and_every_debit_has_exactly_one_tick() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(speech());
    let srv = srv!(&mock);
    let c = convo(&srv, false).await;
    let mut host = open(&srv, &c.host_token).await;
    let mut guest = open(&srv, &c.guest_token).await;
    say(&mut host, json!({ "type": "start" })).await;
    say(&mut guest, json!({ "type": "start" })).await;

    let (host_got, guest_got) = both_speak(&mut host, &mut guest, 5500).await;
    say(&mut host, json!({ "type": "stop" })).await;
    say(&mut guest, json!({ "type": "stop" })).await;
    tokio::time::sleep(Duration::from_millis(800)).await; // let an in-flight charge commit

    // Both phones' speech is billed to the owner, and only the owner is told the balance.
    assert!(
        host_got.iter().any(|v| v["type"] == "balance_update"),
        "the owner is never told: {host_got:?}"
    );
    assert!(
        !guest_got
            .iter()
            .any(|v| matches!(v["type"].as_str(), Some("balance_update" | "low_balance"))),
        "the guest saw someone else's balance: {guest_got:?}"
    );

    // Each debit has exactly one tick row, for the right total, and both phones were metered.
    let (ticks, tick_sum, peers): (i64, Decimal, i64) = sqlx::query_as(
        "SELECT count(*), COALESCE(sum(amount), 0), count(DISTINCT peer_id)
         FROM live_usage_ticks WHERE session_id = $1 AND payer_kind = 'user'",
    )
    .bind(c.session)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    let (rows, ledger_sum): (i64, Decimal) = sqlx::query_as(
        "SELECT count(*), COALESCE(sum(-amount), 0) FROM credit_transactions WHERE user_id = $1 AND kind = 'usage'",
    )
    .bind(c.host_user)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert!(ticks >= 2, "the meters never charged: {ticks}");
    assert_eq!(peers, 2, "the guest's speech must be billed too");
    assert_eq!(rows, ticks, "a debit without a tick row, or the reverse");
    assert_eq!(ledger_sum, tick_sum);
    let left: Decimal = sqlx::query_scalar("SELECT balance FROM users WHERE id = $1")
        .bind(c.host_user)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert_eq!(
        left,
        Decimal::new(500, 2) - tick_sum,
        "the balance moved by exactly what was charged"
    );

    // The owner also watches what this conversation has cost so far, growing tick by tick, and
    // never more than what was really charged; the guest is told nothing about money.
    let spent: Vec<f64> = host_got
        .iter()
        .filter(|v| v["type"] == "session_spent")
        .filter_map(|v| v["spent"].as_f64())
        .collect();
    assert!(
        spent.len() >= 2,
        "the owner is not told what the conversation costs: {host_got:?}"
    );
    assert!(
        spent.windows(2).all(|w| w[1] >= w[0]),
        "the running total went backwards: {spent:?}"
    );
    let total = tick_sum.to_string().parse::<f64>().unwrap();
    let last = *spent.last().unwrap();
    assert!(
        last > 0.0 && last <= total + 1e-9,
        "spent {last} vs charged {total}"
    );
    assert!(
        !guest_got.iter().any(|v| v["type"] == "session_spent"),
        "the guest saw what the owner pays: {guest_got:?}"
    );
}

#[tokio::test]
async fn the_conversation_ends_for_both_phones_when_the_owner_runs_dry() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(speech());
    let srv = setup_with(&mock, Some(0.0)).await;
    let Some(srv) = srv else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let c = convo(&srv, false).await;
    let mut host = open(&srv, &c.host_token).await;
    let mut guest = open(&srv, &c.guest_token).await;

    // Starting is allowed (the gate is "can join"), but the first tick cannot be paid.
    sqlx::query("UPDATE users SET balance = 0 WHERE id = $1")
        .bind(c.host_user)
        .execute(&srv.pool)
        .await
        .unwrap();
    say(&mut host, json!({ "type": "start" })).await;
    let (host_got, guest_got) = both_speak(&mut host, &mut guest, 7000).await;

    for (who, frames) in [("host", &host_got), ("guest", &guest_got)] {
        assert!(
            frames.iter().any(|v| v["type"] == "balance_exhausted"),
            "{who} was not told: {frames:?}"
        );
        let ended = frames.iter().find(|v| v["type"] == "session_state");
        assert_eq!(
            ended.map(|v| v["reason"].clone()),
            Some(json!("balance_exhausted")),
            "{who}: {frames:?}"
        );
    }
    assert_eq!(
        status_of(&srv, c.session).await,
        ("ended".into(), Some("balance_exhausted".into()))
    );
    let charged: i64 =
        sqlx::query_scalar("SELECT count(*) FROM live_usage_ticks WHERE session_id = $1")
            .bind(c.session)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(charged, 0, "nothing is charged for what could not be paid");
}

// ---- business sessions over the socket -------------------------------------------------

/// An org with an active subscription and credit, one project, and `member` as a member.
async fn org_with_member(srv: &Srv, owner: Uuid, member: Uuid) -> (Uuid, Uuid) {
    let org: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id, subscription_status, current_period_end, credits_balance)
         VALUES ('Hotel', $1, $2, 'active', now() + interval '30 days', 1000) RETURNING id",
    )
    .bind(format!("slug-{}", Uuid::new_v4().simple()))
    .bind(owner)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    for (user, role) in [(owner, "owner"), (member, "member")] {
        sqlx::query("INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
            .bind(org)
            .bind(user)
            .bind(role)
            .execute(&srv.pool)
            .await
            .unwrap();
    }
    let project: Uuid = sqlx::query_scalar(
        "INSERT INTO projects (org_id, name, created_by) VALUES ($1, 'Reception', $2) RETURNING id",
    )
    .bind(org)
    .bind(owner)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    (org, project)
}

#[tokio::test]
async fn a_business_conversation_is_recorded_under_its_org_and_project_and_gated_on_the_org_wallet()
{
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(speech());
    let srv = srv!(&mock);
    let (_, owner) = login(&srv).await;
    let (jwt, member) = login(&srv).await;
    let (org, project) = org_with_member(&srv, owner, member).await;

    let (s, created) = srv
        .post(
            "/api/live/sessions",
            Some(&jwt),
            json!({ "lang": "it", "org_id": org, "project_id": project }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{created}");
    let code = created["invite_url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_string();
    let (_, joined) = srv
        .post(
            "/api/live/join",
            None,
            json!({ "code": code, "name": "Customer", "lang": "es", "consent": true }),
        )
        .await;
    let session: Uuid = created["session_id"].as_str().unwrap().parse().unwrap();

    let mut host = open(&srv, created["token"].as_str().unwrap()).await;
    let _guest = open(&srv, joined["token"].as_str().unwrap()).await;

    // The meeting-side row exists straight away (the org ledger references it) and carries the
    // org and project, so the conversation lands in that project's history and search.
    let (kind, row_org, row_project): (String, Option<Uuid>, Option<Uuid>) = sqlx::query_as(
        "SELECT cs.kind, cs.org_id, cs.project_id FROM live_sessions ls
         JOIN call_sessions cs ON cs.id = ls.call_session_id WHERE ls.id = $1",
    )
    .bind(session)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(
        (kind.as_str(), row_org, row_project),
        ("live_connect", Some(org), Some(project))
    );

    // The org wallet, not the person, is what gates speech: below the minimum, no translation.
    sqlx::query("UPDATE organizations SET credits_balance = 5 WHERE id = $1")
        .bind(org)
        .execute(&srv.pool)
        .await
        .unwrap();
    say(&mut host, json!({ "type": "start" })).await;
    let err = wait_for(&mut host, |v| v["type"] == "error")
        .await
        .expect("error frame");
    assert_eq!(err["code"], "insufficient_credits");
    assert!(
        !mock.saw_type("input_audio_buffer.append"),
        "free translation"
    );
}

// ---- defects found by independent verification -----------------------------------------

#[tokio::test]
async fn starting_and_stopping_repeatedly_is_not_free() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(speech());
    let srv = srv!(&mock);
    let c = convo(&srv, false).await;
    let mut host = open(&srv, &c.host_token).await;
    let mut guest = open(&srv, &c.guest_token).await;

    // Four bursts of speech, each shorter than one billing interval (1 s here, 5 s in
    // production), each starting more than an interval after the previous one. Charging only in
    // arrears let every one of them through unpaid. (Starts closer together than an interval
    // share one opening charge, so a Start/Stop loop cannot drain the owner: see
    // `tests/live_enhanced.rs` and `tests/live_billing.rs`.)
    for _ in 0..4 {
        say(&mut host, json!({ "type": "start" })).await;
        let _ = both_speak(&mut host, &mut guest, 600).await;
        say(&mut host, json!({ "type": "stop" })).await;
        tokio::time::sleep(Duration::from_millis(700)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    let host_peer: Uuid = c.host_peer.parse().unwrap();
    let paid: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM live_usage_ticks WHERE session_id = $1 AND peer_id = $2",
    )
    .bind(c.session)
    .bind(host_peer)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert!(paid >= 4, "four starts must be four charges, got {paid}");
}

#[tokio::test]
async fn a_muted_phone_stays_muted_across_a_reconnect() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(speech());
    let srv = srv!(&mock);
    let c = convo(&srv, false).await;
    let mut host = open(&srv, &c.host_token).await;
    let mut guest = open(&srv, &c.guest_token).await;

    say(&mut host, json!({ "type": "mute_audio", "muted": true })).await;
    wait_for(&mut guest, |v| v["type"] == "peer_muted")
        .await
        .expect("peer_muted");

    // The phone drops and comes back on the same token, and does NOT send mute_audio again.
    let _ = host.close(None).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut host = connect(&srv, &c.host_token).await.expect("reconnect");
    let told = wait_for(&mut host, |v| v["type"] == "peer_muted")
        .await
        .expect("told it is still muted");
    assert_eq!(told["muted"], true);

    say(&mut host, json!({ "type": "start" })).await;
    let _ = both_speak(&mut host, &mut guest, 2500).await;
    assert!(
        !mock.saw_type("input_audio_buffer.append"),
        "audio reached the engine after a reconnect while muted"
    );
}

#[tokio::test]
async fn the_other_phone_never_learns_an_account_id() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);

    // Both orders of arrival: the guest learns about the host either from the peer list it is
    // given on joining (host first) or from the announcement of the host's arrival (guest
    // first). Either way, the owner's account id must not be in it.
    for host_first in [true, false] {
        let c = convo(&srv, false).await;
        let host_user = c.host_user.to_string();
        let (mut host, mut guest, frames);
        if host_first {
            host = connect(&srv, &c.host_token).await.expect("host");
            wait_for(&mut host, |v| v["type"] == "room_joined")
                .await
                .expect("host room_joined");
            guest = connect(&srv, &c.guest_token).await.expect("guest");
            let joined = wait_for(&mut guest, |v| v["type"] == "room_joined")
                .await
                .expect("room_joined");
            frames = vec![("guest room_joined", joined)];
        } else {
            guest = connect(&srv, &c.guest_token).await.expect("guest");
            wait_for(&mut guest, |v| v["type"] == "room_joined")
                .await
                .expect("room_joined");
            host = connect(&srv, &c.host_token).await.expect("host");
            let host_joined = wait_for(&mut host, |v| v["type"] == "room_joined")
                .await
                .expect("host room_joined");
            let announced = wait_for(&mut guest, |v| v["type"] == "peer_joined")
                .await
                .expect("peer_joined");
            frames = vec![
                ("host room_joined", host_joined),
                ("peer_joined", announced),
            ];
        }
        for (what, frame) in &frames {
            let shown = frame.to_string();
            assert!(
                !shown.contains(&host_user),
                "host_first={host_first}: {what} leaked the owner's account id: {shown}"
            );
            assert!(
                shown.contains("peer"),
                "the frame must really describe the other phone: {shown}"
            );
        }
        drop((host, guest));
    }
}

#[tokio::test]
async fn a_business_session_is_never_stored_under_a_binding_the_server_did_not_make() {
    // A room that carries a client-made binding must not decide a Live Connect conversation's
    // org and project; the server's own row is the only one that does.
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(speech());
    let srv = srv!(&mock);
    let (_, owner) = login(&srv).await;
    let (jwt, member) = login(&srv).await;
    let (org, project) = org_with_member(&srv, owner, member).await;
    let (_, created) = srv
        .post(
            "/api/live/sessions",
            Some(&jwt),
            json!({ "lang": "it", "org_id": org, "project_id": project }),
        )
        .await;
    let session: Uuid = created["session_id"].as_str().unwrap().parse().unwrap();

    // Pre-binding the future room is refused outright (see also `live_rest`).
    let res = srv
        .http
        .patch(format!(
            "http://{}/api/rooms/live-{session}/business",
            srv.addr
        ))
        .bearer_auth(&jwt)
        .json(&json!({ "org_id": org }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let bound: i64 =
        sqlx::query_scalar("SELECT count(*) FROM room_business_bindings WHERE room = $1")
            .bind(format!("live-{session}"))
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(bound, 0);
}

#[tokio::test]
async fn muting_around_every_tick_does_not_dodge_the_meter() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(speech());
    let srv = srv!(&mock);
    let c = convo(&srv, false).await;
    let mut host = open(&srv, &c.host_token).await;
    let _guest = open(&srv, &c.guest_token).await;

    // A client that knows its own Start time can predict every meter tick (1 s here). It mutes for
    // a window around each one and streams audio the rest of the time. Billing that looked at the
    // mute state at the tick instant charged it nothing while the engine heard most of the speech.
    say(&mut host, json!({ "type": "start" })).await;
    let t0 = tokio::time::Instant::now();
    let mut muted = false;
    while t0.elapsed() < Duration::from_millis(3400) {
        let phase = t0.elapsed().as_millis() % 1000;
        let want = !(150..850).contains(&phase);
        if want != muted {
            say(&mut host, json!({ "type": "mute_audio", "muted": want })).await;
            muted = want;
        }
        if !muted {
            let _ = host.send(Message::binary(vec![0u8; 640])).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(400)).await;

    let host_peer: Uuid = c.host_peer.parse().unwrap();
    let charged: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM live_usage_ticks WHERE session_id = $1 AND peer_id = $2",
    )
    .bind(c.session)
    .bind(host_peer)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert!(
        mock.saw_type("input_audio_buffer.append"),
        "the probe never reached the engine"
    );
    assert!(
        charged >= 3,
        "audio reached the engine in every interval but only {charged} were charged"
    );
}

#[tokio::test]
async fn the_public_socket_cannot_join_a_live_conversations_room() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(speech());
    let srv = srv!(&mock);
    let c = convo(&srv, false).await;
    let _host = open(&srv, &c.host_token).await;

    // Anyone who learned the session id (a colleague sees waiting ids; search hits carry it) must not
    // be able to sit in the room and receive its subtitles and translated audio.
    for room in [format!("live-{}", c.session), format!("LIVE-{}", c.session)] {
        let url = format!("ws://{}/ws?room={room}&lang=en&name=Intruder", srv.addr);
        match tokio_tungstenite::connect_async(url).await {
            Err(WsError::Http(res)) => assert_eq!(res.status().as_u16(), 403, "{room}"),
            other => panic!(
                "the room {room} was joinable through /ws: {:?}",
                other.map(|_| ())
            ),
        }
    }
    // A user-chosen name that merely starts with `live-` is untouched.
    let (_, _) = tokio_tungstenite::connect_async(format!(
        "ws://{}/ws?room=live-room&lang=en&name=Ok",
        srv.addr
    ))
    .await
    .expect("an ordinary room named live-room must keep working");
}

// ---- latency instrumentation (S18) -----------------------------------------------------

/// `(count, sum_ms)` of the Live Connect speech-to-translated-audio histogram on `/metrics`.
async fn live_latency(srv: &Srv) -> (u64, u64) {
    let body = srv
        .http
        .get(format!("http://{}/metrics", srv.addr))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let read = |suffix: &str| -> u64 {
        let key = format!("voxtranslate_live_translation_ms_{suffix} ");
        body.lines()
            .find_map(|l| l.strip_prefix(&key))
            .unwrap_or_else(|| panic!("metric {key}missing from /metrics"))
            .trim()
            .parse()
            .unwrap()
    };
    (read("count"), read("sum"))
}

#[tokio::test]
async fn the_server_times_speech_to_translated_audio_once_per_utterance() {
    let _alone = VOICED_SPEECH.lock().await;
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(speech());
    let srv = srv!(&mock);
    let c = convo(&srv, false).await;
    let (count_before, sum_before) = live_latency(&srv).await;

    let mut host = open(&srv, &c.host_token).await;
    let mut guest = open(&srv, &c.guest_token).await;
    say(&mut host, json!({ "type": "start" })).await;

    // The host talks without a pause (a voiced 100 ms frame every tick) while the guest
    // listens. The mock answers once, on the first audio of the upstream session: one
    // utterance, one chunk of translated audio.
    let voice: Vec<u8> = std::iter::repeat_n(1000i16.to_le_bytes(), 2400)
        .flatten()
        .collect();
    let mut heard = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            _ = tick.tick() => { let _ = host.send(Message::binary(voice.clone())).await; }
            Some(Ok(Message::Text(t))) = guest.next() => {
                if let Ok(v) = serde_json::from_str::<Value>(t.as_str()) {
                    if v["type"] == "translated_audio" && v["speaker_id"] == c.host_peer { heard += 1; }
                }
            }
            Some(Ok(_)) = host.next() => {}
            _ = tokio::time::sleep_until(deadline) => break,
        }
    }
    assert!(heard >= 1, "the guest never heard the host");

    let (count_after, sum_after) = live_latency(&srv).await;
    assert_eq!(
        count_after - count_before,
        1,
        "one utterance is one observation, however many audio chunks answer it"
    );
    let ms = sum_after - sum_before;
    assert!(
        ms > 0 && ms < 5_000,
        "a plausible latency for a local mock: {ms} ms"
    );
}

// ---- the engine is fed at most what a real microphone produces (R1) -------------------

/// How many chunks of audio the mock engine has received so far.
fn appends(mock: &RealtimeMock) -> usize {
    mock.received()
        .iter()
        .filter(|raw| {
            serde_json::from_str::<Value>(raw)
                .ok()
                .is_some_and(|v| v["type"] == "input_audio_buffer.append")
        })
        .count()
}

/// Rows of `kind` the session has recorded.
async fn event_count(srv: &Srv, session: Uuid, kind: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM live_session_events WHERE session_id = $1 AND kind = $2",
    )
    .bind(session)
    .bind(kind)
    .fetch_one(&srv.pool)
    .await
    .unwrap()
}

fn voiced(bytes: usize) -> Vec<u8> {
    std::iter::repeat_n(1000i16.to_le_bytes(), bytes / 2)
        .flatten()
        .collect()
}

/// A frame too big to be 100 ms of microphone is not an audio frame: however the sender packs
/// it (here 1.3 s in one frame, under the old 64 KB transport cap), it never reaches the engine,
/// and the sender is told once.
#[tokio::test]
async fn an_oversized_audio_frame_never_reaches_the_engine() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(speech());
    let srv = srv!(&mock);
    let c = convo(&srv, false).await;
    let mut host = open(&srv, &c.host_token).await;
    // The engine opens an upstream session per listener language, so somebody must be listening.
    let _guest = open(&srv, &c.guest_token).await;
    say(&mut host, json!({ "type": "start" })).await;
    // Let the room reconcile its upstream sessions; frames before that have nowhere to go. A few
    // honest frames at the end of the wait are the control: they MUST show up as appends.
    let frame = voiced(4_800);
    for _ in 0..15 {
        host.send(Message::binary(frame.clone())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(appends(&mock) > 0, "the harness sees no audio at all");
    let warmup = appends(&mock);

    for _ in 0..5 {
        host.send(Message::binary(voiced(60_000))).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The next frame is what carries the report out.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    host.send(Message::binary(voiced(60_000))).await.unwrap();
    let err = wait_for(&mut host, |v| {
        v["type"] == "error" && v["code"] == "frame_too_large"
    })
    .await;
    assert!(err.is_some(), "the sender was never told");
    assert_eq!(appends(&mock), warmup, "an oversized frame was forwarded");
    assert!(event_count(&srv, c.session, "diagnostics").await >= 1);
}

/// Audio pushed at 70x real time through the allowed message rate used to be translated in
/// full while the meter billed the wall clock. Now it is dropped down to the budget.
#[tokio::test]
async fn audio_sent_far_faster_than_it_plays_is_dropped() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(speech());
    let srv = srv!(&mock);
    let c = convo(&srv, false).await;
    let mut host = open(&srv, &c.host_token).await;
    // The engine opens an upstream session per listener language, so somebody must be listening.
    let _guest = open(&srv, &c.guest_token).await;
    say(&mut host, json!({ "type": "start" })).await;
    // Let the room reconcile its upstream sessions; frames before that have nowhere to go. A few
    // honest frames at the end of the wait are the control: they MUST show up as appends.
    let frame = voiced(4_800);
    for _ in 0..15 {
        host.send(Message::binary(frame.clone())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(appends(&mock) > 0, "the harness sees no audio at all");

    // 400 frames of 100 ms = 40 s of audio, as fast as the socket takes them.
    let before = appends(&mock);
    for _ in 0..400 {
        host.send(Message::binary(frame.clone())).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let got = appends(&mock) - before;
    // One second of burst (15 frames) plus the little that refilled meanwhile; the rest of the
    // 400 is gone. The old server forwarded all of it.
    assert!(got <= 40, "{got} of 400 frames reached the engine");
    assert!(got >= 10, "the budget starved the sender: {got}");
    let said = event_count(&srv, c.session, "diagnostics").await;
    assert!(
        (1..=3).contains(&said),
        "one report a second at most, got {said}"
    );
}

/// The other side of the same rule: a real client is untouched, including one whose network
/// holds frames back and then hands them over together.
#[tokio::test]
async fn a_client_sending_at_the_speed_of_a_microphone_loses_nothing() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(speech());
    let srv = srv!(&mock);
    let c = convo(&srv, false).await;
    let mut host = open(&srv, &c.host_token).await;
    // The engine opens an upstream session per listener language, so somebody must be listening.
    let _guest = open(&srv, &c.guest_token).await;
    say(&mut host, json!({ "type": "start" })).await;
    // Let the room reconcile its upstream sessions; frames before that have nowhere to go. A few
    // honest frames at the end of the wait are the control: they MUST show up as appends.
    let frame = voiced(4_800);
    for _ in 0..15 {
        host.send(Message::binary(frame.clone())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(appends(&mock) > 0, "the harness sees no audio at all");
    let before = appends(&mock);

    let mut sent = 0;
    for _ in 0..20 {
        host.send(Message::binary(frame.clone())).await.unwrap();
        sent += 1;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // A stall of 600 ms, then the six frames it held, back to back.
    tokio::time::sleep(Duration::from_millis(600)).await;
    for _ in 0..6 {
        host.send(Message::binary(frame.clone())).await.unwrap();
        sent += 1;
    }
    for _ in 0..10 {
        host.send(Message::binary(frame.clone())).await.unwrap();
        sent += 1;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(appends(&mock) - before, sent, "an honest sender lost audio");
    assert_eq!(event_count(&srv, c.session, "diagnostics").await, 0);
}

// ---- the other phone's microphone hears this phone's speaker ---------------------------

/// A constant-amplitude PCM16 frame of `bytes` bytes: `amp` is also its mean absolute sample.
fn tone(amp: i16, bytes: usize) -> Vec<u8> {
    std::iter::repeat_n(amp.to_le_bytes(), bytes / 2)
        .flatten()
        .collect()
}

/// Both phones stream their microphone for `ms`, everything each one receives is returned. The
/// host's microphone is loud and the guest's quiet: the guest's phone is only hearing the host
/// across the table, which is what the echo filter uses to tell the speaker from the listener.
async fn mics_open(
    host: &mut Ws,
    guest: &mut Ws,
    ms: u64,
    guest_mic: bool,
) -> (Vec<Value>, Vec<Value>) {
    let (mut got_host, mut got_guest) = (Vec::new(), Vec::new());
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let _ = host.send(Message::binary(tone(1000, 4800))).await;
                if guest_mic {
                    let _ = guest.send(Message::binary(tone(200, 4800))).await;
                }
            }
            Some(Ok(Message::Text(t))) = host.next() => {
                if let Ok(v) = serde_json::from_str(t.as_str()) { got_host.push(v); }
            }
            Some(Ok(Message::Text(t))) = guest.next() => {
                if let Ok(v) = serde_json::from_str(t.as_str()) { got_guest.push(v); }
            }
            _ = tokio::time::sleep_until(deadline) => return (got_host, got_guest),
        }
    }
}

/// Frames about `speaker`'s speech: captions and translated audio.
fn speech_of<'a>(frames: &'a [Value], speaker: &str) -> Vec<&'a Value> {
    frames
        .iter()
        .filter(|v| {
            v["speaker_id"] == speaker
                && matches!(
                    v["type"].as_str(),
                    Some("subtitle_interim" | "subtitle_final" | "translated_audio")
                )
        })
        .collect()
}

async fn echo_metric(srv: &Srv, name: &str) -> u64 {
    let body = srv
        .http
        .get(format!("http://{}/metrics", srv.addr))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let key = format!("voxtranslate_live_echo_{name}_total ");
    body.lines()
        .find_map(|l| l.strip_prefix(&key))
        .unwrap_or_else(|| panic!("metric {key}missing from /metrics"))
        .trim()
        .parse()
        .unwrap()
}

#[tokio::test]
async fn the_translation_of_my_own_voice_heard_by_the_other_phone_is_not_played_back_to_me() {
    let _alone = VOICED_SPEECH.lock().await;
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    // Every upstream session hears the same sentence, like two phones side by side: the host's
    // session (it -> es) and the guest's session (es -> it), which "translates" the host's voice.
    mock.reply_with(vec![
        Reply::Transcript {
            original: "ciao come stai oggi".into(),
            translated: "hola como estas hoy".into(),
        },
        Reply::Audio(vec![7u8; 960]),
    ]);
    let srv = srv!(&mock);
    let c = convo(&srv, false).await;
    let suppressed_before = echo_metric(&srv, "suppressed").await;

    let mut host = open(&srv, &c.host_token).await;
    let mut guest = open(&srv, &c.guest_token).await;
    // The host speaks first, and their own caption is on screen before anything else happens.
    // (Which of two sessions gets its first caption out first depends on when the room opened
    // them, up to a second apart with the mock; the filter's hold is 700 ms, so a test that
    // started both at once would measure that race and not the rule.)
    say(&mut host, json!({ "type": "start" })).await;
    let (mut host_got, mut guest_got) = mics_open(&mut host, &mut guest, 2_500, false).await;
    // Now the guest's microphone opens and hears the host across the table.
    say(&mut guest, json!({ "type": "start" })).await;
    let (h, g) = mics_open(&mut host, &mut guest, 4_500, true).await;
    host_got.extend(h);
    guest_got.extend(g);

    // The guest still hears the host's real translation, captions and audio...
    assert!(
        !audio_from(&guest_got, &c.host_peer).is_empty(),
        "the guest never heard the host: {guest_got:?}"
    );
    assert!(
        guest_got
            .iter()
            .any(|v| v["type"] == "subtitle_interim" && v["speaker_id"] == c.host_peer),
        "the guest got no caption of the host"
    );
    // ...the host still sees their own words...
    assert!(
        host_got
            .iter()
            .any(|v| v["type"] == "subtitle_interim" && v["speaker_id"] == c.host_peer),
        "the host lost their own caption: {host_got:?}"
    );
    // ...and none of the guest's session (the host's voice, read back) reaches the host.
    let echoed = speech_of(&host_got, &c.guest_peer);
    assert!(
        echoed.is_empty(),
        "the host was played their own sentence back: {echoed:?}"
    );
    assert!(
        echo_metric(&srv, "suppressed").await > suppressed_before,
        "the suppression was counted"
    );

    // A genuine reply from the guest is a different sentence and still reaches the host,
    // through the same socket and the same outbound path.
    let room = voxtranslate_server::live::ws::room_name(c.session);
    for frame in [
        ServerMessage::SubtitleInterim {
            speaker_id: c.guest_peer.clone(),
            speaker_name: "Lucia".into(),
            text: "sto bene grazie".into(),
            lang: "es".into(),
            original: Some("estoy bien gracias".into()),
        },
        ServerMessage::TranslatedAudio {
            speaker_id: c.guest_peer.clone(),
            lang: "it".into(),
            seq: 99,
            pcm16_b64: "AAAA".into(),
        },
    ] {
        assert!(srv
            .state
            .rooms
            .relay_to_peer(&room, &c.host_peer, &frame.to_json()));
    }
    let (host_reply, _) = mics_open(&mut host, &mut guest, 2_500, true).await;
    assert!(
        host_reply
            .iter()
            .any(|v| v["type"] == "subtitle_interim" && v["text"] == "sto bene grazie"),
        "the real reply never reached the host: {host_reply:?}"
    );
    assert!(
        host_reply
            .iter()
            .any(|v| v["type"] == "translated_audio" && v["seq"] == 99),
        "the real reply's audio never reached the host"
    );

    // The suppression is reported once, as counts only.
    let mut reported = false;
    for _ in 0..20 {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM live_session_events
             WHERE session_id = $1 AND kind = 'diagnostics' AND detail ? 'echo_filter'",
        )
        .bind(c.session)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
        if n > 0 {
            reported = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(reported, "no diagnostics event for the echo filter");
}

#[tokio::test]
async fn with_the_echo_filter_off_the_same_scene_plays_the_echo_back() {
    let _alone = VOICED_SPEECH.lock().await;
    // The kill switch: LIVE_ECHO_FILTER=0 restores today's behaviour exactly.
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(vec![
        Reply::Transcript {
            original: "ciao come stai oggi".into(),
            translated: "hola como estas hoy".into(),
        },
        Reply::Audio(vec![7u8; 960]),
    ]);
    let mut srv = srv!(&mock);
    srv.state.live = Arc::new(LiveState::new(
        LiveConfig::load(|k| match k {
            "LIVE_CONNECT_ENABLED" => Some("1".into()),
            "LIVE_TOKEN_KEY" => Some(LIVE_KEY.into()),
            "LIVE_ECHO_FILTER" => Some("0".into()),
            _ => None,
        })
        .unwrap(),
    ));
    // The server task holds its own clone of the state; serve the changed one.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    srv.addr = listener.local_addr().unwrap();
    let served = srv.state.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(served)).await;
    });
    let c = convo(&srv, false).await;
    let mut host = open(&srv, &c.host_token).await;
    let mut guest = open(&srv, &c.guest_token).await;
    say(&mut host, json!({ "type": "start" })).await;
    say(&mut guest, json!({ "type": "start" })).await;
    let (host_got, _) = mics_open(&mut host, &mut guest, 5_000, true).await;
    assert!(
        !speech_of(&host_got, &c.guest_peer).is_empty(),
        "with the filter off the host hears the guest's session"
    );
}

// ---- one host, several guests, languages free (spec 0121, S1/S2/S3) ---------------------

use futures::stream::SplitSink;
use tokio::sync::mpsc;

/// One phone of a many-seat conversation: its socket split in two so that a test can feed every
/// microphone and read every screen in the same loop. Everything it receives is kept in `seen`.
struct Phone {
    peer: String,
    lang: &'static str,
    tx: SplitSink<Ws, Message>,
    rx: mpsc::UnboundedReceiver<Value>,
    seen: Vec<Value>,
}

impl Phone {
    async fn connect(srv: &Srv, token: &str, peer: &str, lang: &'static str) -> Phone {
        let ws = connect(srv, token).await.expect("upgrade");
        let (tx, mut stream) = ws.split();
        let (out, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(Ok(frame)) = stream.next().await {
                match frame {
                    Message::Text(t) => {
                        if let Ok(v) = serde_json::from_str::<Value>(t.as_str()) {
                            if out.send(v).is_err() {
                                return;
                            }
                        }
                    }
                    Message::Close(_) => return,
                    _ => {}
                }
            }
        });
        Phone {
            peer: peer.into(),
            lang,
            tx,
            rx,
            seen: Vec::new(),
        }
    }

    /// Move whatever has arrived into `seen`.
    fn pull(&mut self) {
        while let Ok(v) = self.rx.try_recv() {
            self.seen.push(v);
        }
    }

    /// Wait (up to 8 s) for a frame matching `wanted`, whether it was already seen or not.
    async fn wait_for(&mut self, wanted: impl Fn(&Value) -> bool) -> Option<Value> {
        self.pull();
        if let Some(v) = self.seen.iter().find(|v| wanted(v)) {
            return Some(v.clone());
        }
        while let Ok(Some(v)) = tokio::time::timeout(Duration::from_secs(8), self.rx.recv()).await {
            self.seen.push(v.clone());
            if wanted(&v) {
                return Some(v);
            }
        }
        None
    }

    async fn say(&mut self, v: Value) {
        let _ = self.tx.send(Message::text(v.to_string())).await;
    }
}

/// A conversation of `1 + guest_langs.len()` phones: a host (`it`) and one guest per language.
struct Many {
    session: Uuid,
    host_user: Uuid,
    host_token: String,
    host_peer: String,
    /// `(token, peer, lang)` per guest, in join order.
    guests: Vec<(String, String, &'static str)>,
}

async fn many(srv: &Srv, guest_langs: &[&'static str], save_transcript: bool) -> Many {
    many_on(srv, guest_langs, save_transcript, None).await
}

/// [`many`] on the tier `engine` (`None` = nothing asked, so Standard).
async fn many_on(
    srv: &Srv,
    guest_langs: &[&'static str],
    save_transcript: bool,
    engine: Option<&str>,
) -> Many {
    let (jwt, host_user) = login(srv).await;
    let (s, created) = srv
        .post(
            "/api/live/sessions",
            Some(&jwt),
            json!({
                "lang": "it",
                "save_transcript": save_transcript,
                "max_guests": guest_langs.len(),
                "engine": engine,
            }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{created}");
    let code = created["invite_url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_string();
    let mut guests = Vec::new();
    for (i, lang) in guest_langs.iter().enumerate() {
        let (s, joined) = srv
            .post(
                "/api/live/join",
                None,
                json!({ "code": code, "name": format!("Guest{i}"), "lang": lang, "consent": true }),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{joined}");
        guests.push((
            joined["token"].as_str().unwrap().into(),
            joined["peer_id"].as_str().unwrap().into(),
            *lang,
        ));
    }
    Many {
        session: created["session_id"].as_str().unwrap().parse().unwrap(),
        host_user,
        host_token: created["token"].as_str().unwrap().into(),
        host_peer: created["peer_id"].as_str().unwrap().into(),
        guests,
    }
}

/// Every phone of `m`, connected in order (host first) and each told `room_joined`.
async fn open_all(srv: &Srv, m: &Many) -> Vec<Phone> {
    let mut phones = vec![Phone::connect(srv, &m.host_token, &m.host_peer, "it").await];
    for (token, peer, lang) in &m.guests {
        phones.push(Phone::connect(srv, token, peer, lang).await);
    }
    for p in &mut phones {
        p.wait_for(|v| v["type"] == "room_joined")
            .await
            .expect("room_joined");
    }
    phones
}

/// Every phone feeds its microphone for `ms` (`mic(i)` is phone `i`'s 100 ms frame, `None` = it
/// stays quiet) while every screen is read.
async fn all_speak(phones: &mut [Phone], ms: u64, mic: impl Fn(usize) -> Option<Vec<u8>>) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    while tokio::time::Instant::now() < deadline {
        for (i, p) in phones.iter_mut().enumerate() {
            if let Some(frame) = mic(i) {
                let _ = p.tx.send(Message::binary(frame)).await;
            }
            p.pull();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for p in phones.iter_mut() {
        p.pull();
    }
}

#[tokio::test]
async fn four_phones_in_three_languages_each_hear_every_other_speaker_in_their_own() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(speech());
    let srv = srv!(&mock);
    let m = many(&srv, &["es", "es", "en"], false).await;
    let mut phones = open_all(&srv, &m).await;
    for p in &mut phones {
        p.say(json!({ "type": "start" })).await;
    }

    // Nobody takes turns: all four talk at once.
    all_speak(&mut phones, 6_000, |_| Some(vec![0u8; 640])).await;

    for (l, listener) in phones.iter().enumerate() {
        for (s, speaker) in phones.iter().enumerate() {
            let heard = audio_from(&listener.seen, &speaker.peer);
            if l == s {
                assert!(heard.is_empty(), "phone {l} was played its own voice");
                continue;
            }
            if listener.lang == speaker.lang {
                // Two people who share a language just hear each other: nothing is translated,
                // so nothing is played, and nothing is billed for it.
                assert!(
                    heard.is_empty(),
                    "phone {l} and phone {s} share {} and need no translation",
                    listener.lang
                );
                continue;
            }
            assert!(
                !heard.is_empty(),
                "phone {l} ({}) never heard phone {s} ({}): {:?}",
                listener.lang,
                speaker.lang,
                listener
                    .seen
                    .iter()
                    .map(|v| v["type"].clone())
                    .collect::<Vec<_>>()
            );
            assert!(
                heard.iter().all(|v| v["lang"] == listener.lang),
                "phone {l} must hear phone {s} in {}, got {:?}",
                listener.lang,
                heard.iter().map(|v| v["lang"].clone()).collect::<Vec<_>>()
            );
        }
        // And sees who is speaking: a caption per other speaker.
        for (s, speaker) in phones.iter().enumerate() {
            if l != s && listener.lang != speaker.lang {
                assert!(
                    listener.seen.iter().any(|v| v["type"]
                        .as_str()
                        .is_some_and(|t| t.starts_with("subtitle"))
                        && v["speaker_id"] == speaker.peer),
                    "phone {l} got no caption of phone {s}"
                );
            }
        }
    }
}

#[tokio::test]
async fn the_room_tells_every_phone_who_is_there_who_arrives_who_leaves_and_who_mutes() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let m = many(&srv, &["es", "es", "en"], false).await;
    let mut phones = open_all(&srv, &m).await;

    // Each phone learns of the ones already there from `room_joined`, and of later ones from
    // `peer_joined`: the last to arrive sees three, the first sees three arrivals.
    let joined = phones[3]
        .seen
        .iter()
        .find(|v| v["type"] == "room_joined")
        .unwrap()["peers"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(joined, 3);
    phones[0].pull();
    let arrivals: Vec<&str> = phones[0]
        .seen
        .iter()
        .filter(|v| v["type"] == "peer_joined")
        .map(|v| v["peer_id"].as_str().unwrap())
        .collect();
    assert_eq!(arrivals.len(), 3, "{arrivals:?}");

    // A mute is told to everybody else, not just one partner.
    phones[1]
        .say(json!({ "type": "mute_audio", "muted": true }))
        .await;
    let want = phones[1].peer.clone();
    for i in [0usize, 2, 3] {
        let v = phones[i]
            .wait_for(|v| v["type"] == "peer_muted" && v["peer_id"] == want.as_str())
            .await
            .unwrap_or_else(|| panic!("phone {i} was never told the guest muted"));
        assert_eq!(v["muted"], true);
    }

    // One guest leaves: the three others each hear about it, and the conversation goes on.
    let mut leaving = phones.pop().unwrap();
    let gone = leaving.peer.clone();
    let _ = leaving.tx.close().await;
    for (i, p) in phones.iter_mut().enumerate() {
        p.wait_for(|v| v["type"] == "peer_left" && v["peer_id"] == gone.as_str())
            .await
            .unwrap_or_else(|| panic!("phone {i} never learnt the guest left"));
    }
    let (status, _) = status_of(&srv, m.session).await;
    assert_eq!(status, "active", "one guest leaving is not the end");
}

#[tokio::test]
async fn a_guest_who_reconnects_keeps_their_seat_and_the_others_never_see_them_leave() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let m = many(&srv, &["es", "en"], false).await;
    let mut phones = open_all(&srv, &m).await;

    // The second guest's phone drops and comes back on the same token before the stale socket timed out.
    let (token, peer, lang) = m.guests[1].clone();
    let mut again = Phone::connect(&srv, &token, &peer, lang).await;
    again
        .wait_for(|v| v["type"] == "room_joined")
        .await
        .expect("reconnected");
    assert_eq!(
        again
            .seen
            .iter()
            .find(|v| v["type"] == "room_joined")
            .unwrap()["peers"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "back in the same room with the two others"
    );

    // A replaced socket is a replacement, not a departure.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    for (i, p) in phones.iter_mut().enumerate().take(2) {
        p.pull();
        assert!(
            !p.seen.iter().any(|v| v["type"] == "peer_left"),
            "phone {i} saw the reconnecting guest leave: {:?}",
            p.seen
        );
    }
    // The stale socket is told to go (its reader ends once the server closes it).
    let stale = &mut phones[2];
    let closed = tokio::time::timeout(Duration::from_secs(8), async {
        while stale.rx.recv().await.is_some() {}
    })
    .await;
    assert!(closed.is_ok(), "the stale socket was never closed");
}

#[tokio::test]
async fn every_phone_has_its_own_ticks_and_the_owner_pays_one_stream_per_distinct_language() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(speech());
    let srv = srv!(&mock);
    let rate = srv
        .state
        .engines
        .default()
        .metadata()
        .user_rate_per_second();

    // host it, guests es, es, en: every speaker has two OTHER languages in the room.
    // host it, guests es, es   : every speaker has ONE other language (two Spanish guests are
    //                            not two streams for the host).
    for (guest_langs, expected_streams) in [
        (vec!["es", "es", "en"], vec![2.0, 2.0, 2.0, 2.0]),
        (vec!["es", "es"], vec![1.0, 1.0, 1.0]),
    ] {
        let m = many(&srv, &guest_langs, false).await;
        let mut phones = open_all(&srv, &m).await;
        for p in &mut phones {
            p.say(json!({ "type": "start" })).await;
        }
        all_speak(&mut phones, 3_500, |_| Some(vec![0u8; 640])).await;
        for p in &mut phones {
            p.say(json!({ "type": "stop" })).await;
        }
        tokio::time::sleep(Duration::from_millis(800)).await;

        let mut seats = vec![m.host_peer.clone()];
        seats.extend(m.guests.iter().map(|g| g.1.clone()));
        for (peer, streams) in seats.iter().zip(&expected_streams) {
            let (n, lo, hi): (i64, Option<Decimal>, Option<Decimal>) = sqlx::query_as(
                "SELECT count(*), min(amount), max(amount) FROM live_usage_ticks
                 WHERE session_id = $1 AND peer_id = $2::uuid",
            )
            .bind(m.session)
            .bind(peer)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
            assert!(n >= 2, "{guest_langs:?}: {peer} was metered {n} times");
            let unit = usd(rate * 1.0 * streams);
            assert_eq!(
                (lo, hi),
                (Some(unit), Some(unit)),
                "{guest_langs:?}: {peer} must pay for {streams} stream(s) a second"
            );
        }

        // Exactly the ticks were debited from the owner, to the last digit.
        let (ticks, tick_sum): (i64, Decimal) = sqlx::query_as(
            "SELECT count(*), COALESCE(sum(amount), 0) FROM live_usage_ticks
             WHERE session_id = $1 AND payer_kind = 'user'",
        )
        .bind(m.session)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
        let (rows, spent): (i64, Decimal) = sqlx::query_as(
            "SELECT count(*), COALESCE(sum(-amount), 0) FROM credit_transactions
             WHERE user_id = $1 AND kind = 'usage'",
        )
        .bind(m.host_user)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
        assert_eq!((rows, spent), (ticks, tick_sum), "{guest_langs:?}");
        let left: Decimal = sqlx::query_scalar("SELECT balance FROM users WHERE id = $1")
            .bind(m.host_user)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
        assert_eq!(left, Decimal::new(500, 2) - tick_sum, "{guest_langs:?}");
        // Guests are never debited and never see the owner's money.
        for p in phones.iter().skip(1) {
            assert!(
                !p.seen.iter().any(|v| matches!(
                    v["type"].as_str(),
                    Some("balance_update" | "low_balance" | "session_spent")
                )),
                "a guest was told what the owner pays"
            );
        }
    }
}

#[tokio::test]
async fn one_utterance_heard_by_three_phones_is_one_latency_observation() {
    let _alone = VOICED_SPEECH.lock().await;
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(speech());
    let srv = srv!(&mock);
    let m = many(&srv, &["es", "es", "en"], false).await;
    let (before, _) = live_latency(&srv).await;
    let mut phones = open_all(&srv, &m).await;
    phones[0].say(json!({ "type": "start" })).await;

    // Only the host talks (voiced, without a pause); three phones listen.
    let voice: Vec<u8> = std::iter::repeat_n(1000i16.to_le_bytes(), 2400)
        .flatten()
        .collect();
    all_speak(&mut phones, 4_000, |i| (i == 0).then(|| voice.clone())).await;
    let heard: usize = phones
        .iter()
        .skip(1)
        .map(|p| audio_from(&p.seen, &m.host_peer).len())
        .sum();
    assert!(
        heard >= 3,
        "the three listeners should all hear the host: {heard}"
    );

    let (after, _) = live_latency(&srv).await;
    assert_eq!(
        after - before,
        1,
        "one utterance is one observation, however many phones hear it"
    );
}

#[tokio::test]
async fn with_three_seats_an_echo_is_dropped_whoever_it_reaches_and_a_real_reply_still_gets_through(
) {
    let _alone = VOICED_SPEECH.lock().await;
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    // Every upstream session hears the same sentence: three phones side by side all pick up the
    // host's voice, and the guests' sessions "translate" it.
    mock.reply_with(vec![
        Reply::Transcript {
            original: "ciao come stai oggi".into(),
            translated: "hola como estas hoy".into(),
        },
        Reply::Audio(vec![7u8; 960]),
    ]);
    let srv = srv!(&mock);
    let m = many(&srv, &["es", "en"], false).await;
    let suppressed_before = echo_metric(&srv, "suppressed").await;
    let mut phones = open_all(&srv, &m).await;
    let (host, g1, g2) = (
        phones[0].peer.clone(),
        phones[1].peer.clone(),
        phones[2].peer.clone(),
    );

    // The host speaks first and their own caption is up before anything else happens.
    phones[0].say(json!({ "type": "start" })).await;
    all_speak(&mut phones, 2_500, |i| (i == 0).then(|| tone(1000, 4800))).await;
    // Now both guests' microphones open (quietly: they only hear the host across the table).
    phones[1].say(json!({ "type": "start" })).await;
    phones[2].say(json!({ "type": "start" })).await;
    all_speak(&mut phones, 4_500, |i| {
        Some(if i == 0 {
            tone(1000, 4800)
        } else {
            tone(200, 4800)
        })
    })
    .await;

    // Both guests still hear the host's real translation...
    for i in [1usize, 2] {
        assert!(
            !audio_from(&phones[i].seen, &host).is_empty(),
            "guest {i} never heard the host"
        );
    }
    // ...the host sees their own words but none of the guests' sessions (the host's voice, read back)...
    assert!(phones[0]
        .seen
        .iter()
        .any(|v| v["type"] == "subtitle_interim" && v["speaker_id"] == host.as_str()));
    for guest in [&g1, &g2] {
        let echoed = speech_of(&phones[0].seen, guest);
        assert!(
            echoed.is_empty(),
            "the host was played their own sentence back: {echoed:?}"
        );
    }
    // ...and neither guest is handed the OTHER guest's reading of the host: that is the case a
    // two-microphone filter could not tell, because it only knew "me" and "the other".
    let g2_hears_g1 = speech_of(&phones[2].seen, &g1);
    assert!(
        g2_hears_g1.is_empty(),
        "guest 2 got guest 1's echo: {g2_hears_g1:?}"
    );
    let g1_hears_g2 = speech_of(&phones[1].seen, &g2);
    assert!(
        g1_hears_g2.is_empty(),
        "guest 1 got guest 2's echo: {g1_hears_g2:?}"
    );
    assert!(echo_metric(&srv, "suppressed").await > suppressed_before);

    // A genuine reply from guest 1 is a different sentence and reaches both others.
    let room = voxtranslate_server::live::ws::room_name(m.session);
    for to in [&host, &g2] {
        for frame in [
            ServerMessage::SubtitleInterim {
                speaker_id: g1.clone(),
                speaker_name: "Guest0".into(),
                text: "sto bene grazie".into(),
                lang: "es".into(),
                original: Some("estoy bien gracias".into()),
            },
            ServerMessage::TranslatedAudio {
                speaker_id: g1.clone(),
                lang: "it".into(),
                seq: 99,
                pcm16_b64: "AAAA".into(),
            },
        ] {
            assert!(srv.state.rooms.relay_to_peer(&room, to, &frame.to_json()));
        }
    }
    all_speak(&mut phones, 2_500, |i| {
        Some(if i == 0 {
            tone(1000, 4800)
        } else {
            tone(200, 4800)
        })
    })
    .await;
    for i in [0usize, 2] {
        assert!(
            phones[i]
                .seen
                .iter()
                .any(|v| v["type"] == "subtitle_interim" && v["text"] == "sto bene grazie"),
            "the real reply never reached phone {i}"
        );
    }
}

// ---- tier choice (S4): Premium end to end, fallback, money ------------------------------

const PREMIUM: &str = "gemini_live_translate";

/// Every distinct per-tick amount a seat was charged, with how many ticks each had.
async fn tick_amounts(srv: &Srv, session: Uuid, peer: &str) -> Vec<(Decimal, i64)> {
    sqlx::query_as(
        "SELECT amount, count(*) FROM live_usage_ticks
         WHERE session_id = $1 AND peer_id = $2::uuid GROUP BY amount ORDER BY amount",
    )
    .bind(session)
    .bind(peer)
    .fetch_all(&srv.pool)
    .await
    .unwrap()
}

fn rate_per_second(srv: &Srv, engine: &str) -> f64 {
    srv.state
        .engines
        .get(engine)
        .expect("registered")
        .metadata()
        .user_rate_per_second()
}

fn downgrades(frames: &[Value]) -> Vec<&Value> {
    frames
        .iter()
        .filter(|v| v["type"] == "engine_downgraded")
        .collect()
}

#[tokio::test]
async fn a_premium_conversation_runs_on_the_premium_engine_and_bills_its_rate_for_every_seat() {
    let standard = RealtimeMock::start(Dialect::Qwen).await;
    let gemini = RealtimeMock::start(Dialect::Gemini).await;
    gemini.reply_with(speech());
    let Some(srv) = setup_tiers(&standard, &gemini, 8).await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let m = many_on(&srv, &["es"], false, Some(PREMIUM)).await;
    let mut phones = open_all(&srv, &m).await;
    for p in &mut phones {
        p.say(json!({ "type": "start" })).await;
    }
    all_speak(&mut phones, 4_500, |_| Some(vec![0u8; 640])).await;
    for p in &mut phones {
        p.say(json!({ "type": "stop" })).await;
    }
    tokio::time::sleep(Duration::from_millis(800)).await;

    // Both phones were served by Gemini; Standard's provider was never dialled.
    assert!(gemini.connections() >= 2, "{}", gemini.connections());
    assert_eq!(
        standard.connections(),
        0,
        "Standard served a Premium conversation"
    );
    // The other phone really hears the translated audio, and nobody sees a fallback notice.
    for (listener, speaker) in [(1, 0), (0, 1)] {
        assert!(
            !audio_from(&phones[listener].seen, &phones[speaker].peer).is_empty(),
            "phone {listener} never heard phone {speaker}"
        );
    }
    assert!(phones.iter().all(|p| downgrades(&p.seen).is_empty()));

    // Every tick of both seats is exactly one stream at the PREMIUM rate, not Standard's.
    let premium_tick = usd(rate_per_second(&srv, PREMIUM));
    let standard_tick = usd(rate_per_second(&srv, "standard"));
    assert!(premium_tick > standard_tick * Decimal::from(10));
    let mut total = Decimal::ZERO;
    for peer in [&m.host_peer, &m.guests[0].1] {
        let amounts = tick_amounts(&srv, m.session, peer).await;
        assert_eq!(
            amounts.len(),
            1,
            "{peer} was charged at several rates: {amounts:?}"
        );
        let (amount, n) = amounts[0];
        assert_eq!(amount, premium_tick, "{peer}");
        assert!(n >= 2, "{peer} was metered {n} times");
        total += amount * Decimal::from(n);
    }
    let left: Decimal = sqlx::query_scalar("SELECT balance FROM users WHERE id = $1")
        .bind(m.host_user)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert_eq!(
        left,
        Decimal::new(500, 2) - total,
        "the owner paid exactly the ticks"
    );
    let tagged: Option<String> =
        sqlx::query_scalar("SELECT engine_id FROM usage_sessions WHERE user_id = $1")
            .bind(m.host_user)
            .fetch_optional(&srv.pool)
            .await
            .unwrap();
    assert_eq!(tagged.as_deref(), Some(PREMIUM));
    let stored: Option<String> =
        sqlx::query_scalar("SELECT engine_id FROM live_sessions WHERE id = $1")
            .bind(m.session)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(
        stored.as_deref(),
        Some(PREMIUM),
        "no fallback, so the tier is unchanged"
    );
}

#[tokio::test]
async fn premium_at_capacity_moves_the_whole_conversation_to_standard_and_bills_it_that_way() {
    let standard = RealtimeMock::start(Dialect::Qwen).await;
    standard.reply_with(speech());
    let gemini = RealtimeMock::start(Dialect::Gemini).await;
    gemini.reply_with(speech());
    // One Premium upstream session in all: the host's takes it, the guest's finds none.
    let Some(srv) = setup_tiers(&standard, &gemini, 1).await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let m = many_on(&srv, &["es"], false, Some(PREMIUM)).await;
    let mut phones = open_all(&srv, &m).await;

    // The host starts alone first and holds the only Premium session.
    phones[0].say(json!({ "type": "start" })).await;
    all_speak(&mut phones, 2_500, |i| (i == 0).then(|| vec![0u8; 640])).await;
    assert!(gemini.connections() >= 1);
    assert_eq!(standard.connections(), 0);

    // Now the guest starts: Premium has no session left. Nobody is told "busy".
    phones[1].say(json!({ "type": "start" })).await;
    all_speak(&mut phones, 3_500, |_| Some(vec![0u8; 640])).await;
    for (i, p) in phones.iter().enumerate() {
        let notices = downgrades(&p.seen);
        assert_eq!(
            notices.len(),
            1,
            "phone {i} must be told exactly once: {:?}",
            p.seen
        );
        assert_eq!(notices[0]["from"], PREMIUM);
        assert_eq!(notices[0]["to"], "standard");
        assert_eq!(notices[0]["reason"], "premium_at_capacity");
        assert_eq!(notices[0]["peer_id"], json!(phones[1].peer));
        assert!(
            !p.seen.iter().any(|v| v["code"] == "live_busy"),
            "a fallback reached the client as live_busy: {:?}",
            p.seen
        );
    }
    assert!(
        standard.connections() >= 1,
        "the guest was not served by Standard"
    );
    let (stored, events): (Option<String>, i64) = (
        sqlx::query_scalar("SELECT engine_id FROM live_sessions WHERE id = $1")
            .bind(m.session)
            .fetch_one(&srv.pool)
            .await
            .unwrap(),
        sqlx::query_scalar(
            "SELECT count(*) FROM live_session_events WHERE session_id = $1 AND kind = 'engine_downgraded'",
        )
        .bind(m.session)
        .fetch_one(&srv.pool)
        .await
        .unwrap(),
    );
    assert_eq!(
        stored.as_deref(),
        Some("standard"),
        "a reconnect must agree"
    );
    assert_eq!(events, 1, "one diagnostics row for the one fallback");

    // The host, still on its Premium session, stops and starts again: it is Standard now, and
    // the conversation (REST) says so.
    phones[0].say(json!({ "type": "stop" })).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let before = standard.connections();
    phones[0].say(json!({ "type": "start" })).await;
    all_speak(&mut phones, 3_000, |_| Some(vec![0u8; 640])).await;
    assert!(
        standard.connections() > before,
        "a seat that starts after the fallback must not retry Premium"
    );
    for p in &mut phones {
        p.say(json!({ "type": "stop" })).await;
    }
    tokio::time::sleep(Duration::from_millis(800)).await;
    let (s, view) = srv
        .http
        .get(format!(
            "http://{}/api/live/sessions/{}",
            srv.addr, m.session
        ))
        .bearer_auth(&m.host_token)
        .send()
        .await
        .map(|r| (r.status(), r))
        .unwrap();
    assert_eq!(s, StatusCode::OK);
    let view: Value = view.json().await.unwrap();
    assert_eq!(view["session"]["engine"], "standard");

    // Money: the guest only ever paid Standard's rate. The host paid Premium's while it ran on
    // Premium and Standard's afterwards — never anything else, and never both rates at once.
    let premium_tick = usd(rate_per_second(&srv, PREMIUM));
    let standard_tick = usd(rate_per_second(&srv, "standard"));
    let guest = tick_amounts(&srv, m.session, &m.guests[0].1).await;
    assert_eq!(
        guest.iter().map(|(a, _)| *a).collect::<Vec<_>>(),
        vec![standard_tick],
        "the downgraded seat was billed at a rate other than Standard's: {guest:?}"
    );
    let host = tick_amounts(&srv, m.session, &m.host_peer).await;
    assert_eq!(
        host.iter().map(|(a, _)| *a).collect::<Vec<_>>(),
        vec![standard_tick, premium_tick],
        "the host: Premium until it restarted, Standard after: {host:?}"
    );
    let total: Decimal = guest
        .iter()
        .chain(host.iter())
        .map(|(a, n)| *a * Decimal::from(*n))
        .sum();
    let left: Decimal = sqlx::query_scalar("SELECT balance FROM users WHERE id = $1")
        .bind(m.host_user)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert_eq!(left, Decimal::new(500, 2) - total);
}

#[tokio::test]
async fn a_standard_conversation_never_touches_premium_even_when_premium_is_configured() {
    let standard = RealtimeMock::start(Dialect::Qwen).await;
    standard.reply_with(speech());
    let gemini = RealtimeMock::start(Dialect::Gemini).await;
    gemini.reply_with(speech());
    let Some(srv) = setup_tiers(&standard, &gemini, 1).await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let m = many(&srv, &["es"], false).await;
    let mut phones = open_all(&srv, &m).await;
    for p in &mut phones {
        p.say(json!({ "type": "start" })).await;
    }
    all_speak(&mut phones, 3_500, |_| Some(vec![0u8; 640])).await;
    assert_eq!(gemini.connections(), 0);
    assert!(standard.connections() >= 2);
    assert!(phones.iter().all(|p| downgrades(&p.seen).is_empty()));
    let standard_tick = usd(rate_per_second(&srv, "standard"));
    for peer in [&m.host_peer, &m.guests[0].1] {
        let amounts = tick_amounts(&srv, m.session, peer).await;
        assert_eq!(
            amounts.iter().map(|(a, _)| *a).collect::<Vec<_>>(),
            vec![standard_tick]
        );
    }
}

#[tokio::test]
async fn starting_to_speak_on_a_tier_needs_that_tiers_runway_not_just_standards() {
    let standard = RealtimeMock::start(Dialect::Qwen).await;
    let gemini = RealtimeMock::start(Dialect::Gemini).await;
    gemini.reply_with(speech());
    let Some(srv) = setup_tiers(&standard, &gemini, 8).await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let m = many_on(&srv, &["es"], false, Some(PREMIUM)).await;
    // Created with plenty; then the wallet drops to $0.50: Standard's minimum ($0.05) is met,
    // Premium's ($1.00) is not.
    sqlx::query("UPDATE users SET balance = 0.50 WHERE id = $1")
        .bind(m.host_user)
        .execute(&srv.pool)
        .await
        .unwrap();
    let mut phones = open_all(&srv, &m).await;
    phones[0].say(json!({ "type": "start" })).await;
    let refusal = phones[0]
        .wait_for(|v| v["type"] == "error")
        .await
        .expect("no refusal");
    assert_eq!(refusal["code"], "insufficient_balance", "{refusal}");
    assert_eq!(
        gemini.connections(),
        0,
        "nothing was opened for an owner who cannot pay"
    );
    assert_eq!(standard.connections(), 0);
}
