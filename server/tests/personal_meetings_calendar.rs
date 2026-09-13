//! `POST /api/meetings` — the consumer-side scheduled meeting, with Google Calendar
//! standing in.
//!
//! This is the org-free twin of `business_meetings_calendar.rs`: one signed-in
//! person, no roles, no projects. The shape is deliberately the same — Calendar is
//! written first and the row only exists if that succeeded — so the two paths do
//! not drift into behaving differently for the same user action.
//!
//! DB-gated: skipped without `DATABASE_URL`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{patch, post};
use axum::{Json, Router};
use chrono::{Duration, Utc};
use reqwest::Client;
use serde_json::{json, Value};
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::config::Config;
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::{app, db, AppState};

const SECRET: &str = "personal-meetings-secret";

#[derive(Clone, Default)]
struct Calendar {
    creates: Arc<Mutex<Vec<Value>>>,
    deletes: Arc<AtomicUsize>,
    fail: Arc<AtomicU16>,
}

async fn create_event(
    State(cal): State<Calendar>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let code = cal.fail.load(Ordering::SeqCst);
    if code != 0 {
        return (StatusCode::from_u16(code).unwrap(), "unavailable").into_response();
    }
    cal.creates.lock().unwrap().push(body);
    Json(json!({ "id": format!("evt-{}", Uuid::new_v4()), "htmlLink": "https://cal.test/e" }))
        .into_response()
}

async fn delete_event(State(cal): State<Calendar>) -> StatusCode {
    cal.deletes.fetch_add(1, Ordering::SeqCst);
    StatusCode::NO_CONTENT
}

async fn other_event(State(_c): State<Calendar>) -> Json<Value> {
    Json(json!({ "id": "evt", "items": [] }))
}

async fn mock_calendar() -> (String, Calendar) {
    let cal = Calendar::default();
    let router = Router::new()
        .route(
            "/calendars/{calendar_id}/events",
            post(create_event).get(other_event),
        )
        .route(
            "/calendars/{calendar_id}/events/{event_id}",
            patch(other_event).delete(delete_event),
        )
        .with_state(cal.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{addr}"), cal)
}

struct Server {
    addr: SocketAddr,
    pool: db::Pool,
    calendar: Calendar,
}

fn base(srv: &Server) -> String {
    format!("http://{}", srv.addr)
}

async fn setup() -> Option<Server> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let (cal_url, calendar) = mock_calendar().await;
    let mut config = Config::test_with_billing(&url, SECRET, 0.0);
    config.calendar_base_url = cal_url;
    if let Some(b) = config.billing.as_mut() {
        b.google_client_secret = "test-secret".into();
        b.google_token_enc_key = Some(vec![7u8; 32]);
    }
    let mut state = AppState::new(config);
    state.safety = Some(SafetyService::new(pool.clone()));
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
        calendar,
    })
}

macro_rules! srv {
    () => {
        match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skipping — no DATABASE_URL");
                return;
            }
        }
    };
}

/// A signed-in person whose Google Calendar is connected. A cached access token
/// that is still good short-circuits the refresh, so no OAuth endpoint is involved.
async fn user(srv: &Server, connected: bool) -> (Uuid, String) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: "Planner".into(),
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
    if connected {
        sqlx::query(
            "INSERT INTO google_oauth_tokens (user_id, access_token, expires_at, scopes)
             VALUES ($1, 'ya29.fake', now() + interval '1 hour',
                     'https://www.googleapis.com/auth/calendar.events')
             ON CONFLICT (user_id) DO UPDATE SET access_token = EXCLUDED.access_token",
        )
        .bind(u.id)
        .execute(&srv.pool)
        .await
        .unwrap();
    }
    let jwt = issue_jwt(SECRET, &u.id, &u.email, &u.name, 168).unwrap();
    (u.id, jwt)
}

fn tomorrow() -> String {
    (Utc::now() + Duration::days(1)).to_rfc3339()
}

