//! The Enhanced tier's two server endpoints, with Cartesia standing in.
//!
//! This tier runs the provider IN THE BROWSER, so the server's whole job is to hand
//! out a scoped, short-lived token and to clone a voice — and in both cases the raw
//! `CARTESIA_API_KEY` must never leave the building. `CartesiaConfig::api_base`
//! points those two calls at a stand-in, so what is asserted here is the gating,
//! what the client is actually told, and what happens when Cartesia is down.
//!
//! DB-gated: skipped without `DATABASE_URL`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use reqwest::multipart::{Form, Part};
use reqwest::Client;
use serde_json::{json, Value};
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::{CartesiaConfig, Config};
use voxtranslate_server::db::{self, Pool};
use voxtranslate_server::{app, AppState};

const SECRET: &str = "cartesia-enhanced-secret";
const SERVER_KEY: &str = "sk_car_server_side_only";

// ---------------------------------------------------------------------------
// A stand-in for Cartesia
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Cartesia {
    /// Every `Authorization` header Cartesia was presented with.
    auth_seen: Arc<Mutex<Vec<String>>>,
    clones: Arc<AtomicUsize>,
    /// Non-zero makes every call fail with this status.
    fail: Arc<AtomicU16>,
    /// Return a token response with no `token` field.
    malformed: Arc<AtomicUsize>,
}

async fn access_token(
    State(c): State<Cartesia>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    c.auth_seen.lock().unwrap().push(
        headers
            .get("authorization")
            .map(|v| v.to_str().unwrap_or_default().to_string())
            .unwrap_or_default(),
    );
    let code = c.fail.load(Ordering::SeqCst);
    if code != 0 {
        return (StatusCode::from_u16(code).unwrap(), "nope").into_response();
    }
    if c.malformed.load(Ordering::SeqCst) != 0 {
        return Json(json!({ "not_a_token": true })).into_response();
    }
    // Echo the grants back so the test can see what the server asked for.
    Json(json!({ "token": "scoped-token", "grants": body["grants"] })).into_response()
}

async fn clone_voice(State(c): State<Cartesia>, headers: HeaderMap) -> axum::response::Response {
    c.clones.fetch_add(1, Ordering::SeqCst);
    c.auth_seen.lock().unwrap().push(
        headers
            .get("authorization")
            .map(|v| v.to_str().unwrap_or_default().to_string())
            .unwrap_or_default(),
    );
    let code = c.fail.load(Ordering::SeqCst);
    if code != 0 {
        return (StatusCode::from_u16(code).unwrap(), "nope").into_response();
    }
    Json(json!({ "id": "voice-abc123" })).into_response()
}

async fn mock_cartesia() -> (String, Cartesia) {
    let c = Cartesia::default();
    let router = Router::new()
        .route("/access-token", post(access_token))
        .route("/voices/clone", post(clone_voice))
        .with_state(c.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{addr}"), c)
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Server {
    addr: SocketAddr,
    pool: Pool,
    cartesia: Cartesia,
}

fn base(srv: &Server) -> String {
    format!("http://{}", srv.addr)
}

/// `enabled = false` reproduces a deployment with `CARTESIA_ENHANCED` off.
async fn setup_with(enabled: bool, cloning: bool) -> Option<Server> {
    let url = voxtranslate_server::db::test_database_url()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let (api_base, cartesia) = mock_cartesia().await;
    let mut config = Config::test_with_billing(&url, SECRET, 5.0);
    if enabled {
        config.cartesia = Some(CartesiaConfig {
            api_key: SERVER_KEY.into(),
            stt_model: "ink-whisper".into(),
            stt_model_by_lang: HashMap::from([("en".to_string(), "ink-2".to_string())]),
            tts_model: "sonic-3.5".into(),
            cost_per_minute: 0.02,
            markup: 0.85,
            voice_cloning_enabled: cloning,
            default_voice_id: Some("fallback-voice".into()),
            api_base,
            stt_endpoint: "wss://cartesia.test/stt".into(),
            tts_endpoint: "wss://cartesia.test/tts".into(),
            version: "2025-04-16".into(),
        });
    }
    let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);
    let mut state = AppState::new(config);
    state.billing = Some(BillingService::new(pool.clone(), min_join));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    Some(Server {
        addr,
        pool,
        cartesia,
    })
}

macro_rules! srv {
    () => {
        srv!(true, true)
    };
    ($enabled:expr, $cloning:expr) => {
        match setup_with($enabled, $cloning).await {
            Some(s) => s,
            None => {
                eprintln!("skipping — no DATABASE_URL");
                return;
            }
        }
    };
}

/// A signed-in user with `credits` dollars of balance.
async fn user(srv: &Server, credits: i64) -> (Uuid, String) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: "Enhanced listener".into(),
        avatar_url: None,
    };
    let (u, _) = upsert_google_user(
        &srv.pool,
        &identity,
        rust_decimal::Decimal::new(credits * 100, 2),
        None,
        None,
    )
    .await
    .unwrap();
    let jwt = issue_jwt(SECRET, &u.id, &u.email, &u.name, 168).unwrap();
    (u.id, jwt)
}

