//! Integration tests for the notification REST endpoints (spec: scheduled meetings,
//! Phase 1d): push subscribe/unsubscribe, the VAPID public-key probe, the
//! preference matrix (get/patch + quiet hours), and the in-app center
//! (list/mark-read/mark-all-read). DB-gated — skipped without `DATABASE_URL`.
//! No real web-push is sent (only the DB-backed handlers are exercised).

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::Client;
use serde_json::{json, Value};
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::config::{Config, PushConfig};
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::{app, db, AppState};

const SECRET: &str = "notif-test-secret";

struct Server {
    addr: SocketAddr,
    pool: db::Pool,
}

fn base(srv: &Server) -> String {
    format!("http://{}", srv.addr)
}

async fn setup(with_push: bool) -> Option<Server> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let mut config = Config::test_with_billing(&url, SECRET, 0.0);
    config.push = with_push.then(|| PushConfig {
        vapid_public_key: "BTestVapidPublicKey".into(),
        vapid_private_key: "test-private".into(),
        vapid_subject: "mailto:test@voxtranslate.app".into(),
    });
    let mut state = AppState::new(config);
    state.safety = Some(SafetyService::new(pool.clone()));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    Some(Server { addr, pool })
}

/// Create a signed-in user, returning (id, jwt).
async fn user(srv: &Server) -> (Uuid, String) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: "Notif Tester".into(),
        avatar_url: None,
    };
    let u = upsert_google_user(
        &srv.pool,
        &identity,
        rust_decimal::Decimal::ZERO,
        None,
        None,
    )
    .await
    .unwrap();
    let jwt = issue_jwt(SECRET, &u.id, &u.email, &u.name, 168).unwrap();
    (u.id, jwt)
}

macro_rules! skip_without_db {
    ($e:expr) => {
        match $e {
            Some(s) => s,
            None => {
                eprintln!("skipping — no DATABASE_URL");
                return;
            }
        }
    };
}

#[tokio::test]
async fn vapid_public_key_present_and_absent() {
    let srv = skip_without_db!(setup(true).await);
    let r = Client::new()
        .get(format!("{}/api/push/vapid-public-key", base(&srv)))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["key"], "BTestVapidPublicKey");

    // Without push configured the probe is a 503 (feature dormant).
    let srv2 = skip_without_db!(setup(false).await);
    let r2 = Client::new()
        .get(format!("{}/api/push/vapid-public-key", base(&srv2)))
        .send()
        .await
        .unwrap();
    assert_eq!(r2.status(), 503);
}

#[tokio::test]
async fn subscribe_then_unsubscribe_roundtrip() {
    let srv = skip_without_db!(setup(true).await);
    let (uid, jwt) = user(&srv).await;
    let body = json!({
        "endpoint": "https://push.example/abc",
        "keys": { "p256dh": "p256-key", "auth": "auth-key" },
        "user_agent": "test-agent"
    });
    let r = Client::new()
        .post(format!("{}/api/push/subscribe", base(&srv)))
        .bearer_auth(&jwt)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let n: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM user_push_subscriptions WHERE user_id = $1")
            .bind(uid)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(n, 1);

    // Re-subscribing the same endpoint upserts (still one row).
    let r = Client::new()
        .post(format!("{}/api/push/subscribe", base(&srv)))
        .bearer_auth(&jwt)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let n: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM user_push_subscriptions WHERE user_id = $1")
            .bind(uid)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(n, 1);

    let r = Client::new()
        .delete(format!("{}/api/push/subscribe", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "endpoint": "https://push.example/abc" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let n: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM user_push_subscriptions WHERE user_id = $1")
            .bind(uid)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn preferences_default_then_patched() {
    let srv = skip_without_db!(setup(true).await);
    let (_uid, jwt) = user(&srv).await;
    // Defaults: no overrides → every channel ON; timezone UTC.
    let r = Client::new()
        .get(format!("{}/api/notifications/preferences", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["timezone"], "UTC");
    let prefs = v["preferences"].as_array().unwrap();
    assert!(prefs.iter().all(|p| p["enabled"] == true));

    // Patch one toggle off + set quiet hours + timezone.
    let r = Client::new()
        .patch(format!("{}/api/notifications/preferences", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({
            "preferences": [{ "type": "meeting_invited", "channel": "push", "enabled": false }],
            "quiet_hours_start": 22,
            "quiet_hours_end": 7,
            "timezone": "Europe/Rome"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    let v: Value = Client::new()
        .get(format!("{}/api/notifications/preferences", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["timezone"], "Europe/Rome");
    assert_eq!(v["quiet_hours_start"], 22);
    assert_eq!(v["quiet_hours_end"], 7);
    let off = v["preferences"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["type"] == "meeting_invited" && p["channel"] == "push")
        .unwrap();
    assert_eq!(off["enabled"], false);

    // An invalid type/channel is ignored (still 204, no row written).
    let r = Client::new()
        .patch(format!("{}/api/notifications/preferences", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "preferences": [{ "type": "bogus", "channel": "push", "enabled": false }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
}

#[tokio::test]
async fn list_mark_read_and_mark_all() {
    let srv = skip_without_db!(setup(true).await);
    let (uid, jwt) = user(&srv).await;

    // Empty to start.
    let v: Value = Client::new()
        .get(format!("{}/api/notifications", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["unread"], 0);
    assert_eq!(v["notifications"].as_array().unwrap().len(), 0);

    // Seed two unread notifications directly.
    let id1: Uuid = sqlx::query_scalar(
        "INSERT INTO notifications (user_id, type, title, body) VALUES ($1,'meeting_invited','A','b1') RETURNING id",
    )
    .bind(uid)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO notifications (user_id, type, title, body) VALUES ($1,'meeting_reminder','B','b2')")
        .bind(uid)
        .execute(&srv.pool)
        .await
        .unwrap();

    let v: Value = Client::new()
        .get(format!("{}/api/notifications?unread=true", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["unread"], 2);
    assert_eq!(v["notifications"].as_array().unwrap().len(), 2);

    // Mark one read → unread drops to 1.
    let r = Client::new()
        .post(format!("{}/api/notifications/{}/read", base(&srv), id1))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let v: Value = Client::new()
        .get(format!("{}/api/notifications", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["unread"], 1);

    // Mark all read → unread 0.
    let r = Client::new()
        .post(format!("{}/api/notifications/read-all", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let v: Value = Client::new()
        .get(format!("{}/api/notifications", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["unread"], 0);
}

#[tokio::test]
async fn endpoints_require_auth() {
    let srv = skip_without_db!(setup(true).await);
    // No bearer token → 401 on an auth-gated endpoint.
    let r = Client::new()
        .get(format!("{}/api/notifications", base(&srv)))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}
