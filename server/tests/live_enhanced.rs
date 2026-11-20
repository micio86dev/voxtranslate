//! Enhanced (`cartesia`) in Live Connect (spec 0121, S5): each phone does its own speech-to-text
//! and sends the text, the server translates and addresses every other phone, each listener's
//! phone speaks it. Cartesia and the translation model are local stand-ins; the room, the
//! sockets, the meter and the database are the real ones.
//!
//! DB-gated like the other integration suites. Run against the local pgvector Postgres:
//! `DATABASE_URL=postgres://vox:vox_local_dev@127.0.0.1:55432/voxtranslate_voip_test \
//!   cargo test --test live_enhanced`. A run of a few milliseconds skipped everything.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode as AxStatus};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use futures::{SinkExt as _, StreamExt as _};
use reqwest::StatusCode;
use rust_decimal::Decimal;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::{CartesiaConfig, Config, QwenConfig};
use voxtranslate_server::db::{self, Pool};
use voxtranslate_server::engine::realtime_mock::{Dialect, RealtimeMock, Reply};
use voxtranslate_server::live::config::LiveConfig;
use voxtranslate_server::live::LiveState;
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::transcripts::TranscriptService;
use voxtranslate_server::{app, AppState};

const SECRET: &str = "live-enhanced-secret";
const LIVE_KEY: &str = "0123456789abcdef0123456789abcdef-live-enhanced";
const SERVER_KEY: &str = "sk_car_server_side_only";
const CARTESIA: &str = "cartesia";

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

// ---- the translation model stand-in ----------------------------------------------------

/// Answers `"<target code>|<text>"`, so a test can tell which language a caption was made for and
/// what it was made from.
#[derive(Default)]
struct MockGroq {
    calls: AtomicUsize,
    status: AtomicU16,
    targets: Mutex<Vec<String>>,
}

impl MockGroq {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    fn fail(&self) {
        self.status.store(400, Ordering::SeqCst);
    }
    fn targets(&self) -> Vec<String> {
        self.targets.lock().unwrap().clone()
    }
}

fn code_of_language(name: &str) -> &'static str {
    match name {
        "Spanish" => "es",
        "English" => "en",
        "French" => "fr",
        "German" => "de",
        "Italian" => "it",
        _ => "??",
    }
}

async fn chat(State(m): State<Arc<MockGroq>>, Json(body): Json<Value>) -> impl IntoResponse {
    m.calls.fetch_add(1, Ordering::SeqCst);
    let system = body["messages"][0]["content"].as_str().unwrap_or_default();
    let user = body["messages"][1]["content"].as_str().unwrap_or_default();
    let target = system
        .split(" to ")
        .nth(1)
        .and_then(|rest| rest.split('.').next())
        .map(code_of_language)
        .unwrap_or("??");
    m.targets.lock().unwrap().push(target.to_string());
    let status = m.status.load(Ordering::SeqCst);
    if status != 0 {
        return (
            AxStatus::from_u16(status).unwrap(),
            Json(json!({ "error": { "message": "no" } })),
        );
    }
    (
        AxStatus::OK,
        Json(json!({ "choices": [{ "message": { "content": format!("{target}|{user}") } }] })),
    )
}

// ---- Cartesia stand-in -----------------------------------------------------------------

#[derive(Clone, Default)]
struct Cartesia {
    mints: Arc<AtomicUsize>,
    fail: Arc<AtomicU16>,
    grants: Arc<Mutex<Vec<Value>>>,
    auth_seen: Arc<Mutex<Vec<String>>>,
}

async fn access_token(
    State(c): State<Cartesia>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    c.mints.fetch_add(1, Ordering::SeqCst);
    c.grants.lock().unwrap().push(body["grants"].clone());
    c.auth_seen.lock().unwrap().push(
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string(),
    );
    let code = c.fail.load(Ordering::SeqCst);
    if code != 0 {
        return (AxStatus::from_u16(code).unwrap(), "nope").into_response();
    }
    Json(json!({ "token": "scoped-token" })).into_response()
}

// ---- the server ------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Opts {
    /// `CARTESIA_ENHANCED` on.
    enhanced: bool,
    /// `CARTESIA_VOICE_CLONING_ENABLED`.
    cloning: bool,
    /// `LIVE_ECHO_FILTER`.
    echo_filter: bool,
    /// Starting balance of the users `login` creates, in cents.
    balance_cents: i64,
}

impl Opts {
    fn enhanced() -> Self {
        Self {
            enhanced: true,
            cloning: true,
            echo_filter: true,
            balance_cents: 500,
        }
    }
}

struct Srv {
    addr: SocketAddr,
    pool: Pool,
    http: reqwest::Client,
    state: AppState,
    groq: Arc<MockGroq>,
    cartesia: Cartesia,
    realtime: RealtimeMock,
    balance_cents: i64,
}

async fn setup(opts: Opts) -> Option<Srv> {
    let url = db::test_database_url()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;

    let groq = Arc::new(MockGroq::default());
    let router = Router::new()
        .route("/chat/completions", post(chat))
        .with_state(groq.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let groq_url = format!("http://{}/chat/completions", l.local_addr().ok()?);
    tokio::spawn(async move {
        let _ = axum::serve(l, router).await;
    });

    let cartesia = Cartesia::default();
    let router = Router::new()
        .route("/access-token", post(access_token))
        .with_state(cartesia.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let api_base = format!("http://{}", l.local_addr().ok()?);
    tokio::spawn(async move {
        let _ = axum::serve(l, router).await;
    });

    let realtime = RealtimeMock::start(Dialect::Qwen).await;
    realtime.reply_with(vec![
        Reply::Transcript {
            original: "ciao a tutti".into(),
            translated: "hola a todos".into(),
        },
        Reply::Audio(vec![7u8; 960]),
    ]);

    let mut config = Config::test_with_billing(&url, SECRET, 5.0);
    config.groq_base_url = Some(groq_url);
    config.qwen = QwenConfig {
        api_key: "test-key".into(),
        endpoint: realtime.base_url(),
        ..Default::default()
    };
    config.standard_enabled = true;
    if opts.enhanced {
        // $0.036 x 1.5 = $0.054 a minute, $0.0009 a second: a round number per one-second tick.
        config.cartesia = Some(CartesiaConfig {
            api_key: SERVER_KEY.into(),
            stt_model: "ink-whisper".into(),
            stt_model_by_lang: HashMap::from([("en".to_string(), "ink-2".to_string())]),
            tts_model: "sonic-3.5".into(),
            cost_per_minute: 0.036,
            markup: 0.5,
            voice_cloning_enabled: opts.cloning,
            default_voice_id: Some("fallback-voice".into()),
            api_base,
            stt_endpoint: "wss://cartesia.test/stt".into(),
            tts_endpoint: "wss://cartesia.test/tts".into(),
            version: "2025-04-16".into(),
        });
    }
    config
        .billing
        .as_mut()
        .unwrap()
        .pricing
        .usage_update_interval = 1;
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
            "LIVE_MAX_GUESTS" => Some("3".into()),
            "LIVE_ECHO_FILTER" if !opts.echo_filter => Some("0".into()),
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
        groq,
        cartesia,
        realtime,
        balance_cents: opts.balance_cents,
    })
}

macro_rules! srv {
    ($opts:expr) => {
        match setup($opts).await {
            Some(s) => s,
            None => {
                eprintln!("skipping — no DATABASE_URL");
                return;
            }
        }
    };
}

