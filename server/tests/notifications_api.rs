//! Notifications: the bell, the preferences that silence it, and the push
//! subscription behind it.
//!
//! `notify` is the fan-out every feature calls, so it is driven directly here
//! rather than through whichever endpoint happens to trigger it. What is asserted
//! is mostly the muting: a user who turned a channel off, and a user who is asleep,
//! must not be reached.
//!
//! DB-gated: skipped without `DATABASE_URL`.

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::Client;
use serde_json::{json, Value};
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::{Config, PushConfig};
use voxtranslate_server::db::{self, Pool};
use voxtranslate_server::notifications::{self, CHANNELS, TYPES};
use voxtranslate_server::{app, AppState};

const SECRET: &str = "notifications-secret";

struct Server {
    addr: SocketAddr,
    pool: Pool,
    state: AppState,
}

/// `push = false` reproduces a deployment with no VAPID keys.
async fn setup_with(push: bool) -> Option<Server> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let mut config = Config::test_with_billing(&url, SECRET, 0.0);
    if push {
        config.push = Some(PushConfig {
            vapid_public_key: "BExamplePublicKeyForTests".into(),
            vapid_private_key: "example-private-key".into(),
            vapid_subject: "mailto:ops@example.com".into(),
        });
    }
    let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);
    let mut state = AppState::new(config);
    state.billing = Some(BillingService::new(pool.clone(), min_join));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);
    let state_for_tests = state.clone();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    Some(Server {
        addr,
        pool,
        state: state_for_tests,
    })
}

async fn setup() -> Option<Server> {
    setup_with(true).await
}

fn base(srv: &Server) -> String {
    format!("http://{}", srv.addr)
}

async fn user(srv: &Server) -> (Uuid, String) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: "Notified".into(),
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
    let jwt = issue_jwt(SECRET, &u.id, &u.email, &u.name, 168).unwrap();
    (u.id, jwt)
}

async fn in_app_rows(srv: &Server, user_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM notifications WHERE user_id = $1")
        .bind(user_id)
        .fetch_one(&srv.pool)
        .await
        .unwrap()
}

macro_rules! skip_without_db {
    ($setup:expr) => {
        match $setup {
            Some(srv) => srv,
            None => {
                eprintln!("skipping — no DATABASE_URL");
                return;
            }
        }
    };
}

// ---------------------------------------------------------------------------
// The fan-out
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_notification_lands_in_the_bell() {
    let srv = skip_without_db!(setup().await);
    let (user_id, _) = user(&srv).await;

    notifications::notify(
        &srv.state,
        user_id,
        "friend_request",
        "en",
        "Ada wants to connect",
        "Accept or ignore.",
        json!({}),
    )
    .await;

    assert_eq!(in_app_rows(&srv, user_id).await, 1);
}

#[tokio::test]
async fn a_user_who_turned_the_bell_off_is_not_written_to() {
    let srv = skip_without_db!(setup().await);
    let (user_id, _) = user(&srv).await;
    sqlx::query(
        "INSERT INTO notification_preferences (user_id, type, channel, enabled)
         VALUES ($1, 'friend_request', 'in_app', false)",
    )
    .bind(user_id)
    .execute(&srv.pool)
    .await
    .unwrap();

    notifications::notify(
        &srv.state,
        user_id,
        "friend_request",
        "en",
        "Ada wants to connect",
        "Accept or ignore.",
        json!({}),
    )
    .await;

    assert_eq!(in_app_rows(&srv, user_id).await, 0);
}

#[tokio::test]
async fn muting_one_type_does_not_mute_the_others() {
    let srv = skip_without_db!(setup().await);
    let (user_id, _) = user(&srv).await;
    sqlx::query(
        "INSERT INTO notification_preferences (user_id, type, channel, enabled)
         VALUES ($1, 'friend_request', 'in_app', false)",
    )
    .bind(user_id)
    .execute(&srv.pool)
    .await
    .unwrap();

    notifications::notify(
        &srv.state,
        user_id,
        "meeting_reminder",
        "en",
        "Standup in 10 minutes",
        "Join now.",
        json!({ "join_url": "https://app.voxtranslate.app/?room=x" }),
    )
    .await;

    assert_eq!(in_app_rows(&srv, user_id).await, 1);
}

#[tokio::test]
async fn quiet_hours_never_silence_the_bell_itself() {
    let srv = skip_without_db!(setup().await);
    let (user_id, _) = user(&srv).await;
    // A window covering the whole day: whatever hour the test runs in, it is inside.
    sqlx::query(
        "INSERT INTO notification_settings (user_id, quiet_hours_start, quiet_hours_end, timezone)
         VALUES ($1, 0, 24, 'UTC')",
    )
    .bind(user_id)
    .execute(&srv.pool)
    .await
    .unwrap();

    notifications::notify(
        &srv.state,
        user_id,
        "meeting_reminder",
        "en",
        "Standup",
        "Join now.",
        json!({}),
    )
    .await;

    // Quiet hours stop what interrupts you, not what waits for you.
    assert_eq!(in_app_rows(&srv, user_id).await, 1);
}