async fn create(http: &Client, srv: &Server, jwt: &str, body: Value) -> reqwest::Response {
    http.post(format!("{}/api/meetings", base(srv)))
        .bearer_auth(jwt)
        .json(&body)
        .send()
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_personal_meeting_is_created_in_the_calendar_and_returned_with_its_room() {
    let srv = srv!();
    let http = Client::new();
    let (_uid, jwt) = user(&srv, true).await;

    let r = create(
        &http,
        &srv,
        &jwt,
        json!({
            "title": "Coffee with Ana",
            "scheduled_at": tomorrow(),
            "duration_minutes": 20,
            "timezone": "Europe/Rome",
            "invitee_emails": ["ana@example.com"],
        }),
    )
    .await;
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();

    let sent = srv.calendar.creates.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["summary"], "Coffee with Ana");
    assert_eq!(sent[0]["start"]["timeZone"], "Europe/Rome");
    // The room is the product: a calendar entry with no way into the call is just
    // a diary note.
    let join = body["join_url"].as_str().unwrap();
    assert!(join.contains("?room="));
    assert_eq!(sent[0]["location"], join);
}

#[tokio::test]
async fn a_meeting_with_no_title_never_reaches_the_calendar() {
    let srv = srv!();
    let http = Client::new();
    let (_uid, jwt) = user(&srv, true).await;

    let r = create(
        &http,
        &srv,
        &jwt,
        json!({ "title": "   ", "scheduled_at": tomorrow() }),
    )
    .await;
    assert_eq!(r.status(), 400);
    assert_eq!(srv.calendar.creates.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn a_meeting_that_ends_before_it_starts_is_refused() {
    let srv = srv!();
    let http = Client::new();
    let (_uid, jwt) = user(&srv, true).await;

    let start = Utc::now() + Duration::days(1);
    let r = create(
        &http,
        &srv,
        &jwt,
        json!({
            "title": "Backwards",
            "scheduled_at": start.to_rfc3339(),
            "end_at": (start - Duration::minutes(30)).to_rfc3339(),
        }),
    )
    .await;
    assert_eq!(r.status(), 400);
    assert_eq!(srv.calendar.creates.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn an_invitee_address_that_is_not_an_address_is_refused() {
    let srv = srv!();
    let http = Client::new();
    let (_uid, jwt) = user(&srv, true).await;

    let r = create(
        &http,
        &srv,
        &jwt,
        json!({
            "title": "Typo",
            "scheduled_at": tomorrow(),
            "invitee_emails": ["ana@example.com", "ana-at-example"],
        }),
    )
    .await;
    // Dropping it silently would leave the organiser believing someone was invited
    // who never hears about the meeting.
    assert_eq!(r.status(), 400);
    assert_eq!(srv.calendar.creates.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn the_same_address_written_twice_is_one_invitation() {
    let srv = srv!();
    let http = Client::new();
    let (_uid, jwt) = user(&srv, true).await;

    let r = create(
        &http,
        &srv,
        &jwt,
        json!({
            "title": "Once",
            "scheduled_at": tomorrow(),
            "invitee_emails": ["ana@example.com", "ANA@Example.com"],
        }),
    )
    .await;
    assert_eq!(r.status(), 201);

    let sent = srv.calendar.creates.lock().unwrap().clone();
    assert_eq!(
        sent[0]["attendees"].as_array().map(|a| a.len()),
        Some(1),
        "the same person was invited twice"
    );
}

#[tokio::test]
async fn a_calendar_outage_leaves_nothing_half_created() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv, true).await;
    srv.calendar.fail.store(500, Ordering::SeqCst);

    let r = create(
        &http,
        &srv,
        &jwt,
        json!({ "title": "Doomed", "scheduled_at": tomorrow() }),
    )
    .await;
    assert_eq!(r.status(), 502);

    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM scheduled_meetings WHERE creator_user_id = $1")
            .bind(uid)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(rows, 0, "an orphan meeting was stored");
}

#[tokio::test]
async fn without_a_connected_calendar_the_answer_says_so() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv, false).await;

    let r = create(
        &http,
        &srv,
        &jwt,
        json!({ "title": "No calendar", "scheduled_at": tomorrow() }),
    )
    .await;
    // The fix is a "connect Google Calendar" button, so this must be a client error
    // the UI can react to, not a 500.
    assert!(r.status().is_client_error(), "got {}", r.status());

    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM scheduled_meetings WHERE creator_user_id = $1")
            .bind(uid)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn a_recurring_meeting_carries_its_rule() {
    let srv = srv!();
    let http = Client::new();
    let (_uid, jwt) = user(&srv, true).await;

    let r = create(
        &http,
        &srv,
        &jwt,
        json!({
            "title": "Weekly one-to-one",
            "scheduled_at": tomorrow(),
            "recurrence": { "freq": "weekly", "interval": 1, "count": 12 },
        }),
    )
    .await;
    assert_eq!(r.status(), 201);

    let sent = srv.calendar.creates.lock().unwrap().clone();
    let rule = sent[0]["recurrence"].as_array().expect("no RRULE")[0]
        .as_str()
        .unwrap()
        .to_string();
    assert!(rule.starts_with("RRULE:FREQ=WEEKLY"), "{rule}");
    assert!(rule.contains("COUNT=12"), "{rule}");
}

#[tokio::test]
async fn meetings_are_listed_fetched_and_cancelled_by_their_owner_only() {
    let srv = srv!();
    let http = Client::new();
    let (_mine, jwt) = user(&srv, true).await;
    let (_theirs, other_jwt) = user(&srv, true).await;

    let created: Value = create(
        &http,
        &srv,
        &jwt,
        json!({
            "title": "Mine",
            "scheduled_at": tomorrow(),
            "invitee_emails": ["ana@example.com"],
        }),
    )
    .await
    .json()
    .await
    .unwrap();
    let id = created["id"].as_str().unwrap();

    let detail: Value = http
        .get(format!("{}/api/meetings/{id}", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail["title"], "Mine");
    assert_eq!(detail["invitees"].as_array().map(|a| a.len()), Some(1));

    // Somebody else's meeting is a 404, not a 403: confirming it exists would leak
    // that this person has a meeting at all.
    let theirs = http
        .get(format!("{}/api/meetings/{id}", base(&srv)))
        .bearer_auth(&other_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(theirs.status(), 404);

    let list: Value = http
        .get(format!("{}/api/meetings", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&id));

    // The list is a window, not everything: a meeting outside it is not returned.
    // `Z`, not `+00:00`: a raw `+` in a query string is a space, and the window
    // then fails to parse instead of being applied.
    let empty: Value = http
        .get(format!(
            "{}/api/meetings?from={}&to={}",
            base(&srv),
            (Utc::now() + Duration::days(300)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            (Utc::now() + Duration::days(400)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(empty.as_array().unwrap().is_empty());

    let refused = http
        .post(format!("{}/api/meetings/{id}/cancel", base(&srv)))
        .bearer_auth(&other_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 404, "someone else cancelled my meeting");

    let ok = http
        .post(format!("{}/api/meetings/{id}/cancel", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert!(ok.status().is_success(), "cancel: {}", ok.status());
    assert_eq!(
        srv.calendar.deletes.load(Ordering::SeqCst),
        1,
        "the invitee's calendar still shows the meeting"
    );

    let status: String =
        sqlx::query_scalar("SELECT status FROM scheduled_meetings WHERE id = $1::uuid")
            .bind(id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(status, "cancelled");
}

#[tokio::test]
async fn a_meeting_that_never_reached_the_calendar_still_cancels() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv, true).await;

    // Seeded straight into the table with no event id — the shape a row left over
    // from an older schema, or a partial failure, actually has.
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO scheduled_meetings
            (id, creator_user_id, title, scheduled_at, end_at, room_code, join_url, status)
         VALUES ($1,$2,'Orphan',$3,$4,$5,'https://app.test/?room=x','scheduled')",
    )
    .bind(id)
    .bind(uid)
    .bind(Utc::now() + Duration::days(1))
    .bind(Utc::now() + Duration::days(1) + Duration::minutes(30))
    .bind(format!("room-{}", Uuid::new_v4().simple()))
    .execute(&srv.pool)
    .await
    .unwrap();

    let r = http
        .post(format!("{}/api/meetings/{id}/cancel", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();

    // Nothing to delete upstream is not a failure — the user asked for the meeting
    // to be off, and it is off.
    assert!(r.status().is_success(), "cancel: {}", r.status());
    assert_eq!(srv.calendar.deletes.load(Ordering::SeqCst), 0);
}
