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
use voxtranslate_server::config::{Config, QwenConfig};
use voxtranslate_server::db::{self, Pool};
use voxtranslate_server::engine::realtime_mock::{Dialect, RealtimeMock, Reply};
use voxtranslate_server::live::config::LiveConfig;
use voxtranslate_server::live::sweeper::sweep_sessions;
use voxtranslate_server::live::LiveState;
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::transcripts::TranscriptService;
use voxtranslate_server::{app, AppState};

const SECRET: &str = "live-ws-secret";
const LIVE_KEY: &str = "0123456789abcdef0123456789abcdef-live-ws";

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

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
    // production). Charging only in arrears let every one of them through unpaid.
    for _ in 0..4 {
        say(&mut host, json!({ "type": "start" })).await;
        let _ = both_speak(&mut host, &mut guest, 600).await;
        say(&mut host, json!({ "type": "stop" })).await;
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
