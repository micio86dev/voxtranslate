//! Integration tests for the project API EDGE cases not hit by the happy-path
//! suite: name validation, 404s for unknown ids, and the idempotent
//! archive-then-404 delete. DB-gated — skipped without `DATABASE_URL`.

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

const SECRET: &str = "projects-secret";

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

async fn user(srv: &Server) -> String {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: "Proj Owner".into(),
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
    issue_jwt(SECRET, &u.id, &u.email, &u.name, 168).unwrap()
}

fn base(srv: &Server) -> String {
    format!("http://{}", srv.addr)
}

async fn create_org(http: &Client, srv: &Server, jwt: &str) -> Uuid {
    let r = http
        .post(format!("{}/api/business/organizations", base(srv)))
        .bearer_auth(jwt)
        .json(&json!({ "name": "Proj Co", "slug": format!("org-{}", Uuid::new_v4().simple()) }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
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
async fn create_rejects_blank_and_overlong_names() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let jwt = user(&srv).await;
    let org = create_org(&http, &srv, &jwt).await;
    let url = format!("{}/api/business/organizations/{}/projects", base(&srv), org);

    let blank = http
        .post(&url)
        .bearer_auth(&jwt)
        .json(&json!({ "name": "   " }))
        .send()
        .await
        .unwrap();
    assert_eq!(blank.status(), 400);

    let overlong = http
        .post(&url)
        .bearer_auth(&jwt)
        .json(&json!({ "name": "x".repeat(200) }))
        .send()
        .await
        .unwrap();
    assert_eq!(overlong.status(), 400);
}

#[tokio::test]
async fn get_patch_delete_unknown_project_is_404() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let jwt = user(&srv).await;
    let org = create_org(&http, &srv, &jwt).await;
    let ghost = Uuid::new_v4();
    let p = format!(
        "{}/api/business/organizations/{}/projects/{}",
        base(&srv),
        org,
        ghost
    );

    let get = http.get(&p).bearer_auth(&jwt).send().await.unwrap();
    assert_eq!(get.status(), 404);

    let patch = http
        .patch(&p)
        .bearer_auth(&jwt)
        .json(&json!({ "description": "x" }))
        .send()
        .await
        .unwrap();
    assert_eq!(patch.status(), 404);

    let del = http.delete(&p).bearer_auth(&jwt).send().await.unwrap();
    assert_eq!(del.status(), 404);
}

#[tokio::test]
async fn patch_rejects_blank_name() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let jwt = user(&srv).await;
    let org = create_org(&http, &srv, &jwt).await;

    // Create a real project to patch.
    let created: Value = http
        .post(format!(
            "{}/api/business/organizations/{}/projects",
            base(&srv),
            org
        ))
        .bearer_auth(&jwt)
        .json(&json!({ "name": "Real" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pid = created["id"].as_str().unwrap();

    let bad = http
        .patch(format!(
            "{}/api/business/organizations/{}/projects/{}",
            base(&srv),
            org,
            pid
        ))
        .bearer_auth(&jwt)
        .json(&json!({ "name": "   " }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
}

#[tokio::test]
async fn delete_is_idempotent_then_404() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let jwt = user(&srv).await;
    let org = create_org(&http, &srv, &jwt).await;

    let created: Value = http
        .post(format!(
            "{}/api/business/organizations/{}/projects",
            base(&srv),
            org
        ))
        .bearer_auth(&jwt)
        .json(&json!({ "name": "Doomed" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pid = created["id"].as_str().unwrap();
    let url = format!(
        "{}/api/business/organizations/{}/projects/{}",
        base(&srv),
        org,
        pid
    );

    // First delete archives it (2xx); a second delete finds nothing live → 404.
    let first = http.delete(&url).bearer_auth(&jwt).send().await.unwrap();
    assert!(
        first.status().is_success(),
        "first delete: {}",
        first.status()
    );
    let second = http.delete(&url).bearer_auth(&jwt).send().await.unwrap();
    assert_eq!(second.status(), 404);
}
