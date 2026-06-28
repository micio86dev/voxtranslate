//! Integration tests for the BUSINESS scheduled-meetings endpoints
//! (business/meetings.rs, org-scoped): role enforcement, create input validation +
//! the "connect Google Calendar" guard, update guards (not-editable / empty title),
//! and the DB-backed list/get/cancel (rows seeded directly). DB-gated; skipped without
//! `DATABASE_URL`. The create/update happy paths (Google Calendar) aren't exercised.

use std::net::SocketAddr;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use reqwest::Client;
use serde_json::{json, Value};
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::config::Config;
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::{app, db, AppState};

const SECRET: &str = "biz-meetings-secret";

struct Server {
    addr: SocketAddr,
    pool: db::Pool,
}

fn base(srv: &Server) -> String {
    format!("http://{}", srv.addr)
}

async fn setup() -> Option<Server> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let config = Config::test_with_billing(&url, SECRET, 0.0);
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

async fn user(srv: &Server) -> (Uuid, String) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: "Org Scheduler".into(),
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

async fn make_org(srv: &Server, owner: Uuid) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id) VALUES ('Meet Co', $1, $2) RETURNING id",
    )
    .bind(format!("meet-{}", Uuid::new_v4().simple()))
    .bind(owner)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(id)
    .bind(owner)
    .execute(&srv.pool)
    .await
    .unwrap();
    id
}

async fn seed_meeting(
    srv: &Server,
    org: Uuid,
    creator: Uuid,
    status: &str,
    at: DateTime<Utc>,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO scheduled_meetings
            (id, creator_user_id, org_id, title, scheduled_at, end_at, room_code, join_url, status)
         VALUES ($1,$2,$3,'Sync',$4,$5,$6,$7,$8)",
    )
    .bind(id)
    .bind(creator)
    .bind(org)
    .bind(at)
    .bind(at + Duration::hours(1))
    .bind(format!("room-{}", Uuid::new_v4().simple()))
    .bind("https://app.test/?room=x")
    .bind(status)
    .execute(&srv.pool)
    .await
    .unwrap();
    id
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
async fn create_role_validation_and_google_guard() {
    let srv = skip_without_db!(setup().await);
    let (uid, jwt) = user(&srv).await;
    let org = make_org(&srv, uid).await;
    let at = (Utc::now() + Duration::days(1)).to_rfc3339();
    let url = format!("{}/api/business/organizations/{}/meetings", base(&srv), org);

    // Empty title → 400.
    let r = Client::new()
        .post(&url)
        .bearer_auth(&jwt)
        .json(&json!({ "title": "  ", "scheduled_at": at }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    // end before start → 400.
    let r = Client::new()
        .post(&url)
        .bearer_auth(&jwt)
        .json(&json!({ "title": "X", "scheduled_at": at, "end_at": Utc::now().to_rfc3339() }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    // Valid input, no Google connection → 409.
    let r = Client::new()
        .post(&url)
        .bearer_auth(&jwt)
        .json(&json!({ "title": "X", "scheduled_at": at }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);

    // A non-member of the org is rejected (404/403 — org existence isn't leaked).
    let (_stranger, other_jwt) = user(&srv).await;
    let r = Client::new()
        .post(&url)
        .bearer_auth(&other_jwt)
        .json(&json!({ "title": "X", "scheduled_at": at }))
        .send()
        .await
        .unwrap();
    assert!(r.status() == 404 || r.status() == 403, "got {}", r.status());
}

#[tokio::test]
async fn list_get_in_org() {
    let srv = skip_without_db!(setup().await);
    let (uid, jwt) = user(&srv).await;
    let org = make_org(&srv, uid).await;
    let id = seed_meeting(&srv, org, uid, "scheduled", Utc::now() + Duration::days(1)).await;
    seed_meeting(&srv, org, uid, "scheduled", Utc::now() + Duration::days(2)).await;

    let v: Value = Client::new()
        .get(format!(
            "{}/api/business/organizations/{}/meetings",
            base(&srv),
            org
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v.as_array().unwrap().len(), 2);

    let r = Client::new()
        .get(format!(
            "{}/api/business/organizations/{}/meetings/{}",
            base(&srv),
            org,
            id
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["id"], id.to_string());

    let r = Client::new()
        .get(format!(
            "{}/api/business/organizations/{}/meetings/{}",
            base(&srv),
            org,
            Uuid::new_v4()
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn update_guards_not_editable_and_empty_title() {
    let srv = skip_without_db!(setup().await);
    let (uid, jwt) = user(&srv).await;
    let org = make_org(&srv, uid).await;
    let at = (Utc::now() + Duration::days(1)).to_rfc3339();

    // A cancelled meeting is not editable → 409.
    let cancelled = seed_meeting(&srv, org, uid, "cancelled", Utc::now() + Duration::days(1)).await;
    let r = Client::new()
        .patch(format!(
            "{}/api/business/organizations/{}/meetings/{}",
            base(&srv),
            org,
            cancelled
        ))
        .bearer_auth(&jwt)
        .json(&json!({ "title": "New", "scheduled_at": at }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);

    // A scheduled meeting with an empty title → 400.
    let scheduled = seed_meeting(&srv, org, uid, "scheduled", Utc::now() + Duration::days(1)).await;
    let r = Client::new()
        .patch(format!(
            "{}/api/business/organizations/{}/meetings/{}",
            base(&srv),
            org,
            scheduled
        ))
        .bearer_auth(&jwt)
        .json(&json!({ "title": "  ", "scheduled_at": at }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn cancel_marks_cancelled_or_404() {
    let srv = skip_without_db!(setup().await);
    let (uid, jwt) = user(&srv).await;
    let org = make_org(&srv, uid).await;
    let id = seed_meeting(&srv, org, uid, "scheduled", Utc::now() + Duration::days(1)).await;

    let r = Client::new()
        .post(format!(
            "{}/api/business/organizations/{}/meetings/{}/cancel",
            base(&srv),
            org,
            id
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let status: String = sqlx::query_scalar("SELECT status FROM scheduled_meetings WHERE id = $1")
        .bind(id)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert_eq!(status, "cancelled");

    let r = Client::new()
        .post(format!(
            "{}/api/business/organizations/{}/meetings/{}/cancel",
            base(&srv),
            org,
            Uuid::new_v4()
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn meetings_require_auth() {
    let srv = skip_without_db!(setup().await);
    let (uid, _) = user(&srv).await;
    let org = make_org(&srv, uid).await;
    let r = Client::new()
        .get(format!(
            "{}/api/business/organizations/{}/meetings",
            base(&srv),
            org
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}
