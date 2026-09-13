//! Integration tests for the Dashboard Help Assistant WebSocket handler.
//!
//! Tests that don't need a live DB or OpenAI are unconditional.
//! Handler/DB tests are `#[ignore]`d — run them with:
//!   cargo test --test help_assistant_integration -- --ignored --nocapture
//!
//! The handler tests reference `business::help_assistant::ws_handler` which is
//! implemented in task 1.10. Until then some tests here compile but the handler
//! ones are exercised only through the Axum test client.

use voxtranslate_server::business::credits::MinuteRateMeter;
use voxtranslate_server::config::HelpAssistantConfig;
use voxtranslate_server::engine::help_assistant::{capacity_full_error, try_acquire};

use std::sync::Arc;
use tokio::sync::Semaphore;

// ---------------------------------------------------------------------------
// Config helper
// ---------------------------------------------------------------------------

fn test_cfg(cost_per_minute: f64, markup: f64, max_sessions: usize) -> HelpAssistantConfig {
    HelpAssistantConfig {
        api_key: "test-key".into(),
        model: "gpt-realtime-2.1".into(),
        cost_per_minute,
        markup,
        max_sessions,
        realtime_base_url: voxtranslate_server::config::OPENAI_DEFAULT_REALTIME_BASE.into(),
    }
}

// ---------------------------------------------------------------------------
// Unit tests — no DB, no network
// ---------------------------------------------------------------------------

/// Any member role must be accepted (help assistant has NO role restriction).
/// This verifies the design decision: role_rank("member") = MEMBER = 1 is enough.
#[test]
fn any_role_rank_satisfies_help_assistant_gate() {
    use voxtranslate_server::business::{role_rank, MEMBER};
    // member rank (the lowest authenticated role) must be ≥ MEMBER.
    assert!(
        role_rank("member") >= MEMBER,
        "member should satisfy the help assistant gate (no role restriction)"
    );
    // admin and owner also pass.
    assert!(role_rank("admin") >= MEMBER);
    assert!(role_rank("owner") >= MEMBER);
    // unknown role does NOT pass.
    assert!(role_rank("guest") < MEMBER);
}

/// Semaphore full → try_acquire returns None.
#[tokio::test]
async fn semaphore_full_try_acquire_returns_none() {
    let sem = Arc::new(Semaphore::new(1));
    // Acquire the only permit so the semaphore is exhausted.
    let _permit = sem.clone().try_acquire_owned().unwrap();
    let result = try_acquire(&sem).await;
    assert!(
        result.is_none(),
        "try_acquire on a full semaphore must return None"
    );
}

/// capacity_full_error JSON has the expected shape.
#[test]
fn capacity_full_error_json_shape() {
    let json: serde_json::Value = serde_json::from_str(&capacity_full_error()).expect("valid JSON");
    assert_eq!(json["type"], "error");
    assert_eq!(json["code"], "capacity_full");
    assert!(
        json["message"].as_str().is_some_and(|m| !m.is_empty()),
        "capacity_full error must have a non-empty message"
    );
}

/// Semaphore with capacity=2: first two acquires succeed, third fails.
#[tokio::test]
async fn semaphore_cap_is_enforced() {
    let sem = Arc::new(Semaphore::new(2));
    let p1 = try_acquire(&sem).await;
    let p2 = try_acquire(&sem).await;
    let p3 = try_acquire(&sem).await;
    assert!(p1.is_some(), "first acquire should succeed");
    assert!(p2.is_some(), "second acquire should succeed");
    assert!(p3.is_none(), "third acquire on cap=2 semaphore should fail");
}

/// A minute costs `cost_per_minute × (1 + markup)`, at 100 credits = $1.
/// Default params: $0.18 × 1.25 = $0.225 = 22.5 credits, so 22 are charged and
/// the half is carried rather than rounded up.
#[test]
fn credits_formula_default() {
    let cfg = test_cfg(0.18, 0.25, 10);
    let mut m = MinuteRateMeter::new(cfg.cost_per_minute, cfg.markup);
    assert_eq!(m.due(60), 22);
    assert_eq!(m.due(120), 23, "the carried half arrives with minute two");
    assert_eq!(m.charged(), 45);
}

/// Triangulation: $0.15 × 1.25 = $0.1875 = 18.75 credits a minute.
#[test]
fn credits_formula_triangulation() {
    let cfg = test_cfg(0.15, 0.25, 10);
    let mut m = MinuteRateMeter::new(cfg.cost_per_minute, cfg.markup);
    assert_eq!(m.due(60), 18);
    assert_eq!(m.due(240), 57, "4 × 18.75 = 75 credits over four minutes");
    assert_eq!(m.charged(), 75);
}

// ---------------------------------------------------------------------------
// DB integration tests (ignored by default — need local PG on :5433)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// DB integration — the eligibility gate is reported IN-BAND
// ---------------------------------------------------------------------------
//
// Regression: these two refusals used to be a pre-upgrade HTTP 402. A browser
// cannot read the status of a failed WebSocket handshake — JS gets a bare
// `error` event — so a lapsed subscription reached the dashboard as
// "Errore di connessione. Riprova." and the user had no idea what to fix. The
// upgrade must therefore SUCCEED and carry the reason as the first frame.