impl Srv {
    async fn call(
        &self,
        method: &str,
        path: &str,
        bearer: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let url = format!("http://{}{path}", self.addr);
        let mut req = match method {
            "POST" => self.http.post(url),
            "DELETE" => self.http.delete(url),
            _ => self.http.get(url),
        };
        if let Some(b) = body {
            req = req.json(&b);
        }
        if let Some(t) = bearer {
            req = req.bearer_auth(t);
        }
        let res = req.send().await.unwrap();
        let status = res.status();
        (status, res.json().await.unwrap_or(Value::Null))
    }
    async fn post(&self, path: &str, bearer: Option<&str>, body: Value) -> (StatusCode, Value) {
        self.call("POST", path, bearer, Some(body)).await
    }
    async fn get(&self, path: &str, bearer: &str) -> (StatusCode, Value) {
        self.call("GET", path, Some(bearer), None).await
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
        Decimal::new(srv.balance_cents, 2),
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

async fn give_clone(srv: &Srv, user: Uuid, voice: &str) {
    sqlx::query("UPDATE users SET cartesia_voice_id = $2 WHERE id = $1")
        .bind(user)
        .bind(voice)
        .execute(&srv.pool)
        .await
        .unwrap();
}

/// One conversation: an `it` host and one guest per entry of `guest_langs`.
struct Convo {
    session: Uuid,
    jwt: String,
    user: Uuid,
    host_token: String,
    host_peer: String,
    /// `(token, peer)` per guest, in join order.
    guests: Vec<(String, String)>,
    created: Value,
}

struct Make<'a> {
    engine: Option<&'a str>,
    guest_langs: &'a [&'static str],
    voice_clone: bool,
    clone_id: Option<&'a str>,
    save_transcript: bool,
}

impl Default for Make<'_> {
    fn default() -> Self {
        Self {
            engine: Some(CARTESIA),
            guest_langs: &["es"],
            voice_clone: false,
            clone_id: None,
            save_transcript: false,
        }
    }
}

async fn convo(srv: &Srv, m: Make<'_>) -> Convo {
    let (jwt, user) = login(srv).await;
    if let Some(v) = m.clone_id {
        give_clone(srv, user, v).await;
    }
    let (s, created) = srv
        .post(
            "/api/live/sessions",
            Some(&jwt),
            json!({
                "lang": "it",
                "engine": m.engine,
                "voice_clone": m.voice_clone,
                "max_guests": m.guest_langs.len(),
                "save_transcript": m.save_transcript,
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
    for (i, lang) in m.guest_langs.iter().enumerate() {
        let (s, joined) = srv
            .post(
                "/api/live/join",
                None,
                json!({ "code": code, "name": format!("Guest{i}"), "lang": lang, "consent": true }),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{joined}");
        guests.push((
            joined["token"].as_str().unwrap().to_string(),
            joined["peer_id"].as_str().unwrap().to_string(),
        ));
    }
    Convo {
        session: created["session_id"].as_str().unwrap().parse().unwrap(),
        jwt,
        user,
        host_token: created["token"].as_str().unwrap().into(),
        host_peer: created["peer_id"].as_str().unwrap().into(),
        guests,
        created,
    }
}

// ---- sockets ---------------------------------------------------------------------------

async fn open(srv: &Srv, token: &str) -> Ws {
    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://{}/ws/live?token={token}", srv.addr))
            .await
            .expect("upgrade");
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

/// Everything `ws` receives for `ms`.
async fn collect(ws: &mut Ws, ms: u64) -> Vec<Value> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    while let Ok(Some(Ok(frame))) = tokio::time::timeout_at(deadline, ws.next()).await {
        if let Message::Text(t) = frame {
            if let Ok(v) = serde_json::from_str::<Value>(t.as_str()) {
                out.push(v);
            }
        }
    }
    out
}

async fn say(ws: &mut Ws, v: Value) {
    let _ = ws.send(Message::text(v.to_string())).await;
}

/// Open the phone's meter and give the server a moment to finish starting it.
async fn start(ws: &mut Ws) {
    say(ws, json!({ "type": "start" })).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
}

async fn text(ws: &mut Ws, segment: &str, body: &str, is_final: bool) {
    say(
        ws,
        json!({ "type": "translate_text", "segment_id": segment, "text": body, "final": is_final }),
    )
    .await;
}

fn of_type<'a>(frames: &'a [Value], ty: &str) -> Vec<&'a Value> {
    frames.iter().filter(|v| v["type"] == ty).collect()
}

fn is_listener_frame(v: &Value) -> bool {
    matches!(
        v["type"].as_str(),
        Some("subtitle_interim" | "subtitle_final" | "enhanced_speak")
    )
}

/// The Enhanced rate for one stream for one second, exactly as the server computes it.
fn enhanced_unit(streams: u32) -> Decimal {
    usd(0.036 * (1.0 + 0.5) / 60.0 * 1.0 * f64::from(streams))
}

async fn ticks(srv: &Srv, session: Uuid, peer: &str) -> Vec<Decimal> {
    sqlx::query_scalar("SELECT amount FROM live_usage_ticks WHERE session_id = $1 AND peer_id = $2")
        .bind(session)
        .bind(peer.parse::<Uuid>().unwrap())
        .fetch_all(&srv.pool)
        .await
        .unwrap()
}

async fn balance(srv: &Srv, user: Uuid) -> Decimal {
    sqlx::query_scalar("SELECT balance FROM users WHERE id = $1")
        .bind(user)
        .fetch_one(&srv.pool)
        .await
        .unwrap()
}

// ===== create, join, read ===============================================================

#[tokio::test]
async fn an_enhanced_conversation_is_stored_and_every_seat_is_told_its_tier() {
    let srv = srv!(Opts::enhanced());
    let c = convo(&srv, Make::default()).await;
    assert_eq!(c.created["engine"], CARTESIA);
    assert_eq!(c.created["voice_clone"], false, "a clone is opt-in");

    let (_, host_view) = srv
        .get(&format!("/api/live/sessions/{}", c.session), &c.host_token)
        .await;
    assert_eq!(host_view["session"]["engine"], CARTESIA);
    assert_eq!(host_view["session"]["voice_clone"], false);
    let (_, guest_view) = srv
        .get(&format!("/api/live/sessions/{}", c.session), &c.guests[0].0)
        .await;
    assert_eq!(guest_view["session"]["engine"], CARTESIA);
    assert_eq!(guest_view["session"]["voice_clone"], false);
}

#[tokio::test]
async fn a_guests_join_answer_carries_the_tier_and_the_voice_flag() {
    let srv = srv!(Opts::enhanced());
    let (jwt, user) = login(&srv).await;
    give_clone(&srv, user, "voice-host-1").await;
    let (_, created) = srv
        .post(
            "/api/live/sessions",
            Some(&jwt),
            json!({ "lang": "it", "engine": CARTESIA, "voice_clone": true }),
        )
        .await;
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
    assert_eq!(joined["engine"], CARTESIA);
    assert_eq!(joined["voice_clone"], true);
    assert!(
        !joined.to_string().contains("voice-host-1"),
        "the voice id never travels in a join answer: {joined}"
    );
}

#[tokio::test]
async fn the_voice_clone_option_is_validated_per_conversation() {
    let srv = srv!(Opts::enhanced());
    let (jwt, user) = login(&srv).await;
    let create = |body: Value| {
        let (srv, jwt) = (&srv, jwt.clone());
        async move { srv.post("/api/live/sessions", Some(&jwt), body).await }
    };

    // No clone on the account: refused, and nothing is opened.
    let (s, b) = create(json!({ "lang": "it", "engine": CARTESIA, "voice_clone": true })).await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("voice_clone_unavailable"))
    );
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM live_participants WHERE user_id = $1 AND role = 'host'",
    )
    .bind(user)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(open, 0, "a refusal opens nothing");

    give_clone(&srv, user, "voice-host-1").await;
    // Only the Enhanced tier plays a voice.
    let (s, b) = create(json!({ "lang": "it", "voice_clone": true })).await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("voice_clone_unavailable")),
        "Standard has no voice to clone into"
    );
    // Not a boolean.
    let (s, b) = create(json!({ "lang": "it", "engine": CARTESIA, "voice_clone": "yes" })).await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("bad_voice_clone"))
    );
    // The opt-in works, is stored for THIS conversation and read back.
    let (s, b) = create(json!({ "lang": "it", "engine": CARTESIA, "voice_clone": true })).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["voice_clone"], true);
    let stored: bool =
        sqlx::query_scalar("SELECT use_voice_clone FROM live_sessions WHERE id = $1")
            .bind(b["session_id"].as_str().unwrap().parse::<Uuid>().unwrap())
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert!(stored);
    // The next conversation of the same user does not inherit it.
    let (s, b) = create(json!({ "lang": "it", "engine": CARTESIA })).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["voice_clone"], false);
    // Nor does `false` mean anything else.
    let (s, b) = create(json!({ "lang": "it", "engine": CARTESIA, "voice_clone": false })).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["voice_clone"], false);
}

