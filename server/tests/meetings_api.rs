//! Integration tests for the consumer scheduled-meetings endpoints (meetings.rs):
//! create input validation + the "connect Google Calendar" guard, and the DB-backed
//! list/get/cancel (rows seeded directly, so no Google API is needed). DB-gated;
//! skipped without `DATABASE_URL`. The create happy path (which calls Google Calendar)
//! is intentionally not exercised here.

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

const SECRET: &str = "meetings-api-secret";

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
        name: "Scheduler".into(),
        avatar_url: None,
    };
    let u = upsert_google_user(&srv.pool, &identity, rust_decimal::Decimal::ZERO, None, None)
        .await
        .unwrap();
    let jwt = issue_jwt(SECRET, &u.id, &u.email, &u.name, 168).unwrap();
    (u.id, jwt)
}

/// Seed a consumer scheduled_meeting (no org), returning its id.
async fn seed_meeting(srv: &Server, creator: Uuid, at: DateTime<Utc>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO scheduled_meetings
            (id, creator_user_id, title, scheduled_at, end_at, room_code, join_url)
         VALUES ($1,$2,'Standup',$3,$4,$5,$6)",
    )
    .bind(id)
    .bind(creator)
    .bind(at)
    .bind(at + Duration::hours(1))
    .bind(format!("room-{}", Uuid::new_v4().simple()))
    .bind("https://app.test/?room=x")
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
async fn create_validates_input_then_requires_google() {
    let srv = skip_without_db!(setup().await);
    let (_uid, jwt) = user(&srv).await;
    let at = (Utc::now() + Duration::days(1)).to_rfc3339();
    let post = |body: Value, jwt: String, b: String| async move {
        Client::new()
            .post(format!("{b}/api/meetings"))
            .bearer_auth(jwt)
            .json(&body)
            .send()
            .await
            .unwrap()
    };

    // Empty title → 400.
    let r = post(json!({ "title": "  ", "scheduled_at": at }), jwt.clone(), base(&srv)).await;
    assert_eq!(r.status(), 400);

    // end before start → 400.
    let r = post(
        json!({ "title": "X", "scheduled_at": at, "end_at": (Utc::now()).to_rfc3339() }),
        jwt.clone(),
        base(&srv),
    )
    .await;
    assert_eq!(r.status(), 400);

    // Invalid invitee email → 400.
    let r = post(
        json!({ "title": "X", "scheduled_at": at, "invitee_emails": ["not-an-email"] }),
        jwt.clone(),
        base(&srv),
    )
    .await;
    assert_eq!(r.status(), 400);

    // Valid input but no Google Calendar connection → 409 (connect your calendar).
    let r = post(json!({ "title": "X", "scheduled_at": at }), jwt, base(&srv)).await;
    assert_eq!(r.status(), 409);
}

#[tokio::test]
async fn list_returns_user_meetings_in_window() {
    let srv = skip_without_db!(setup().await);
    let (uid, jwt) = user(&srv).await;
    seed_meeting(&srv, uid, Utc::now() + Duration::days(1)).await;
    seed_meeting(&srv, uid, Utc::now() + Duration::days(2)).await;
    // Another user's meeting must NOT appear.
    let (other, _) = user(&srv).await;
    seed_meeting(&srv, other, Utc::now() + Duration::days(1)).await;

    let r = Client::new()
        .get(format!("{}/api/meetings", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let rows: Value = r.json().await.unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn get_returns_detail_or_404() {
    let srv = skip_without_db!(setup().await);
    let (uid, jwt) = user(&srv).await;
    let id = seed_meeting(&srv, uid, Utc::now() + Duration::days(1)).await;

    let r = Client::new()
        .get(format!("{}/api/meetings/{}", base(&srv), id))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    // MeetingDetail flattens the meeting fields to the top level + an `invitees` array.
    assert_eq!(v["id"], id.to_string());
    assert_eq!(v["invitees"].as_array().unwrap().len(), 0);

    // Unknown id → 404.
    let r = Client::new()
        .get(format!("{}/api/meetings/{}", base(&srv), Uuid::new_v4()))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn cancel_marks_cancelled_or_404() {
    let srv = skip_without_db!(setup().await);
    let (uid, jwt) = user(&srv).await;
    let id = seed_meeting(&srv, uid, Utc::now() + Duration::days(1)).await;

    let r = Client::new()
        .post(format!("{}/api/meetings/{}/cancel", base(&srv), id))
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

    // Cancelling someone else's / unknown meeting → 404.
    let r = Client::new()
        .post(format!("{}/api/meetings/{}/cancel", base(&srv), Uuid::new_v4()))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn meetings_require_auth() {
    let srv = skip_without_db!(setup().await);
    let r = Client::new()
        .get(format!("{}/api/meetings", base(&srv)))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}
