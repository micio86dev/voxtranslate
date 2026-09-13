//! The org-scoped scheduled-meeting lifecycle, with Google Calendar standing in.
//!
//! `business_meetings_api.rs` covers the refusals; these are the paths that reach
//! Calendar, which is the source of truth: every create, update and cancel writes
//! there BEFORE our own row and bails if it cannot. `Config::calendar_base_url`
//! points those four calls at a stand-in, so what is asserted here is the ordering
//! (no orphan rows), the invitee resolution, and what a Calendar outage does.
//!
//! DB-gated: skipped without `DATABASE_URL`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Path as AxPath, State};
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

const SECRET: &str = "biz-meetings-calendar-secret";

// ---------------------------------------------------------------------------
// A stand-in for Google Calendar
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Calendar {
    creates: Arc<Mutex<Vec<Value>>>,
    updates: Arc<Mutex<Vec<Value>>>,
    deletes: Arc<AtomicUsize>,
    /// Non-zero makes every write fail with this status.
    fail: Arc<AtomicU16>,
}

fn maybe_fail(cal: &Calendar) -> Option<axum::response::Response> {
    let code = cal.fail.load(Ordering::SeqCst);
    (code != 0).then(|| {
        (
            StatusCode::from_u16(code).unwrap(),
            "the calendar is unavailable",
        )
            .into_response()
    })
}

async fn create_event(
    State(cal): State<Calendar>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    if let Some(r) = maybe_fail(&cal) {
        return r;
    }
    cal.creates.lock().unwrap().push(body);
    Json(json!({ "id": format!("evt-{}", Uuid::new_v4()), "htmlLink": "https://cal.test/e" }))
        .into_response()
}

async fn update_event(
    State(cal): State<Calendar>,
    AxPath((_c, event_id)): AxPath<(String, String)>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    if let Some(r) = maybe_fail(&cal) {
        return r;
    }
    cal.updates.lock().unwrap().push(body);
    Json(json!({ "id": event_id, "htmlLink": "https://cal.test/e" })).into_response()
}

async fn delete_event(State(cal): State<Calendar>) -> StatusCode {
    cal.deletes.fetch_add(1, Ordering::SeqCst);
    StatusCode::NO_CONTENT
}

async fn list_events(State(_cal): State<Calendar>) -> Json<Value> {
    Json(json!({ "items": [] }))
}

async fn mock_calendar() -> (String, Calendar) {
    let cal = Calendar::default();
    let router = Router::new()
        .route(
            "/calendars/{calendar_id}/events",
            post(create_event).get(list_events),
        )
        .route(
            "/calendars/{calendar_id}/events/{event_id}",
            patch(update_event).delete(delete_event).get(list_events),
        )
        .with_state(cal.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{addr}"), cal)
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

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
    // `calendar_enabled()` needs both: without them `valid_access_token` reports
    // "not configured" and no handler ever reaches Calendar.
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

async fn user(srv: &Server, name: &str) -> (Uuid, String) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: name.into(),
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

/// Give `uid` a Calendar connection. A cached access token that is still good
/// short-circuits the refresh, so no OAuth token endpoint is involved.
async fn connect_calendar(srv: &Server, uid: Uuid) {
    sqlx::query(
        "INSERT INTO google_oauth_tokens (user_id, access_token, expires_at, scopes)
         VALUES ($1, 'ya29.fake-access-token', now() + interval '1 hour',
                 'https://www.googleapis.com/auth/calendar.events')
         ON CONFLICT (user_id) DO UPDATE
            SET access_token = EXCLUDED.access_token, expires_at = EXCLUDED.expires_at",
    )
    .bind(uid)
    .execute(&srv.pool)
    .await
    .unwrap();
}

async fn org(srv: &Server, owner: Uuid) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id) VALUES ('Meet Co', $1, $2) RETURNING id",
    )
    .bind(format!("meet-{}", Uuid::new_v4().simple()))
    .bind(owner)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO organization_members (org_id, user_id, role) VALUES ($1,$2,'owner')")
        .bind(id)
        .bind(owner)
        .execute(&srv.pool)
        .await
        .unwrap();
    id
}

async fn add_member(srv: &Server, org_id: Uuid, uid: Uuid, role: &str) {
    sqlx::query("INSERT INTO organization_members (org_id, user_id, role) VALUES ($1,$2,$3)")
        .bind(org_id)
        .bind(uid)
        .bind(role)
        .execute(&srv.pool)
        .await
        .unwrap();
}

fn meetings_url(srv: &Server, org_id: Uuid) -> String {
    format!("{}/api/business/organizations/{org_id}/meetings", base(srv))
}

fn tomorrow() -> Value {
    json!((Utc::now() + Duration::days(1)).to_rfc3339())
}