#[tokio::test]
async fn a_notification_for_a_user_who_no_longer_exists_is_a_no_op() {
    let srv = skip_without_db!(setup().await);

    notifications::notify(
        &srv.state,
        Uuid::new_v4(),
        "friend_request",
        "en",
        "Hello",
        "Body",
        json!({}),
    )
    .await;
    // No panic, nothing written — the fan-out is called from background tasks
    // where a deleted account must not take the task down.
}

// ---------------------------------------------------------------------------
// Reading the bell
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_list_reports_what_is_unread() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (user_id, jwt) = user(&srv).await;
    for i in 0..3 {
        notifications::notify(
            &srv.state,
            user_id,
            "friend_request",
            "en",
            &format!("Request {i}"),
            "Body",
            json!({}),
        )
        .await;
    }

    let body: Value = http
        .get(format!("{}/api/notifications", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(body["notifications"].as_array().unwrap().len(), 3);
    assert_eq!(body["unread"], 3);
}

#[tokio::test]
async fn the_list_needs_a_signed_in_caller() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();

    let r = http
        .get(format!("{}/api/notifications", base(&srv)))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn the_list_clamps_an_absurd_page_size() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (_, jwt) = user(&srv).await;

    for query in ["?limit=0", "?limit=100000", "?unread=true"] {
        let r = http
            .get(format!("{}/api/notifications{query}", base(&srv)))
            .bearer_auth(&jwt)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{query} should be clamped, not refused");
    }
}

#[tokio::test]
async fn one_notification_can_be_marked_read() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (user_id, jwt) = user(&srv).await;
    notifications::notify(
        &srv.state,
        user_id,
        "friend_request",
        "en",
        "Request",
        "Body",
        json!({}),
    )
    .await;
    let id: Uuid = sqlx::query_scalar("SELECT id FROM notifications WHERE user_id = $1")
        .bind(user_id)
        .fetch_one(&srv.pool)
        .await
        .unwrap();

    let r = http
        .post(format!("{}/api/notifications/{id}/read", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "got {}", r.status());

    let unread: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notifications WHERE user_id = $1 AND read_at IS NULL",
    )
    .bind(user_id)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(unread, 0);
}

#[tokio::test]
async fn someone_elses_notification_cannot_be_marked_read() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, _) = user(&srv).await;
    let (_, outsider) = user(&srv).await;
    notifications::notify(
        &srv.state,
        owner,
        "friend_request",
        "en",
        "Request",
        "Body",
        json!({}),
    )
    .await;
    let id: Uuid = sqlx::query_scalar("SELECT id FROM notifications WHERE user_id = $1")
        .bind(owner)
        .fetch_one(&srv.pool)
        .await
        .unwrap();

    http.post(format!("{}/api/notifications/{id}/read", base(&srv)))
        .bearer_auth(&outsider)
        .send()
        .await
        .unwrap();

    let still_unread: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notifications WHERE id = $1 AND read_at IS NULL",
    )
    .bind(id)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(still_unread, 1, "a stranger must not clear your bell");
}

#[tokio::test]
async fn the_whole_bell_can_be_cleared_at_once() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (user_id, jwt) = user(&srv).await;
    for i in 0..4 {
        notifications::notify(
            &srv.state,
            user_id,
            "friend_request",
            "en",
            &format!("Request {i}"),
            "Body",
            json!({}),
        )
        .await;
    }

    let r = http
        .post(format!("{}/api/notifications/read-all", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "got {}", r.status());

    let unread: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notifications WHERE user_id = $1 AND read_at IS NULL",
    )
    .bind(user_id)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(unread, 0);
}

// ---------------------------------------------------------------------------
// Preferences
// ---------------------------------------------------------------------------

#[tokio::test]
async fn preferences_round_trip() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (_, jwt) = user(&srv).await;
    let url = format!("{}/api/notifications/preferences", base(&srv));

    let patch = http
        .patch(&url)
        .bearer_auth(&jwt)
        .json(&json!({
            "preferences": [
                { "type": "friend_request", "channel": "email", "enabled": false },
            ],
            "quiet_hours_start": 22,
            "quiet_hours_end": 7,
            "timezone": "Europe/Rome",
        }))
        .send()
        .await
        .unwrap();
    assert!(patch.status().is_success(), "got {}", patch.status());

    let got: Value = http
        .get(&url)
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["quiet_hours_start"], 22);
    assert_eq!(got["quiet_hours_end"], 7);
    assert_eq!(got["timezone"], "Europe/Rome");
}