#[tokio::test]
async fn a_clone_needs_cloning_to_be_enabled_on_the_deployment() {
    let srv = srv!(Opts {
        cloning: false,
        ..Opts::enhanced()
    });
    let (jwt, user) = login(&srv).await;
    give_clone(&srv, user, "voice-host-1").await;
    let (s, b) = srv
        .post(
            "/api/live/sessions",
            Some(&jwt),
            json!({ "lang": "it", "engine": CARTESIA, "voice_clone": true }),
        )
        .await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("voice_clone_unavailable"))
    );
}

#[tokio::test]
async fn enhanced_is_refused_when_off_and_for_business_and_costs_a_scaled_minimum() {
    // Off on this deployment: refused like any unknown tier.
    let off = srv!(Opts {
        enhanced: false,
        ..Opts::enhanced()
    });
    let (jwt, _) = login(&off).await;
    let (s, b) = off
        .post(
            "/api/live/sessions",
            Some(&jwt),
            json!({ "lang": "it", "engine": CARTESIA }),
        )
        .await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("engine_not_available"))
    );

    let srv = srv!(Opts {
        balance_cents: 50,
        ..Opts::enhanced()
    });
    let (jwt, _) = login(&srv).await;
    // A business conversation is Standard only, whatever else is true.
    let (s, b) = srv
        .post(
            "/api/live/sessions",
            Some(&jwt),
            json!({ "lang": "it", "engine": CARTESIA, "org_id": Uuid::new_v4(),
                    "project_id": Uuid::new_v4() }),
        )
        .await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("engine_not_allowed"))
    );
    // Standard needs $0.05; Enhanced costs 12x as much a minute so it needs the same runway,
    // $0.60. $0.50 buys the first and not the second.
    let (s, b) = srv
        .post(
            "/api/live/sessions",
            Some(&jwt),
            json!({ "lang": "it", "engine": CARTESIA }),
        )
        .await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::PAYMENT_REQUIRED, Some("insufficient_balance"))
    );
    let (s, b) = srv
        .post("/api/live/sessions", Some(&jwt), json!({ "lang": "it" }))
        .await;
    assert_eq!(s, StatusCode::OK, "Standard keeps its own minimum: {b}");

    let rich = srv!(Opts {
        balance_cents: 60,
        ..Opts::enhanced()
    });
    let (jwt, _) = login(&rich).await;
    let (s, b) = rich
        .post(
            "/api/live/sessions",
            Some(&jwt),
            json!({ "lang": "it", "engine": CARTESIA }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "$0.60 is exactly the minimum: {b}");
}