async fn count_rows(srv: &Server, org_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM scheduled_meetings WHERE org_id = $1")
        .bind(org_id)
        .fetch_one(&srv.pool)
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_created_meeting_lands_in_the_calendar_and_in_the_row() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv, "Owner").await;
    connect_calendar(&srv, uid).await;
    let o = org(&srv, uid).await;

    let r = http
        .post(meetings_url(&srv, o))
        .bearer_auth(&jwt)
        .json(&json!({
            "title": "Quarterly review",
            "scheduled_at": tomorrow(),
            "duration_minutes": 45,
            "timezone": "Europe/Rome",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();

    let sent = srv.calendar.creates.lock().unwrap().clone();
    assert_eq!(sent.len(), 1, "nothing reached the calendar");
    assert_eq!(sent[0]["summary"], "Quarterly review");
    // The join link is the reason a calendar entry is worth anything: the invitee
    // opens their calendar, not our dashboard.
    let join = body["join_url"].as_str().unwrap();
    assert!(sent[0]["description"].as_str().unwrap().contains(join));
    assert_eq!(sent[0]["location"], join);
    assert_eq!(sent[0]["start"]["timeZone"], "Europe/Rome");

    assert_eq!(count_rows(&srv, o).await, 1);
}

#[tokio::test]
async fn a_calendar_that_refuses_leaves_no_orphan_row_behind() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv, "Owner").await;
    connect_calendar(&srv, uid).await;
    let o = org(&srv, uid).await;
    srv.calendar.fail.store(503, Ordering::SeqCst);

    let r = http
        .post(meetings_url(&srv, o))
        .bearer_auth(&jwt)
        .json(&json!({ "title": "Doomed", "scheduled_at": tomorrow() }))
        .send()
        .await
        .unwrap();

    // The calendar is the source of truth, so it is written first and the row is
    // never created without it — a row nobody was invited to is worse than an error.
    assert_eq!(r.status(), 502);
    assert_eq!(count_rows(&srv, o).await, 0, "an orphan meeting was stored");
}

