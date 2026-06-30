//! Integration tests for the Directus backoffice admin API (`/api/admin/*`),
//! authenticated by the shared `ADMIN_API_SECRET` (server-to-server, not a JWT).
//! DB-gated — skipped without `DATABASE_URL`. Covers the auth gate plus
//! ban/unban/credit/bonus/delete-user, org gift-subscription, and report-resolve,
//! including the validation + not-found branches. No provider calls (Resend is
//! unset, so the bonus email is a no-op).

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::Client;
use serde_json::{json, Value};
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::Config;
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::{app, db, AppState};

const SECRET: &str = "admin-jwt-secret";
const ADMIN: &str = "test-admin-secret"; // matches Config::test_with_billing

struct Server {
    addr: SocketAddr,
    pool: db::Pool,
}

async fn setup() -> Option<Server> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let config = Config::test_with_billing(&url, SECRET, 0.0);
    let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);
    let mut state = AppState::new(config);
    state.billing = Some(BillingService::new(pool.clone(), min_join));
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

async fn make_user(srv: &Server) -> (Uuid, String) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: "Admin Target".into(),
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

fn base(srv: &Server) -> String {
    format!("http://{}", srv.addr)
}

/// POST to an admin endpoint with the shared secret.
async fn admin_post(http: &Client, srv: &Server, path: &str, body: Value) -> reqwest::Response {
    http.post(format!("{}{}", base(srv), path))
        .header("X-Admin-Secret", ADMIN)
        .json(&body)
        .send()
        .await
        .unwrap()
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
async fn admin_auth_gate() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();

    // No secret → 403.
    let none = http
        .post(format!("{}/api/admin/ban", base(&srv)))
        .json(&json!({ "user_id": Uuid::new_v4(), "reason": "x" }))
        .send()
        .await
        .unwrap();
    assert_eq!(none.status(), 403);

    // Wrong secret → 403.
    let wrong = http
        .post(format!("{}/api/admin/ban", base(&srv)))
        .header("X-Admin-Secret", "nope")
        .json(&json!({ "user_id": Uuid::new_v4(), "reason": "x" }))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 403);

    // Correct secret via the Bearer form is accepted (reaches the handler).
    let (uid, _) = make_user(&srv).await;
    let ok = http
        .post(format!("{}/api/admin/ban", base(&srv)))
        .bearer_auth(ADMIN)
        .json(&json!({ "user_id": uid, "reason": "spam" }))
        .send()
        .await
        .unwrap();
    assert!(
        ok.status().is_success(),
        "bearer admin auth: {}",
        ok.status()
    );
}

#[tokio::test]
async fn ban_unban_credit_bonus_delete() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (uid, _) = make_user(&srv).await;

    // ban → unban.
    assert!(admin_post(
        &http,
        &srv,
        "/api/admin/ban",
        json!({ "user_id": uid, "days": 7, "reason": "abuse" })
    )
    .await
    .status()
    .is_success());
    assert!(
        admin_post(&http, &srv, "/api/admin/unban", json!({ "user_id": uid }))
            .await
            .status()
            .is_success()
    );

    // credit (an adjustment with a reason).
    assert!(admin_post(
        &http,
        &srv,
        "/api/admin/credit",
        json!({ "user_id": uid, "amount": 1.5, "reason": "goodwill" })
    )
    .await
    .status()
    .is_success());

    // bonus: positive ok; non-positive rejected.
    assert!(admin_post(
        &http,
        &srv,
        "/api/admin/bonus",
        json!({ "user_id": uid, "amount": 2.0 })
    )
    .await
    .status()
    .is_success());
    let bad_bonus = admin_post(
        &http,
        &srv,
        "/api/admin/bonus",
        json!({ "user_id": uid, "amount": -1.0 }),
    )
    .await;
    assert_eq!(bad_bonus.status(), 400);

    // balance reflects credit + bonus (1.5 + 2.0 = 3.5).
    let bal: rust_decimal::Decimal = sqlx::query_scalar("SELECT balance FROM users WHERE id = $1")
        .bind(uid)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert!(
        bal >= rust_decimal::Decimal::new(35, 1),
        "balance was {bal}"
    );

    // delete the user (GDPR erase).
    assert!(admin_post(
        &http,
        &srv,
        "/api/admin/user/delete",
        json!({ "user_id": uid })
    )
    .await
    .status()
    .is_success());
    let exists: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE id = $1")
        .bind(uid)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert_eq!(exists, 0, "user erased");
}

#[tokio::test]
async fn gift_subscription_paths() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = make_user(&srv).await;
    let _ = owner;

    // Create a real org via the API (owner = creator).
    let r = http
        .post(format!("{}/api/business/organizations", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "name": "Gift Co", "slug": format!("org-{}", Uuid::new_v4().simple()) }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let org: Value = r.json().await.unwrap();
    let org_id = org["id"].as_str().unwrap();

    // Valid gift (defaults credits to the plan package × months).
    let ok = admin_post(
        &http,
        &srv,
        "/api/admin/org/gift-subscription",
        json!({ "org_id": org_id, "plan": "business", "months": 2 }),
    )
    .await;
    assert!(ok.status().is_success(), "gift: {}", ok.status());

    // Invalid plan → 400.
    let bad_plan = admin_post(
        &http,
        &srv,
        "/api/admin/org/gift-subscription",
        json!({ "org_id": org_id, "plan": "platinum" }),
    )
    .await;
    assert_eq!(bad_plan.status(), 400);

    // Months out of range → 400.
    let bad_months = admin_post(
        &http,
        &srv,
        "/api/admin/org/gift-subscription",
        json!({ "org_id": org_id, "plan": "enterprise", "months": 99 }),
    )
    .await;
    assert_eq!(bad_months.status(), 400);

    // Unknown org → 404.
    let ghost = admin_post(
        &http,
        &srv,
        "/api/admin/org/gift-subscription",
        json!({ "org_id": Uuid::new_v4(), "plan": "business" }),
    )
    .await;
    assert_eq!(ghost.status(), 404);
}

#[tokio::test]
async fn resolve_report_validation() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();

    // Invalid action → 400.
    let bad = admin_post(
        &http,
        &srv,
        "/api/admin/report/resolve",
        json!({ "report_id": Uuid::new_v4(), "action": "ignore" }),
    )
    .await;
    assert_eq!(bad.status(), 400);

    // Valid action but unknown report → 404.
    let missing = admin_post(
        &http,
        &srv,
        "/api/admin/report/resolve",
        json!({ "report_id": Uuid::new_v4(), "action": "dismissed" }),
    )
    .await;
    assert_eq!(missing.status(), 404);
}