#[tokio::test]
async fn the_tier_list_offers_enhanced_with_this_users_voice_clone_state() {
    let srv = srv!(Opts::enhanced());
    let (jwt, user) = login(&srv).await;
    let (s, b) = srv.get("/api/live/engines", &jwt).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let list = b["engines"].as_array().unwrap();
    let ids: Vec<&str> = list.iter().map(|e| e["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["standard", CARTESIA], "Standard, then Enhanced");
    let e = &list[1];
    assert_eq!(e["client_direct"], true);
    assert_eq!(e["scales_per_language"], true);
    assert_eq!(e["available"], true);
    assert!((e["rate_per_minute"].as_f64().unwrap() - 0.054).abs() < 1e-9);
    assert!(
        (e["min_balance"].as_f64().unwrap() - 0.6).abs() < 1e-9,
        "{e}"
    );
    assert!(
        e.get("cost_per_minute").is_none() && e.get("markup").is_none(),
        "raw cost and markup never leave the server: {e}"
    );
    assert_eq!(e["voice_clone"]["available"], false, "no clone yet");
    assert_eq!(e["voice_clone"]["has_clone"], false);
    assert_eq!(e["voice_clone"]["cloning_enabled"], true);
    assert_eq!(list[0]["client_direct"], false);

    give_clone(&srv, user, "voice-host-1").await;
    let (_, b) = srv.get("/api/live/engines", &jwt).await;
    assert_eq!(b["engines"][1]["voice_clone"]["available"], true);
    assert!(
        !b.to_string().contains("voice-host-1"),
        "the id is not listed: {b}"
    );

    // Cloning switched off on the deployment: a clone exists but cannot be used.
    let no_clone = srv!(Opts {
        cloning: false,
        ..Opts::enhanced()
    });
    let (jwt, user) = login(&no_clone).await;
    give_clone(&no_clone, user, "voice-host-1").await;
    let (_, b) = no_clone.get("/api/live/engines", &jwt).await;
    assert_eq!(b["engines"][1]["voice_clone"]["available"], false);
    assert_eq!(b["engines"][1]["voice_clone"]["has_clone"], true);

    // Off on this deployment: not listed at all.
    let off = srv!(Opts {
        enhanced: false,
        ..Opts::enhanced()
    });
    let (jwt, _) = login(&off).await;
    let (_, b) = off.get("/api/live/engines", &jwt).await;
    let ids: Vec<&str> = b["engines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["standard"]);
}

// ===== the token endpoint ===============================================================

fn token_path(session: Uuid) -> String {
    format!("/api/live/sessions/{session}/enhanced/session")
}

#[tokio::test]
async fn a_host_and_a_guest_each_get_the_token_meet_clients_already_read() {
    let srv = srv!(Opts::enhanced());
    let c = convo(
        &srv,
        Make {
            voice_clone: true,
            clone_id: Some("voice-host-1"),
            ..Make::default()
        },
    )
    .await;
    for token in [&c.host_token, &c.guests[0].0] {
        let (s, b) = srv
            .post(&token_path(c.session), Some(token), json!({}))
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        // Exactly Meet's shape.
        assert_eq!(b["token"], "scoped-token");
        assert!(b["expires_at"].as_i64().unwrap() > chrono::Utc::now().timestamp());
        assert_eq!(b["cartesia_version"], "2025-04-16");
        assert_eq!(b["stt"]["endpoint"], "wss://cartesia.test/stt");
        assert_eq!(b["stt"]["model"], "ink-whisper");
        assert_eq!(b["stt"]["models_by_lang"]["en"], "ink-2");
        assert_eq!(b["tts"]["endpoint"], "wss://cartesia.test/tts");
        assert_eq!(b["tts"]["model"], "sonic-3.5");
        assert_eq!(b["voice_cloning_enabled"], true);
        assert_eq!(b["default_voice_id"], "fallback-voice");
        // Neither the account's clone nor the server's key.
        let all = b.to_string();
        assert!(!all.contains("voice-host-1"), "{all}");
        assert!(!all.contains(SERVER_KEY), "{all}");
    }
    // Both grants (the phone transcribes its own microphone AND speaks what it hears), and the
    // raw key only ever went to Cartesia.
    let grants = srv.cartesia.grants.lock().unwrap().clone();
    assert_eq!(grants.len(), 2);
    assert!(grants.iter().all(|g| g["stt"] == true && g["tts"] == true));
    assert!(srv
        .cartesia
        .auth_seen
        .lock()
        .unwrap()
        .iter()
        .all(|a| a == &format!("Bearer {SERVER_KEY}")));
}

#[tokio::test]
async fn the_token_endpoint_refuses_everyone_who_is_not_a_current_seat_of_an_enhanced_session() {
    let srv = srv!(Opts::enhanced());
    let a = convo(&srv, Make::default()).await;
    let b = convo(&srv, Make::default()).await;
    let std = convo(
        &srv,
        Make {
            engine: None,
            ..Make::default()
        },
    )
    .await;

    // No credential, the owner's account JWT, nonsense.
    let (s, _) = srv.post(&token_path(a.session), None, json!({})).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = srv
        .post(&token_path(a.session), Some(&a.jwt), json!({}))
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "an account JWT is not a seat");
    let (s, _) = srv
        .post(&token_path(a.session), Some("nonsense"), json!({}))
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    // Another conversation's token, host or guest.
    for t in [&b.host_token, &b.guests[0].0] {
        let (s, body) = srv.post(&token_path(a.session), Some(t), json!({})).await;
        assert_eq!(
            (s, body["code"].as_str()),
            (StatusCode::FORBIDDEN, Some("wrong_session"))
        );
    }
    // A Standard conversation has nothing to mint a token for.
    for t in [&std.host_token, &std.guests[0].0] {
        let (s, body) = srv.post(&token_path(std.session), Some(t), json!({})).await;
        assert_eq!(
            (s, body["code"].as_str()),
            (StatusCode::CONFLICT, Some("not_enhanced"))
        );
    }
    assert_eq!(
        srv.cartesia.mints.load(Ordering::SeqCst),
        0,
        "no refusal ever reached Cartesia"
    );

    // A seat that no longer exists.
    sqlx::query("DELETE FROM live_participants WHERE peer_id = $1")
        .bind(a.guests[0].1.parse::<Uuid>().unwrap())
        .execute(&srv.pool)
        .await
        .unwrap();
    let (s, _) = srv
        .post(&token_path(a.session), Some(&a.guests[0].0), json!({}))
        .await;
    assert_eq!(s, StatusCode::GONE);

    // An ended conversation, for the host and the guest alike.
    let (s, _) = srv
        .post(
            &format!("/api/live/sessions/{}/end", b.session),
            Some(&b.host_token),
            json!({}),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    for t in [&b.host_token, &b.guests[0].0] {
        let (s, body) = srv.post(&token_path(b.session), Some(t), json!({})).await;
        assert_eq!(
            (s, body["code"].as_str()),
            (StatusCode::GONE, Some("session_ended"))
        );
    }
    assert_eq!(srv.cartesia.mints.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_token_is_not_minted_for_an_owner_with_no_credit_and_cartesia_down_is_a_clean_502() {
    let srv = srv!(Opts::enhanced());
    let c = convo(&srv, Make::default()).await;
    sqlx::query("UPDATE users SET balance = 0 WHERE id = $1")
        .bind(c.user)
        .execute(&srv.pool)
        .await
        .unwrap();
    let (s, b) = srv
        .post(&token_path(c.session), Some(&c.guests[0].0), json!({}))
        .await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::PAYMENT_REQUIRED, Some("insufficient_balance")),
        "a guest cannot spend an owner who cannot pay"
    );
    assert_eq!(srv.cartesia.mints.load(Ordering::SeqCst), 0);

    sqlx::query("UPDATE users SET balance = 5 WHERE id = $1")
        .bind(c.user)
        .execute(&srv.pool)
        .await
        .unwrap();
    srv.cartesia.fail.store(500, Ordering::SeqCst);
    let (s, b) = srv
        .post(&token_path(c.session), Some(&c.host_token), json!({}))
        .await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::BAD_GATEWAY, Some("enhanced_unavailable"))
    );
    assert!(!b.to_string().contains(SERVER_KEY));
}

#[tokio::test]
async fn a_seat_cannot_farm_tokens_and_neither_can_a_conversation() {
    let srv = srv!(Opts::enhanced());
    let c = convo(
        &srv,
        Make {
            guest_langs: &["es", "en", "fr"],
            ..Make::default()
        },
    )
    .await;
    // One seat: 6 a minute, the 7th is refused.
    let mut answers = Vec::new();
    for _ in 0..7 {
        let (s, _) = srv
            .post(&token_path(c.session), Some(&c.guests[0].0), json!({}))
            .await;
        answers.push(s);
    }
    assert_eq!(
        answers.iter().filter(|s| **s == StatusCode::OK).count(),
        6,
        "{answers:?}"
    );
    assert_eq!(answers[6], StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(srv.cartesia.mints.load(Ordering::SeqCst), 6);

    // The conversation as a whole: 20 a minute across its four seats. 6 are spent; the other
    // three seats take 5 each (14 more is the room that is left).
    let mut ok = 6;
    'outer: for _ in 0..5 {
        for token in [&c.host_token, &c.guests[1].0, &c.guests[2].0] {
            let (s, _) = srv
                .post(&token_path(c.session), Some(token), json!({}))
                .await;
            if s == StatusCode::OK {
                ok += 1;
            } else {
                assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
                break 'outer;
            }
        }
    }
    assert_eq!(
        ok, 20,
        "no more than 20 tokens a minute for one conversation"
    );
    assert_eq!(srv.cartesia.mints.load(Ordering::SeqCst), 20);

    // The client address: 30 a minute across conversations. 20 are spent; a second conversation
    // (its own seat and conversation budgets untouched) gets 10 more and then the address is cut.
    let other = convo(&srv, Make::default()).await;
    let mut from_address = 0;
    for _ in 0..6 {
        for token in [&other.host_token, &other.guests[0].0] {
            let (s, body) = srv
                .post(&token_path(other.session), Some(token), json!({}))
                .await;
            if s == StatusCode::OK {
                from_address += 1;
            } else {
                assert_eq!(
                    (s, body["code"].as_str()),
                    (StatusCode::TOO_MANY_REQUESTS, Some("rate_limited"))
                );
            }
        }
    }
    assert_eq!(
        from_address, 10,
        "30 tokens a minute for one address, however many conversations"
    );
    assert_eq!(srv.cartesia.mints.load(Ordering::SeqCst), 30);
}

// ===== the conversation =================================================================

/// A final segment from the host reaches the guest as captions and a thing to speak; the host
/// is never sent its own words; a guest's reply never carries a voice.
#[tokio::test]
async fn two_phones_exchange_text_and_each_hears_the_other_in_the_right_voice() {
    let srv = srv!(Opts::enhanced());
    let c = convo(
        &srv,
        Make {
            voice_clone: true,
            clone_id: Some("voice-host-1"),
            ..Make::default()
        },
    )
    .await;
    let mut host = open(&srv, &c.host_token).await;
    let mut guest = open(&srv, &c.guests[0].0).await;
    start(&mut host).await;
    start(&mut guest).await;

    // The host speaks: a partial, then the segment.
    text(&mut host, "s1", "ciao a tutti", false).await;
    let interim = wait_for(&mut guest, |v| v["type"] == "subtitle_interim")
        .await
        .expect("a live caption");
    assert_eq!(interim["speaker_id"], c.host_peer.as_str());
    assert_eq!(interim["speaker_name"], "Marco");
    assert_eq!(
        interim["lang"], "it",
        "the SPEAKER's language, as the engines send it"
    );
    assert_eq!(
        interim["text"], "es|ciao a tutti",
        "translated for the listener"
    );
    assert_eq!(interim["original"], "ciao a tutti");

    text(&mut host, "s1", "ciao a tutti voi", true).await;
    let fin = wait_for(&mut guest, |v| v["type"] == "subtitle_final")
        .await
        .expect("the final caption");
    assert_eq!(fin["speaker_id"], c.host_peer.as_str());
    assert_eq!(fin["original"], "ciao a tutti voi");
    assert_eq!(fin["lang"], "it");
    assert_eq!(fin["translations"]["es"], "es|ciao a tutti voi");
    let speak = wait_for(&mut guest, |v| v["type"] == "enhanced_speak")
        .await
        .expect("what the guest's phone speaks");
    assert_eq!(speak["speaker_id"], c.host_peer.as_str());
    assert_eq!(speak["segment_id"], "s1");
    assert_eq!(speak["lang"], "es");
    assert_eq!(speak["text"], "es|ciao a tutti voi");
    assert_eq!(
        speak["voice_id"], "voice-host-1",
        "the host opted into their clone for this conversation"
    );

    // Nothing of the host's own segment came back to the host.
    let host_got = collect(&mut host, 500).await;
    assert!(
        host_got.iter().all(|v| !is_listener_frame(v)),
        "the speaker was sent their own words: {host_got:?}"
    );

    // The guest replies: the host gets captions and a thing to speak, in the DEFAULT voice: a
    // guest has no account and so no clone.
    text(&mut guest, "g1", "hola estoy muy bien gracias", true).await;
    let speak = wait_for(&mut host, |v| v["type"] == "enhanced_speak")
        .await
        .expect("the host's phone speaks the reply");
    assert_eq!(speak["speaker_id"], c.guests[0].1.as_str());
    assert_eq!(speak["lang"], "it");
    assert_eq!(speak["text"], "it|hola estoy muy bien gracias");
    assert!(speak["voice_id"].is_null(), "{speak}");
    let guest_got = collect(&mut guest, 400).await;
    assert!(
        guest_got.iter().all(|v| !is_listener_frame(v)),
        "{guest_got:?}"
    );
}

#[tokio::test]
async fn without_the_opt_in_even_the_host_speaks_with_the_default_voice() {
    let srv = srv!(Opts::enhanced());
    // The account HAS a clone, the conversation did not ask for it.
    let c = convo(
        &srv,
        Make {
            voice_clone: false,
            clone_id: Some("voice-host-1"),
            ..Make::default()
        },
    )
    .await;
    let mut host = open(&srv, &c.host_token).await;
    let mut guest = open(&srv, &c.guests[0].0).await;
    start(&mut host).await;
    text(&mut host, "s1", "ciao a tutti voi", true).await;
    let speak = wait_for(&mut guest, |v| v["type"] == "enhanced_speak")
        .await
        .expect("enhanced_speak");
    assert!(speak["voice_id"].is_null(), "{speak}");
}

#[tokio::test]
async fn a_clone_removed_or_cloning_switched_off_means_no_voice_not_a_broken_conversation() {
    let srv = srv!(Opts {
        cloning: false,
        ..Opts::enhanced()
    });
    let (jwt, user) = login(&srv).await;
    give_clone(&srv, user, "voice-host-1").await;
    // Created before cloning was switched off: force the flag as it would have been stored.
    let (_, created) = srv
        .post(
            "/api/live/sessions",
            Some(&jwt),
            json!({ "lang": "it", "engine": CARTESIA }),
        )
        .await;
    let session: Uuid = created["session_id"].as_str().unwrap().parse().unwrap();
    sqlx::query("UPDATE live_sessions SET use_voice_clone = true WHERE id = $1")
        .bind(session)
        .execute(&srv.pool)
        .await
        .unwrap();
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
            json!({ "code": code, "name": "L", "lang": "es", "consent": true }),
        )
        .await;
    let mut host = open(&srv, created["token"].as_str().unwrap()).await;
    let mut guest = open(&srv, joined["token"].as_str().unwrap()).await;
    start(&mut host).await;
    text(&mut host, "s1", "ciao a tutti voi", true).await;
    let speak = wait_for(&mut guest, |v| v["type"] == "enhanced_speak")
        .await
        .expect("enhanced_speak");
    assert!(speak["voice_id"].is_null(), "{speak}");
}