#[tokio::test]
async fn invitees_are_resolved_to_members_of_this_org_only() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv, "Owner").await;
    connect_calendar(&srv, uid).await;
    let o = org(&srv, uid).await;
    let (colleague, _) = user(&srv, "Colleague").await;
    add_member(&srv, o, colleague, "member").await;
    let (stranger, _) = user(&srv, "Stranger").await; // in no org of ours

    let r = http
        .post(meetings_url(&srv, o))
        .bearer_auth(&jwt)
        .json(&json!({
            "title": "Kickoff",
            "scheduled_at": tomorrow(),
            "invitee_user_ids": [colleague, stranger],
            "invitee_emails": ["client@example.com", "CLIENT@example.com"],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();

    let sent = srv.calendar.creates.lock().unwrap().clone();
    let attendees: Vec<String> = sent[0]["attendees"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["email"].as_str().unwrap().to_lowercase())
        .collect();

    // Two, not three: turning a user id into an email address is a directory
    // lookup, and it must not cross the tenant line. And the same address written
    // twice in two cases is one person, not two invitations.
    assert_eq!(attendees.len(), 2, "resolved: {attendees:?}");
    assert_eq!(
        attendees
            .iter()
            .filter(|e| e.as_str() == "client@example.com")
            .count(),
        1,
        "the same address was invited twice"
    );

    let stored: i64 =
        sqlx::query_scalar("SELECT count(*) FROM scheduled_meeting_invitees WHERE meeting_id = $1")
            .bind(Uuid::parse_str(body["id"].as_str().unwrap()).unwrap())
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(stored, 2, "the invitees were not persisted");
}

#[tokio::test]
async fn an_address_that_is_not_an_address_is_refused_not_dropped() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv, "Owner").await;
    connect_calendar(&srv, uid).await;
    let o = org(&srv, uid).await;

    let r = http
        .post(meetings_url(&srv, o))
        .bearer_auth(&jwt)
        .json(&json!({
            "title": "Typo",
            "scheduled_at": tomorrow(),
            "invitee_emails": ["colleague@example.com", "not-an-email"],
        }))
        .send()
        .await
        .unwrap();

    // Silently dropping it would mean the organiser believes someone was invited
    // who never hears about the meeting — the one failure a calendar cannot have.
    assert_eq!(r.status(), 400);
    assert_eq!(count_rows(&srv, o).await, 0);
    assert_eq!(srv.calendar.creates.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn a_recurring_meeting_carries_its_rule_to_the_calendar() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv, "Owner").await;
    connect_calendar(&srv, uid).await;
    let o = org(&srv, uid).await;

    let r = http
        .post(meetings_url(&srv, o))
        .bearer_auth(&jwt)
        .json(&json!({
            "title": "Weekly standup",
            "scheduled_at": tomorrow(),
            "recurrence": { "freq": "weekly", "interval": 2, "count": 10 },
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    let sent = srv.calendar.creates.lock().unwrap().clone();
    // Recurrence belongs to Calendar, not to a loop of our own: it is what makes
    // the series editable from the invitee's own calendar app.
    let rrule = sent[0]["recurrence"].as_array().expect("no RRULE");
    let rule = rrule[0].as_str().unwrap();
    assert!(rule.starts_with("RRULE:FREQ=WEEKLY"), "{rule}");
    assert!(rule.contains("INTERVAL=2"), "{rule}");
    assert!(rule.contains("COUNT=10"), "{rule}");
}

#[tokio::test]
async fn a_frequency_the_calendar_would_reject_becomes_a_one_off() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv, "Owner").await;
    connect_calendar(&srv, uid).await;
    let o = org(&srv, uid).await;

    let r = http
        .post(meetings_url(&srv, o))
        .bearer_auth(&jwt)
        .json(&json!({
            "title": "Every fortnight-ish",
            "scheduled_at": tomorrow(),
            "recurrence": { "freq": "hourly" },
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    // A frequency we do not model is dropped rather than refused: the meeting the
    // user asked for still exists, it simply does not repeat.
    let sent = srv.calendar.creates.lock().unwrap().clone();
    assert!(
        sent[0].get("recurrence").is_none_or(|v| v.is_null()),
        "an unmodelled frequency reached the calendar"
    );
}

#[tokio::test]
async fn a_meeting_cannot_end_before_it_starts() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv, "Owner").await;
    connect_calendar(&srv, uid).await;
    let o = org(&srv, uid).await;

    let start = Utc::now() + Duration::days(1);
    let r = http
        .post(meetings_url(&srv, o))
        .bearer_auth(&jwt)
        .json(&json!({
            "title": "Backwards",
            "scheduled_at": start.to_rfc3339(),
            "end_at": (start - Duration::hours(1)).to_rfc3339(),
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(r.status(), 400);
    // Validation runs before the calendar call, so nothing was created there either.
    assert_eq!(srv.calendar.creates.lock().unwrap().len(), 0);
}

// ---------------------------------------------------------------------------
// Update and cancel
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_edit_is_pushed_to_the_calendar_the_invitees_are_looking_at() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv, "Owner").await;
    connect_calendar(&srv, uid).await;
    let o = org(&srv, uid).await;

    let created: Value = http
        .post(meetings_url(&srv, o))
        .bearer_auth(&jwt)
        .json(&json!({ "title": "Draft title", "scheduled_at": tomorrow() }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap();

    let r = http
        .patch(format!("{}/{id}", meetings_url(&srv, o)))
        .bearer_auth(&jwt)
        .json(&json!({
            "title": "Final title",
            "scheduled_at": (Utc::now() + Duration::days(2)).to_rfc3339(),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let sent = srv.calendar.updates.lock().unwrap().clone();
    assert_eq!(sent.len(), 1, "the calendar was not told about the edit");
    assert_eq!(sent[0]["summary"], "Final title");

    let title: String =
        sqlx::query_scalar("SELECT title FROM scheduled_meetings WHERE id = $1::uuid")
            .bind(id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(title, "Final title");
}

#[tokio::test]
async fn cancelling_removes_the_event_and_marks_the_row() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv, "Owner").await;
    connect_calendar(&srv, uid).await;
    let o = org(&srv, uid).await;

    let created: Value = http
        .post(meetings_url(&srv, o))
        .bearer_auth(&jwt)
        .json(&json!({ "title": "Called off", "scheduled_at": tomorrow() }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap();

    let r = http
        .post(format!("{}/{id}/cancel", meetings_url(&srv, o)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "cancel: {}", r.status());

    assert_eq!(
        srv.calendar.deletes.load(Ordering::SeqCst),
        1,
        "the invitees' calendars still show the meeting"
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
async fn listing_and_fetching_return_what_was_created() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv, "Owner").await;
    connect_calendar(&srv, uid).await;
    let o = org(&srv, uid).await;

    let created: Value = http
        .post(meetings_url(&srv, o))
        .bearer_auth(&jwt)
        .json(&json!({
            "title": "Readable",
            "scheduled_at": tomorrow(),
            "invitee_emails": ["guest@example.com"],
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap();

    let one: Value = http
        .get(format!("{}/{id}", meetings_url(&srv, o)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(one["title"], "Readable");
    // The detail view carries the invitees; the list deliberately does not, so a
    // month of meetings is not a month of joins.
    assert_eq!(one["invitees"].as_array().map(|a| a.len()), Some(1));

    let all: Value = http
        .get(meetings_url(&srv, o))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = all
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&id));
}

#[tokio::test]
async fn a_member_without_a_connected_calendar_is_told_what_to_do() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv, "Owner").await;
    let o = org(&srv, uid).await; // deliberately NOT connected

    let r = http
        .post(meetings_url(&srv, o))
        .bearer_auth(&jwt)
        .json(&json!({ "title": "No calendar", "scheduled_at": tomorrow() }))
        .send()
        .await
        .unwrap();

    // Not a 500: the fix is a button in the UI, and the status has to say so.
    assert!(
        r.status().is_client_error(),
        "expected a client error, got {}",
        r.status()
    );
    assert_eq!(count_rows(&srv, o).await, 0);
}