use std::net::SocketAddr;
use std::time::Duration;

use futures::StreamExt as _;
use serde_json::Value;
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::config::Config;
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::{app, db, AppState};

const HA_SECRET: &str = "help-assistant-gate-secret";

struct GateServer {
    addr: SocketAddr,
    pool: db::Pool,
}

/// Boot a server with the help assistant ENABLED (the route is only registered
/// when its config is present) against the local test database.
async fn gate_setup() -> Option<GateServer> {
    gate_setup_against(None).await
}

/// Like [`gate_setup`], with the relay's upstream pointed at a stand-in so the
/// session itself runs rather than dying at OpenAI.
async fn gate_setup_against(upstream: Option<String>) -> Option<GateServer> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let mut config = Config::test_with_billing(&url, HA_SECRET, 0.0);
    let mut ha = test_cfg(0.18, 0.25, 10);
    if let Some(base) = upstream {
        ha.realtime_base_url = base;
    }
    config.help_assistant = Some(ha);
    let mut state = AppState::new(config);
    state.safety = Some(SafetyService::new(pool.clone()));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    Some(GateServer { addr, pool })
}

/// A member of a brand-new org (subscription_status defaults to 'none').
async fn member_of_fresh_org(srv: &GateServer) -> (Uuid, String) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: "Helper".into(),
        avatar_url: None,
    };
    let (u, _) = upsert_google_user(
        &srv.pool,
        &identity,
        rust_decimal::Decimal::ZERO,
        None,
        None,
    )
    .await
    .unwrap();
    let org: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id) VALUES ('Help Co', $1, $2) RETURNING id",
    )
    .bind(format!("help-{}", Uuid::new_v4().simple()))
    .bind(u.id)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(org)
    .bind(u.id)
    .execute(&srv.pool)
    .await
    .unwrap();
    let jwt = issue_jwt(HA_SECRET, &u.id, &u.email, &u.name, 168).unwrap();
    (org, jwt)
}

/// Open the help-assistant socket and return its first text frame, parsed.
/// A panic here means the upgrade itself was refused — which is the bug.
async fn first_frame(srv: &GateServer, org: Uuid, jwt: &str) -> Value {
    let url = format!(
        "ws://{}/api/business/organizations/{org}/help-assistant?token={jwt}",
        srv.addr
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("the upgrade must succeed — the reason travels in-band, not as a status code");
    loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
            Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t)))) => {
                return serde_json::from_str(t.as_str()).expect("frame is JSON");
            }
            Ok(Some(Ok(_))) => continue,
            other => panic!("no text frame before close: {other:?}"),
        }
    }
}

#[tokio::test]
async fn lapsed_subscription_is_explained_in_band_not_as_a_dead_socket() {
    let Some(srv) = gate_setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let (org, jwt) = member_of_fresh_org(&srv).await;

    // A subscription that Stripe never cancelled but whose paid period has ended
    // — exactly the gifted-subscription shape, which no webhook ever flips to
    // 'canceled'. The row still says 'active'; the date says otherwise.
    sqlx::query(
        "UPDATE organizations
            SET subscription_status = 'active',
                current_period_end = now() - interval '8 days',
                credits_balance = 500
          WHERE id = $1",
    )
    .bind(org)
    .execute(&srv.pool)
    .await
    .unwrap();

    let frame = first_frame(&srv, org, &jwt).await;
    assert_eq!(frame["type"], "error");
    assert_eq!(frame["code"], "subscription_required");
    // Without an action the dashboard can only show text; with it, a button.
    assert_eq!(frame["action"], "purchase_subscription");
}

#[tokio::test]
async fn an_empty_credit_pool_is_explained_in_band_too() {
    let Some(srv) = gate_setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let (org, jwt) = member_of_fresh_org(&srv).await;

    // Subscription genuinely live, but the pool can't fund a session.
    sqlx::query(
        "UPDATE organizations
            SET subscription_status = 'active',
                current_period_end = now() + interval '30 days',
                credits_balance = 3
          WHERE id = $1",
    )
    .bind(org)
    .execute(&srv.pool)
    .await
    .unwrap();

    let frame = first_frame(&srv, org, &jwt).await;
    assert_eq!(frame["type"], "error");
    assert_eq!(frame["code"], "insufficient_credits");
    assert_eq!(frame["action"], "purchase_credits");
    assert_eq!(frame["balance"], 3);
    assert_eq!(frame["required"], 10);
}