#[tokio::test]
async fn three_languages_cost_two_translations_and_everyone_gets_their_own() {
    let srv = srv!(Opts::enhanced());
    // Host it; guests es, es, en: one translation per DISTINCT language, not per phone.
    let c = convo(
        &srv,
        Make {
            guest_langs: &["es", "es", "en"],
            ..Make::default()
        },
    )
    .await;
    let mut host = open(&srv, &c.host_token).await;
    let mut es1 = open(&srv, &c.guests[0].0).await;
    let mut es2 = open(&srv, &c.guests[1].0).await;
    let mut en = open(&srv, &c.guests[2].0).await;
    start(&mut host).await;

    text(&mut host, "s1", "buongiorno a tutti voi", true).await;
    for ws in [&mut es1, &mut es2] {
        let speak = wait_for(ws, |v| v["type"] == "enhanced_speak")
            .await
            .unwrap();
        assert_eq!(speak["text"], "es|buongiorno a tutti voi");
        assert_eq!(speak["lang"], "es");
    }
    let speak = wait_for(&mut en, |v| v["type"] == "enhanced_speak")
        .await
        .unwrap();
    assert_eq!(speak["text"], "en|buongiorno a tutti voi");
    assert_eq!(speak["lang"], "en");
    assert_eq!(
        srv.groq.calls(),
        2,
        "two languages, two calls: {:?}",
        srv.groq.targets()
    );
    let mut targets = srv.groq.targets();
    targets.sort();
    assert_eq!(targets, ["en", "es"]);

    // The speaker heard nothing of it.
    assert!(collect(&mut host, 300)
        .await
        .iter()
        .all(|v| !is_listener_frame(v)));

    // A guest speaking reaches the other three, again one translation per language that is NOT
    // the speaker's own: the host (it) and the English phone; the other Spanish phone shares it.
    let before = srv.groq.calls();
    start(&mut es1).await;
    text(&mut es1, "e1", "hola a todos amigos", true).await;
    let to_es2 = wait_for(&mut es2, |v| v["type"] == "subtitle_final")
        .await
        .unwrap();
    assert_eq!(
        to_es2["translations"]["es"], "hola a todos amigos",
        "a listener who shares the speaker's language reads the original"
    );
    let to_host = wait_for(&mut host, |v| v["type"] == "enhanced_speak")
        .await
        .unwrap();
    assert_eq!(to_host["text"], "it|hola a todos amigos");
    let to_en = wait_for(&mut en, |v| v["type"] == "enhanced_speak")
        .await
        .unwrap();
    assert_eq!(to_en["text"], "en|hola a todos amigos");
    assert_eq!(srv.groq.calls() - before, 2);
}

#[tokio::test]
async fn a_listener_who_shares_the_speakers_language_reads_it_and_is_not_asked_to_speak_it() {
    let srv = srv!(Opts::enhanced());
    let c = convo(
        &srv,
        Make {
            guest_langs: &["it", "es"],
            ..Make::default()
        },
    )
    .await;
    let mut host = open(&srv, &c.host_token).await;
    let mut it = open(&srv, &c.guests[0].0).await;
    let mut es = open(&srv, &c.guests[1].0).await;
    start(&mut host).await;
    text(&mut host, "s1", "ciao a tutti voi", false).await;
    let interim = wait_for(&mut it, |v| v["type"] == "subtitle_interim")
        .await
        .unwrap();
    assert_eq!(interim["text"], "ciao a tutti voi");
    assert!(
        interim.get("original").is_none(),
        "absent, not null: the text IS the original: {interim}"
    );
    text(&mut host, "s1", "ciao a tutti voi", true).await;
    let fin = wait_for(&mut it, |v| v["type"] == "subtitle_final")
        .await
        .unwrap();
    assert_eq!(fin["translations"]["it"], "ciao a tutti voi");
    wait_for(&mut es, |v| v["type"] == "enhanced_speak")
        .await
        .unwrap();
    // The speaker shares the `it` group with that listener and still never hears itself.
    let host_got = collect(&mut host, 300).await;
    assert!(
        host_got.iter().all(|v| !is_listener_frame(v)),
        "the speaker was sent its own words: {host_got:?}"
    );
    let it_rest = collect(&mut it, 400).await;
    assert!(
        of_type(&it_rest, "enhanced_speak").is_empty(),
        "the same-language phone hears the speaker itself: {it_rest:?}"
    );
    assert_eq!(
        srv.groq.targets(),
        ["es", "es"],
        "only the other language was translated"
    );
}