fn clip_form() -> Form {
    Form::new()
        .part(
            "clip",
            Part::bytes(vec![0u8; 2048])
                .file_name("voice.webm")
                .mime_str("audio/webm")
                .unwrap(),
        )
        .text("language", "it")
}

// ---------------------------------------------------------------------------
// The language catalogue
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_language_catalogue_is_public_and_cacheable() {
    let srv = srv!();
    let http = Client::new();

    let r = http
        .get(format!("{}/api/languages", base(&srv)))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    // It never changes between deploys and every client asks for it on load, so
    // serving it uncached would be pure waste.
    let cc = r
        .headers()
        .get("cache-control")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(cc.contains("max-age=3600"), "{cc}");
    assert!(cc.contains("stale-while-revalidate"), "{cc}");

    let body: Value = r.json().await.unwrap();
    for key in ["languages", "regions", "tiers"] {
        assert!(
            body[key].as_array().is_some_and(|a| !a.is_empty())
                || body[key].as_object().is_some_and(|o| !o.is_empty()),
            "{key} was empty"
        );
    }
}

// ---------------------------------------------------------------------------
// Minting a session token
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_guest_gets_no_token_at_all() {
    let srv = srv!();
    let http = Client::new();

    // Guests are pinned to Standard server-side, so there is no Enhanced session
    // for them to open and no paid token to mint.
    let r = http
        .post(format!("{}/api/sessions/enhanced/session", base(&srv)))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    assert_eq!(srv.cartesia.auth_seen.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn the_tier_being_switched_off_is_a_503_not_a_crash() {
    let srv = srv!(false, false);
    let http = Client::new();
    let (_uid, jwt) = user(&srv, 5).await;

    let r = http
        .post(format!("{}/api/sessions/enhanced/session", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
}

#[tokio::test]
async fn the_browser_receives_a_scoped_token_and_never_the_server_key() {
    let srv = srv!();
    let http = Client::new();
    let (_uid, jwt) = user(&srv, 5).await;

    let r = http
        .post(format!("{}/api/sessions/enhanced/session", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();

    assert_eq!(body["token"], "scoped-token");
    // The whole point of minting: the raw key is what Cartesia sees, and the
    // browser sees only what it needs to open its own socket.
    let raw = serde_json::to_string(&body).unwrap();
    assert!(
        !raw.contains(SERVER_KEY),
        "the server key reached the client"
    );
    assert_eq!(
        srv.cartesia.auth_seen.lock().unwrap()[0],
        format!("Bearer {SERVER_KEY}")
    );

    // An expiry the client can refresh against, not a token it has to guess about.
    assert!(body["expires_at"].as_i64().unwrap() > chrono::Utc::now().timestamp());
    assert_eq!(body["stt"]["endpoint"], "wss://cartesia.test/stt");
    assert_eq!(body["stt"]["models_by_lang"]["en"], "ink-2");
    assert_eq!(body["tts"]["model"], "sonic-3.5");
    assert_eq!(body["default_voice_id"], "fallback-voice");
}

#[tokio::test]
async fn a_listener_who_cannot_afford_the_call_is_not_given_a_token() {
    let srv = srv!();
    let http = Client::new();
    let (_uid, jwt) = user(&srv, 0).await;

    let r = http
        .post(format!("{}/api/sessions/enhanced/session", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();

    // Minting a token they cannot use just costs us a Cartesia call and shows them
    // a broken session instead of a purchase prompt.
    assert!(r.status().is_client_error(), "got {}", r.status());
    assert_eq!(
        srv.cartesia.auth_seen.lock().unwrap().len(),
        0,
        "Cartesia was called for a listener with no credit"
    );
}

#[tokio::test]
async fn cartesia_being_down_is_a_bad_gateway_not_a_500() {
    let srv = srv!();
    let http = Client::new();
    let (_uid, jwt) = user(&srv, 5).await;
    srv.cartesia.fail.store(500, Ordering::SeqCst);

    let r = http
        .post(format!("{}/api/sessions/enhanced/session", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 502, "an upstream failure was reported as ours");
}

#[tokio::test]
async fn a_token_response_with_no_token_in_it_is_treated_as_a_failure() {
    let srv = srv!();
    let http = Client::new();
    let (_uid, jwt) = user(&srv, 5).await;
    srv.cartesia.malformed.store(1, Ordering::SeqCst);

    let r = http
        .post(format!("{}/api/sessions/enhanced/session", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    // Handing the browser `null` would fail later, inside a WebSocket handshake,
    // where there is nothing left to report.
    assert_eq!(r.status(), 502);
}

// ---------------------------------------------------------------------------
// Instant Voice Cloning
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_cloned_voice_is_returned_and_remembered() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv, 5).await;

    let r = http
        .post(format!("{}/api/sessions/enhanced/clone-voice", base(&srv)))
        .bearer_auth(&jwt)
        .multipart(clip_form())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["voice_id"], "voice-abc123");

    // Stored on the user, so the NEXT call does not ask them to record again.
    let stored: Option<String> =
        sqlx::query_scalar("SELECT cartesia_voice_id FROM users WHERE id = $1")
            .bind(uid)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(stored.as_deref(), Some("voice-abc123"));
}

#[tokio::test]
async fn a_clone_with_no_clip_is_refused_before_cartesia_is_called() {
    let srv = srv!();
    let http = Client::new();
    let (_uid, jwt) = user(&srv, 5).await;

    let r = http
        .post(format!("{}/api/sessions/enhanced/clone-voice", base(&srv)))
        .bearer_auth(&jwt)
        .multipart(Form::new().text("language", "en"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(srv.cartesia.clones.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cloning_switched_off_is_a_503() {
    let srv = srv!(true, false);
    let http = Client::new();
    let (_uid, jwt) = user(&srv, 5).await;

    let r = http
        .post(format!("{}/api/sessions/enhanced/clone-voice", base(&srv)))
        .bearer_auth(&jwt)
        .multipart(clip_form())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
    assert_eq!(srv.cartesia.clones.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_failed_clone_degrades_to_a_default_voice_instead_of_blocking_the_call() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv, 5).await;
    srv.cartesia.fail.store(502, Ordering::SeqCst);

    let r = http
        .post(format!("{}/api/sessions/enhanced/clone-voice", base(&srv)))
        .bearer_auth(&jwt)
        .multipart(clip_form())
        .send()
        .await
        .unwrap();

    // 200 with `fallback: true`, deliberately: a voice that does not sound like you
    // is a small loss, and a call you cannot join is a total one.
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["voice_id"].is_null());
    assert_eq!(body["fallback"], true);

    let stored: Option<String> =
        sqlx::query_scalar("SELECT cartesia_voice_id FROM users WHERE id = $1")
            .bind(uid)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert!(stored.is_none(), "a failed clone was stored anyway");
}

#[tokio::test]
async fn a_guest_cannot_clone_a_voice() {
    let srv = srv!();
    let http = Client::new();

    let r = http
        .post(format!("{}/api/sessions/enhanced/clone-voice", base(&srv)))
        .multipart(clip_form())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    assert_eq!(srv.cartesia.clones.load(Ordering::SeqCst), 0);
}