#[tokio::test]
async fn a_preference_naming_something_that_does_not_exist_is_dropped_not_stored() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (user_id, jwt) = user(&srv).await;
    let url = format!("{}/api/notifications/preferences", base(&srv));

    // An older client sending a type this build no longer has must not have its
    // whole patch refused, so unknown entries are skipped rather than rejected —
    // but nothing unknown may reach the table either.
    let r = http
        .patch(&url)
        .bearer_auth(&jwt)
        .json(&json!({
            "preferences": [
                { "type": "friend_request", "channel": "carrier_pigeon", "enabled": false },
                { "type": "telepathy", "channel": "email", "enabled": false },
                { "type": "friend_request", "channel": "email", "enabled": false },
            ],
        }))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "got {}", r.status());

    let stored: Vec<(String, String)> = sqlx::query_as(
        "SELECT type, channel FROM notification_preferences WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_all(&srv.pool)
    .await
    .unwrap();
    assert_eq!(
        stored,
        vec![("friend_request".to_string(), "email".to_string())],
        "only the entry this build knows about is kept"
    );
}

#[tokio::test]
async fn every_declared_type_and_channel_is_accepted() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (_, jwt) = user(&srv).await;
    let url = format!("{}/api/notifications/preferences", base(&srv));

    // The two constant lists are the contract the dashboard renders from; a value
    // in them that the endpoint refuses would be a checkbox that cannot be ticked.
    let prefs: Vec<Value> = TYPES
        .iter()
        .flat_map(|t| {
            CHANNELS
                .iter()
                .map(move |c| json!({ "type": t, "channel": c, "enabled": true }))
        })
        .collect();

    let r = http
        .patch(&url)
        .bearer_auth(&jwt)
        .json(&json!({ "preferences": prefs }))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "got {}", r.status());
}

#[tokio::test]
async fn preferences_need_a_signed_in_caller() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let url = format!("{}/api/notifications/preferences", base(&srv));

    assert_eq!(http.get(&url).send().await.unwrap().status(), 401);
    assert_eq!(
        http.patch(&url)
            .json(&json!({ "preferences": [] }))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
}

// ---------------------------------------------------------------------------
// Push subscriptions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_browser_is_given_the_key_it_needs_to_subscribe() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();

    let r = http
        .get(format!("{}/api/push/vapid-public-key", base(&srv)))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["key"], "BExamplePublicKeyForTests");
}

#[tokio::test]
async fn without_vapid_keys_push_is_reported_unavailable() {
    let srv = skip_without_db!(setup_with(false).await);
    let http = Client::new();

    let r = http
        .get(format!("{}/api/push/vapid-public-key", base(&srv)))
        .send()
        .await
        .unwrap();
    assert!(!r.status().is_success(), "got {}", r.status());
}

#[tokio::test]
async fn a_subscription_is_stored_once_per_endpoint() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (user_id, jwt) = user(&srv).await;
    let url = format!("{}/api/push/subscribe", base(&srv));
    let body = json!({
        "endpoint": "https://push.example/abc",
        "keys": { "p256dh": "key", "auth": "auth" },
        "user_agent": "Firefox",
    });

    assert_eq!(
        http.post(&url)
            .bearer_auth(&jwt)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    // A browser re-subscribing with rotated keys updates the row rather than
    // adding a second one, or every push would be sent twice.
    let refreshed = json!({
        "endpoint": "https://push.example/abc",
        "keys": { "p256dh": "new-key", "auth": "new-auth" },
    });
    http.post(&url)
        .bearer_auth(&jwt)
        .json(&refreshed)
        .send()
        .await
        .unwrap();

    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT p256dh FROM user_push_subscriptions WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_all(&srv.pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "new-key");
}

#[tokio::test]
async fn a_subscription_can_be_withdrawn() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (user_id, jwt) = user(&srv).await;
    let endpoint = "https://push.example/withdraw-me";
    http.post(format!("{}/api/push/subscribe", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({
            "endpoint": endpoint,
            "keys": { "p256dh": "key", "auth": "auth" },
        }))
        .send()
        .await
        .unwrap();

    let r = http
        .delete(format!("{}/api/push/subscribe", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "endpoint": endpoint }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM user_push_subscriptions WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(left, 0);
}

#[tokio::test]
async fn subscribing_needs_a_signed_in_caller() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();

    let r = http
        .post(format!("{}/api/push/subscribe", base(&srv)))
        .json(&json!({
            "endpoint": "https://push.example/abc",
            "keys": { "p256dh": "k", "auth": "a" },
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}