#[tokio::test]
async fn an_unstarted_muted_oversized_or_malformed_frame_is_ignored_and_costs_nothing() {
    let srv = srv!(Opts::enhanced());
    let c = convo(&srv, Make::default()).await;
    let mut host = open(&srv, &c.host_token).await;
    let mut guest = open(&srv, &c.guests[0].0).await;

    // Not started yet: ignored.
    text(&mut host, "s1", "ciao a tutti voi", true).await;
    assert!(collect(&mut guest, 500)
        .await
        .iter()
        .all(|v| !is_listener_frame(v)));

    start(&mut host).await;
    // Oversized: never reaches the model; the speaker is told.
    text(&mut host, "s2", &"parola ".repeat(200), true).await;
    let told = wait_for(&mut host, |v| v["type"] == "error").await.unwrap();
    assert_eq!(told["code"], "text_too_long");
    // Empty, whitespace, a bad segment id, no id at all.
    text(&mut host, "s3", "   ", true).await;
    say(
        &mut host,
        json!({ "type": "translate_text", "segment_id": [1], "text": "ciao a tutti", "final": true }),
    )
    .await;
    say(
        &mut host,
        json!({ "type": "translate_text", "text": "ciao a tutti", "final": true }),
    )
    .await;
    // `lang` is ignored: the seat's declared language is authoritative.
    say(
        &mut host,
        json!({ "type": "translate_text", "segment_id": "ok", "text": "ciao a tutti voi",
                "final": true, "lang": "ja" }),
    )
    .await;
    let speak = wait_for(&mut guest, |v| v["type"] == "enhanced_speak")
        .await
        .unwrap();
    assert_eq!(speak["segment_id"], "ok");
    assert_eq!(
        speak["text"], "es|ciao a tutti voi",
        "translated FROM it, as declared"
    );
    assert_eq!(
        srv.groq.calls(),
        1,
        "only the one good frame was translated"
    );

    // Muted: nothing is sent on, and unmuting resumes.
    say(&mut host, json!({ "type": "mute_audio", "muted": true })).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    text(&mut host, "m1", "questo non va detto", true).await;
    assert!(collect(&mut guest, 500)
        .await
        .iter()
        .all(|v| v["type"] != "enhanced_speak"));
    say(&mut host, json!({ "type": "mute_audio", "muted": false })).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    text(&mut host, "m2", "adesso si puo parlare", true).await;
    let speak = wait_for(&mut guest, |v| v["type"] == "enhanced_speak")
        .await
        .unwrap();
    assert_eq!(speak["segment_id"], "m2");

    // Stop closes it again.
    say(&mut host, json!({ "type": "stop" })).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let calls = srv.groq.calls();
    text(&mut host, "x1", "dopo lo stop niente", true).await;
    assert!(collect(&mut guest, 500)
        .await
        .iter()
        .all(|v| v["type"] != "enhanced_speak"));
    assert_eq!(srv.groq.calls(), calls);
}

#[tokio::test]
async fn a_flood_of_frames_is_cut_to_eight_a_second() {
    let srv = srv!(Opts::enhanced());
    let c = convo(&srv, Make::default()).await;
    let mut host = open(&srv, &c.host_token).await;
    let mut guest = open(&srv, &c.guests[0].0).await;
    start(&mut host).await;
    for i in 0..40 {
        text(
            &mut host,
            &format!("f{i}"),
            &format!("frase numero {} qui", number_word(i)),
            true,
        )
        .await;
    }
    let got = collect(&mut guest, 2500).await;
    let spoken = of_type(&got, "enhanced_speak").len();
    assert!(
        (8..=10).contains(&spoken),
        "a burst of 8 plus a refill, not 40: {spoken}"
    );
}

fn number_word(i: usize) -> String {
    // Distinct words, so the text-level echo guard never sees two frames as the same.
    [
        "alfa", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
    ]
    .iter()
    .cycle()
    .skip(i)
    .take(3)
    .copied()
    .collect::<Vec<_>>()
    .join(&format!("{i}"))
}

#[tokio::test]
async fn text_sent_to_a_standard_conversation_is_refused_once_and_never_translated() {
    let srv = srv!(Opts::enhanced());
    let c = convo(
        &srv,
        Make {
            engine: None,
            ..Make::default()
        },
    )
    .await;
    let mut host = open(&srv, &c.host_token).await;
    let mut guest = open(&srv, &c.guests[0].0).await;
    start(&mut host).await;
    for i in 0..3 {
        text(&mut host, &format!("s{i}"), "ciao a tutti voi", true).await;
    }
    let told = wait_for(&mut host, |v| v["type"] == "error").await.unwrap();
    assert_eq!(told["code"], "enhanced_not_active");
    assert!(
        collect(&mut host, 400)
            .await
            .iter()
            .all(|v| v["code"] != "enhanced_not_active"),
        "said once"
    );
    assert!(collect(&mut guest, 200)
        .await
        .iter()
        .all(|v| !is_listener_frame(v)));
    assert_eq!(srv.groq.calls(), 0);
}