/// Membership is still a pre-upgrade HTTP refusal: a non-member has no business
/// holding a socket open, and nothing about that is the user's to fix.
#[tokio::test]
async fn a_non_member_is_still_refused_before_the_upgrade() {
    let Some(srv) = gate_setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let (org, _owner_jwt) = member_of_fresh_org(&srv).await;
    let (_other_org, outsider_jwt) = member_of_fresh_org(&srv).await;

    let url = format!(
        "ws://{}/api/business/organizations/{org}/help-assistant?token={outsider_jwt}",
        srv.addr
    );
    assert!(
        tokio_tungstenite::connect_async(url).await.is_err(),
        "an outsider must not get a socket at all"
    );
}

// ---------------------------------------------------------------------------
// The relay itself, against a stand-in provider
// ---------------------------------------------------------------------------

use voxtranslate_server::engine::realtime_mock::{Dialect, RealtimeMock, Reply};

/// An eligible org on a server whose upstream is the stand-in.
async fn relay_ready(mock: &RealtimeMock) -> Option<(GateServer, Uuid, String)> {
    let srv = gate_setup_against(Some(mock.base_url())).await?;
    let (org, jwt) = member_of_fresh_org(&srv).await;
    sqlx::query(
        "UPDATE organizations
            SET subscription_status = 'active',
                current_period_end = now() + interval '30 days',
                credits_balance = 5000
          WHERE id = $1",
    )
    .bind(org)
    .execute(&srv.pool)
    .await
    .unwrap();
    Some((srv, org, jwt))
}

/// Open the help-assistant socket, optionally send audio, and read until `wanted`.
async fn relay_frame(
    srv: &GateServer,
    org: Uuid,
    jwt: &str,
    audio: Option<Vec<u8>>,
    wanted: impl Fn(&Value) -> bool,
) -> Option<Value> {
    use futures::SinkExt as _;
    let url = format!(
        "ws://{}/api/business/organizations/{org}/help-assistant?token={jwt}",
        srv.addr
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.ok()?;
    if let Some(pcm) = audio {
        let _ = ws
            .send(tokio_tungstenite::tungstenite::Message::binary(pcm))
            .await;
    }
    while let Ok(Some(Ok(frame))) = tokio::time::timeout(Duration::from_secs(6), ws.next()).await {
        if let tokio_tungstenite::tungstenite::Message::Text(t) = frame {
            if let Ok(v) = serde_json::from_str::<Value>(t.as_str()) {
                if wanted(&v) {
                    return Some(v);
                }
            }
        }
    }
    None
}

#[tokio::test]
async fn the_relay_configures_its_upstream_session_first() {
    let mock = RealtimeMock::start(Dialect::OpenAiAssistant).await;
    let Some((srv, org, jwt)) = relay_ready(&mock).await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };

    let url = format!(
        "ws://{}/api/business/organizations/{org}/help-assistant?token={jwt}",
        srv.addr
    );
    let _ = tokio_tungstenite::connect_async(url).await;
    tokio::time::sleep(Duration::from_millis(600)).await;

    let frames = mock.received();
    assert!(
        !frames.is_empty(),
        "the relay never opened an upstream session"
    );
    assert!(
        frames[0].contains("session.update"),
        "the first frame configures the session: {}",
        frames[0]
    );
}

#[tokio::test]
async fn the_users_audio_is_forwarded_upstream() {
    let mock = RealtimeMock::start(Dialect::OpenAiAssistant).await;
    let Some((srv, org, jwt)) = relay_ready(&mock).await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };

    let _ = relay_frame(&srv, org, &jwt, Some(vec![0u8; 640]), |v| {
        v["type"] == "nothing-matches-this"
    })
    .await;

    assert!(
        mock.saw_type("input_audio_buffer.append"),
        "the microphone must reach the model: {:?}",
        mock.received()
    );
}

#[tokio::test]
async fn the_answer_reaches_the_dashboard_as_text_and_voice() {
    let mock = RealtimeMock::start(Dialect::OpenAiAssistant).await;
    mock.reply_with(vec![
        Reply::Transcript {
            original: "where do I change the plan".into(),
            translated: "Under Billing.".into(),
        },
        Reply::Audio(vec![1, 2, 3, 4]),
    ]);
    let Some((srv, org, jwt)) = relay_ready(&mock).await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };

    assert!(
        relay_frame(&srv, org, &jwt, Some(vec![0u8; 640]), |v| v["type"]
            == "transcript")
        .await
        .is_some(),
        "the answer must be readable, not only audible"
    );
    assert!(
        relay_frame(&srv, org, &jwt, Some(vec![0u8; 640]), |v| v["type"]
            == "answer_audio")
        .await
        .is_some(),
        "and audible"
    );
}

#[tokio::test]
async fn an_upstream_error_is_said_out_loud() {
    let mock = RealtimeMock::start(Dialect::OpenAiAssistant).await;
    mock.reply_with(vec![Reply::Error("model unavailable".into())]);
    let Some((srv, org, jwt)) = relay_ready(&mock).await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };

    // A socket that opens and then goes quiet reaches the dashboard as an
    // unexplained "connection error" — the whole reason this relay reports in-band.
    assert!(
        relay_frame(&srv, org, &jwt, Some(vec![0u8; 640]), |v| v["type"]
            == "error")
        .await
        .is_some()
    );
}