#[tokio::test]
async fn when_translation_fails_the_speaker_is_told_once_and_the_server_carries_on() {
    let srv = srv!(Opts::enhanced());
    let c = convo(&srv, Make::default()).await;
    let mut host = open(&srv, &c.host_token).await;
    let mut guest = open(&srv, &c.guests[0].0).await;
    start(&mut host).await;
    srv.groq.fail();
    for i in 0..3 {
        text(
            &mut host,
            &format!("s{i}"),
            &format!("frase {} per tutti voi", number_word(i)),
            true,
        )
        .await;
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    let told = wait_for(&mut host, |v| v["type"] == "error").await.unwrap();
    assert_eq!(told["code"], "translation_unavailable");
    let rest = collect(&mut host, 800).await;
    assert!(
        rest.iter().all(|v| v["code"] != "translation_unavailable"),
        "once per ten seconds: {rest:?}"
    );
    let guest_got = collect(&mut guest, 300).await;
    assert!(
        guest_got.iter().all(|v| v["type"] != "enhanced_speak"),
        "nothing is spoken in the wrong language: {guest_got:?}"
    );
    // The socket is alive: a phone-to-phone ping still answers.
    srv.groq.status.store(0, Ordering::SeqCst);
    text(&mut host, "ok", "e adesso funziona di nuovo", true).await;
    // (the failure notice window is 10 s, so no second notice either way)
    wait_for(&mut guest, |v| v["type"] == "enhanced_speak")
        .await
        .unwrap();
}

// ===== the text-level echo guard ========================================================

#[tokio::test]
async fn what_the_partner_phone_hears_and_transcribes_is_dropped_as_an_echo() {
    let srv = srv!(Opts::enhanced());
    let c = convo(&srv, Make::default()).await;
    let mut host = open(&srv, &c.host_token).await;
    let mut guest = open(&srv, &c.guests[0].0).await;
    start(&mut host).await;
    start(&mut guest).await;

    // The host speaks first (its own STT is ahead)...
    text(&mut host, "h1", "ciao come stai oggi amico", true).await;
    wait_for(&mut guest, |v| v["type"] == "enhanced_speak")
        .await
        .unwrap();
    let before = srv.groq.calls();
    // ...and the guest's phone, next to it, hears the same words and sends them, as a partial and
    // as a final, in the language it was told the guest speaks (garbled a little, as ASR does).
    text(&mut guest, "g1", "ciao come stai", false).await;
    text(&mut guest, "g1", "Ciao, come stai oggi amico?", true).await;
    let host_got = collect(&mut host, 900).await;
    assert!(
        host_got.iter().all(|v| !is_listener_frame(v)),
        "the host was played back its own sentence: {host_got:?}"
    );
    assert_eq!(srv.groq.calls(), before, "an echo is never translated");

    // A genuine reply is not an echo.
    text(&mut guest, "g2", "hola estoy muy bien gracias", true).await;
    let speak = wait_for(&mut host, |v| v["type"] == "enhanced_speak")
        .await
        .unwrap();
    assert_eq!(speak["segment_id"], "g2");

    // Counted in diagnostics, with no text in them.
    let mut found = false;
    for _ in 0..20 {
        let events: Vec<Value> = sqlx::query_scalar(
            "SELECT detail FROM live_session_events WHERE session_id = $1 AND kind = 'diagnostics'",
        )
        .bind(c.session)
        .fetch_all(&srv.pool)
        .await
        .unwrap();
        if events.iter().any(|d| {
            d["echo_filter"]["text"] == true
                && d["echo_filter"]["suppressed"].as_u64().unwrap_or(0) >= 1
        }) {
            let all = serde_json::to_string(&events).unwrap();
            assert!(!all.contains("ciao"), "no text in diagnostics: {all}");
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(found, "the suppression was never reported");
}

#[tokio::test]
async fn a_phone_that_hears_its_own_speech_synthesis_does_not_translate_it_back() {
    let srv = srv!(Opts::enhanced());
    let c = convo(&srv, Make::default()).await;
    let mut host = open(&srv, &c.host_token).await;
    let mut guest = open(&srv, &c.guests[0].0).await;
    start(&mut host).await;
    start(&mut guest).await;
    // The guest speaks; the host's phone is told to speak the Italian translation aloud...
    text(&mut guest, "g1", "hola como estas amigo", true).await;
    let speak = wait_for(&mut host, |v| v["type"] == "enhanced_speak")
        .await
        .unwrap();
    let spoken = speak["text"].as_str().unwrap().to_string();
    let before = srv.groq.calls();
    // ...and the host's own microphone hears that, seconds later, and its STT sends it.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    text(&mut host, "h1", spoken.trim_start_matches("it|"), true).await;
    let guest_got = collect(&mut guest, 700).await;
    assert!(
        guest_got.iter().all(|v| !is_listener_frame(v)),
        "a translation of a translation: {guest_got:?}"
    );
    assert_eq!(srv.groq.calls(), before);
}

#[tokio::test]
async fn the_text_echo_guard_honours_live_echo_filter_off() {
    let srv = srv!(Opts {
        echo_filter: false,
        ..Opts::enhanced()
    });
    let c = convo(&srv, Make::default()).await;
    let mut host = open(&srv, &c.host_token).await;
    let mut guest = open(&srv, &c.guests[0].0).await;
    start(&mut host).await;
    start(&mut guest).await;
    text(&mut host, "h1", "ciao come stai oggi amico", true).await;
    wait_for(&mut guest, |v| v["type"] == "enhanced_speak")
        .await
        .unwrap();
    text(&mut guest, "g1", "ciao come stai oggi amico", true).await;
    wait_for(&mut host, |v| v["type"] == "enhanced_speak")
        .await
        .expect("with the filter off the echo is a message like any other");
}

// ===== billing ==========================================================================

#[tokio::test]
async fn the_owner_pays_the_enhanced_rate_per_distinct_language_for_what_was_actually_sent() {
    let srv = srv!(Opts::enhanced());
    // Host it, guests es + es: ONE target language for the host's words, whatever the headcount.
    let c = convo(
        &srv,
        Make {
            guest_langs: &["es", "es"],
            ..Make::default()
        },
    )
    .await;
    let mut host = open(&srv, &c.host_token).await;
    let mut g1 = open(&srv, &c.guests[0].0).await;
    let mut g2 = open(&srv, &c.guests[1].0).await;
    let start_balance = balance(&srv, c.user).await;
    start(&mut host).await;
    for i in 0..10 {
        text(
            &mut host,
            &format!("s{i}"),
            &format!("frase {} per voi", number_word(i)),
            true,
        )
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    // Drain so the sockets do not back up.
    let _ = collect(&mut g1, 100).await;
    let _ = collect(&mut g2, 100).await;
    let host_ticks = ticks(&srv, c.session, &c.host_peer).await;
    assert!(
        host_ticks.len() >= 3,
        "an opening charge and a tick per second of speech: {host_ticks:?}"
    );
    assert!(
        host_ticks.iter().all(|a| *a == enhanced_unit(1)),
        "one stream at $0.054/min = {} a second: {host_ticks:?}",
        enhanced_unit(1)
    );
    // Seats that never started cost nothing.
    assert!(ticks(&srv, c.session, &c.guests[0].1).await.is_empty());
    // The wallet moved by exactly the ledger.
    let spent: Decimal = host_ticks.iter().sum();
    assert_eq!(balance(&srv, c.user).await, start_balance - spent);

    // Silence is free: nothing is sent, nothing is charged after the opening charge.
    let n = ticks(&srv, c.session, &c.host_peer).await.len();
    tokio::time::sleep(Duration::from_millis(2600)).await;
    assert_eq!(ticks(&srv, c.session, &c.host_peer).await.len(), n);
}

#[tokio::test]
async fn two_target_languages_cost_two_streams_for_every_seat_that_speaks() {
    let srv = srv!(Opts::enhanced());
    // Host it, guests es + en. The host's words need es and en; the Spanish guest's need it and en.
    let c = convo(
        &srv,
        Make {
            guest_langs: &["es", "en"],
            ..Make::default()
        },
    )
    .await;
    let mut host = open(&srv, &c.host_token).await;
    let mut g1 = open(&srv, &c.guests[0].0).await;
    let mut g2 = open(&srv, &c.guests[1].0).await;
    start(&mut host).await;
    start(&mut g1).await;
    for i in 0..8 {
        text(
            &mut host,
            &format!("h{i}"),
            &format!("frase {} per voi", number_word(i)),
            true,
        )
        .await;
        text(
            &mut g1,
            &format!("g{i}"),
            &format!("oracion {} para ustedes", number_word(i + 3)),
            true,
        )
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let _ = collect(&mut g2, 100).await;
    for peer in [&c.host_peer, &c.guests[0].1] {
        let t = ticks(&srv, c.session, peer).await;
        assert!(t.len() >= 3, "{t:?}");
        assert!(
            t.iter().all(|a| *a == enhanced_unit(2)),
            "two streams: {t:?} vs {}",
            enhanced_unit(2)
        );
    }
    // The owner, not the guests, paid all of it, once.
    let all: Vec<Decimal> = sqlx::query_scalar(
        "SELECT amount FROM live_usage_ticks WHERE session_id = $1 AND payer_kind = 'user'",
    )
    .bind(c.session)
    .fetch_all(&srv.pool)
    .await
    .unwrap();
    let total: Decimal = all.iter().sum();
    assert_eq!(balance(&srv, c.user).await, usd(5.0) - total);
}

#[tokio::test]
async fn frames_the_server_dropped_are_never_billed() {
    let srv = srv!(Opts::enhanced());
    let c = convo(&srv, Make::default()).await;
    let mut host = open(&srv, &c.host_token).await;
    let mut guest = open(&srv, &c.guests[0].0).await;
    start(&mut host).await;
    // The opening charge happens at start; let it land, then count.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let opening = ticks(&srv, c.session, &c.host_peer).await.len();
    assert_eq!(opening, 1);
    // Muted, oversized and empty frames, for three ticks' worth of time.
    say(&mut host, json!({ "type": "mute_audio", "muted": true })).await;
    for _ in 0..8 {
        text(&mut host, "m", "parole mentre sono muto", true).await;
        text(&mut host, "o", &"x ".repeat(600), true).await;
        text(&mut host, "e", "", true).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    let _ = collect(&mut guest, 100).await;
    assert_eq!(
        ticks(&srv, c.session, &c.host_peer).await.len(),
        opening,
        "nothing the server dropped was billed"
    );
}

// ===== the whole-session fallback =======================================================

#[tokio::test]
async fn a_phone_that_cannot_reach_cartesia_moves_the_whole_conversation_to_standard() {
    let srv = srv!(Opts::enhanced());
    let c = convo(&srv, Make::default()).await;
    let mut host = open(&srv, &c.host_token).await;
    let mut guest = open(&srv, &c.guests[0].0).await;
    start(&mut host).await;
    start(&mut guest).await;
    // Both are billed at the Enhanced rate while it lasts.
    for i in 0..4 {
        text(
            &mut host,
            &format!("h{i}"),
            &format!("frase {} per voi", number_word(i)),
            true,
        )
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let _ = collect(&mut guest, 100).await;
    let enhanced_rows = |t: &[Decimal]| t.iter().filter(|a| **a == enhanced_unit(1)).count();
    assert!(enhanced_rows(&ticks(&srv, c.session, &c.host_peer).await) >= 1);

    // The GUEST's phone is the one blocked.
    say(
        &mut guest,
        json!({ "type": "enhanced_fallback", "reason": "token_refused" }),
    )
    .await;
    for ws in [&mut host, &mut guest] {
        let d = wait_for(ws, |v| v["type"] == "engine_downgraded")
            .await
            .expect("every phone is told");
        assert_eq!(d["from"], CARTESIA);
        assert_eq!(d["to"], "standard");
        assert_eq!(d["reason"], "enhanced_unavailable");
        assert_eq!(d["peer_id"], c.guests[0].1.as_str());
    }
    // Said once: a second request changes nothing and announces nothing.
    say(
        &mut guest,
        json!({ "type": "enhanced_fallback", "reason": "again" }),
    )
    .await;
    assert!(collect(&mut host, 500)
        .await
        .iter()
        .all(|v| v["type"] != "engine_downgraded"));

    // The stored tier follows, so a reconnect agrees; the diagnostics say why.
    let (_, view) = srv
        .get(&format!("/api/live/sessions/{}", c.session), &c.host_token)
        .await;
    assert_eq!(view["session"]["engine"], "standard");
    let kinds: Vec<(String, Value)> = sqlx::query_as(
        "SELECT kind, detail FROM live_session_events WHERE session_id = $1 AND kind = 'engine_downgraded'",
    )
    .bind(c.session)
    .fetch_all(&srv.pool)
    .await
    .unwrap();
    assert_eq!(kinds.len(), 1);
    assert_eq!(kinds[0].1["reason"], "enhanced_unavailable");
    assert_eq!(kinds[0].1["from"], CARTESIA);

    // Text is no longer a thing, and no token is minted for a Standard conversation.
    text(&mut host, "late", "troppo tardi per il testo", true).await;
    let told = wait_for(&mut host, |v| v["type"] == "error").await.unwrap();
    assert_eq!(told["code"], "enhanced_not_active");
    let (s, b) = srv
        .post(&token_path(c.session), Some(&c.host_token), json!({}))
        .await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::CONFLICT, Some("not_enhanced"))
    );
    assert_eq!(
        srv.groq.calls(),
        4,
        "nothing was translated after the fallback"
    );

    // Give the stale Enhanced meters time to notice, then freeze the count: no more Enhanced
    // charges from now on, for either phone.
    tokio::time::sleep(Duration::from_millis(3500)).await;
    let frozen = (
        enhanced_rows(&ticks(&srv, c.session, &c.host_peer).await),
        enhanced_rows(&ticks(&srv, c.session, &c.guests[0].1).await),
    );

    // The normal Standard path: the client re-starts and streams PCM16.
    say(&mut host, json!({ "type": "start" })).await;
    say(&mut guest, json!({ "type": "start" })).await;
    let mut heard = false;
    for _ in 0..40 {
        let _ = host.send(Message::binary(vec![9u8; 4800])).await;
        let _ = guest.send(Message::binary(vec![9u8; 4800])).await;
        for v in collect(&mut guest, 100).await {
            if v["type"] == "translated_audio" && v["speaker_id"] == c.host_peer.as_str() {
                heard = true;
            }
        }
        if heard {
            break;
        }
    }
    assert!(
        heard,
        "the Standard engine never produced audio for the guest"
    );
    assert!(srv.realtime.saw_type("input_audio_buffer.append"));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let host_ticks = ticks(&srv, c.session, &c.host_peer).await;
    let standard = usd(srv
        .state
        .engines
        .default()
        .metadata()
        .user_rate_per_second()
        * 1.0
        * 1.0);
    assert!(
        host_ticks.contains(&standard),
        "re-metered at the Standard rate {standard}: {host_ticks:?}"
    );
    assert_eq!(
        enhanced_rows(&host_ticks),
        frozen.0,
        "no more Enhanced-rate charges for the host"
    );
    assert_eq!(
        enhanced_rows(&ticks(&srv, c.session, &c.guests[0].1).await),
        frozen.1,
        "nor for the guest"
    );
    assert_ne!(standard, enhanced_unit(1));
}

// ===== persistence and erasure ==========================================================

#[tokio::test]
async fn an_opted_in_enhanced_conversation_is_recorded_by_speaker_and_erased_with_its_owner_request(
) {
    let srv = srv!(Opts::enhanced());
    let c = convo(
        &srv,
        Make {
            save_transcript: true,
            ..Make::default()
        },
    )
    .await;
    let mut host = open(&srv, &c.host_token).await;
    let mut guest = open(&srv, &c.guests[0].0).await;
    start(&mut host).await;
    start(&mut guest).await;
    // Partials are not stored, finals are, one row each.
    text(&mut host, "h1", "ciao a", false).await;
    text(&mut host, "h1", "ciao a tutti voi", true).await;
    wait_for(&mut guest, |v| v["type"] == "enhanced_speak")
        .await
        .unwrap();
    text(&mut guest, "g1", "hola estoy aqui con ustedes", true).await;
    wait_for(&mut host, |v| v["type"] == "enhanced_speak")
        .await
        .unwrap();

    let (call, kind): (Uuid, String) = sqlx::query_as(
        "SELECT cs.id, cs.kind FROM live_sessions ls JOIN call_sessions cs ON cs.id = ls.call_session_id WHERE ls.id = $1",
    )
    .bind(c.session)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(kind, "live_connect");
    let mut rows: Vec<(String, String, String, String, Value)> = Vec::new();
    for _ in 0..30 {
        rows = sqlx::query_as(
            "SELECT speaker_peer_id, speaker_name, original_text, original_lang, translations
             FROM transcript_events WHERE session_id = $1 AND event_type = 'speech' ORDER BY ts",
        )
        .bind(call)
        .fetch_all(&srv.pool)
        .await
        .unwrap();
        if rows.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(
        rows.len(),
        2,
        "one row per final, none for the partial: {rows:?}"
    );
    assert_eq!(rows[0].0, c.host_peer);
    assert_eq!(rows[0].1, "Marco");
    assert_eq!(
        rows[0].2, "ciao a tutti voi",
        "the original as sent, never a translation"
    );
    assert_eq!(rows[0].3, "it");
    assert_eq!(rows[0].4["es"], "es|ciao a tutti voi");
    assert_eq!(rows[0].4["it"], "ciao a tutti voi");
    assert_eq!(rows[1].0, c.guests[0].1, "speakers stay apart");
    assert_eq!(rows[1].1, "Guest0");
    assert_eq!(rows[1].3, "es");
    assert_eq!(rows[1].4["it"], "it|hola estoy aqui con ustedes");

    // The owner ends it and asks for its content to be deleted: every row goes.
    let (s, _) = srv
        .post(
            &format!("/api/live/sessions/{}/end", c.session),
            Some(&c.host_token),
            json!({}),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, b) = srv
        .call(
            "DELETE",
            &format!("/api/live/sessions/{}", c.session),
            Some(&c.jwt),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM transcript_events WHERE session_id = $1")
            .bind(call)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(left, 0);
    // No voice material was ever stored with the conversation.
    let cols: Vec<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns WHERE table_name = 'live_participants'",
    )
    .fetch_all(&srv.pool)
    .await
    .unwrap();
    assert!(cols.iter().all(|c| !c.contains("voice")), "{cols:?}");
}

#[tokio::test]
async fn an_unsaved_enhanced_conversation_stores_nothing() {
    let srv = srv!(Opts::enhanced());
    let c = convo(&srv, Make::default()).await;
    let mut host = open(&srv, &c.host_token).await;
    let mut guest = open(&srv, &c.guests[0].0).await;
    start(&mut host).await;
    text(&mut host, "h1", "ciao a tutti voi", true).await;
    wait_for(&mut guest, |v| v["type"] == "enhanced_speak")
        .await
        .unwrap();
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM transcript_events te JOIN live_sessions ls ON ls.call_session_id = te.session_id WHERE ls.id = $1",
    )
    .bind(c.session)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(events, 0);
}

// ===== the rest keeps working ============================================================

#[tokio::test]
async fn a_standard_conversation_and_its_sockets_are_unchanged_when_enhanced_exists() {
    let srv = srv!(Opts::enhanced());
    let c = convo(
        &srv,
        Make {
            engine: None,
            ..Make::default()
        },
    )
    .await;
    assert_eq!(c.created["engine"], "standard");
    assert_eq!(c.created["voice_clone"], false);
    let mut host = open(&srv, &c.host_token).await;
    let mut guest = open(&srv, &c.guests[0].0).await;
    say(&mut host, json!({ "type": "start" })).await;
    say(&mut guest, json!({ "type": "start" })).await;
    let mut heard = false;
    for _ in 0..40 {
        let _ = host.send(Message::binary(vec![9u8; 4800])).await;
        let _ = guest.send(Message::binary(vec![9u8; 4800])).await;
        for v in collect(&mut guest, 100).await {
            if v["type"] == "translated_audio" {
                heard = true;
            }
        }
        if heard {
            break;
        }
    }
    assert!(heard, "Standard translated audio still flows");
    // And its meter is the Standard rate.
    let t = ticks(&srv, c.session, &c.host_peer).await;
    let standard = usd(srv
        .state
        .engines
        .default()
        .metadata()
        .user_rate_per_second());
    assert!(!t.is_empty() && t.iter().all(|a| *a == standard), "{t:?}");
}
