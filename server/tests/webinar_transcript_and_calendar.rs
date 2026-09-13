//! A webinar's transcript endpoints and its calendar entry.
//!
//! Both sides of the transcript are here — the public one a viewer catches up
//! with and the host one the dashboard reads — because the interesting rule is
//! that they answer DIFFERENTLY for the same webinar: one is gated by
//! `members_only`, the other by org membership, and both return an empty array
//! rather than an error when recording was never switched on.
//!
//! `POST /api/webinars/{id}/calendar` is reachable now that
//! `Config::calendar_base_url` has a stand-in to point at.
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
use voxtranslate_server::config::{Config, WebinarConfig};
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::{app, db, AppState};

const SECRET: &str = "webinar-transcript-secret";

#[derive(Clone, Default)]
struct Calendar {
    creates: Arc<Mutex<Vec<Value>>>,
    updates: Arc<AtomicUsize>,
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
    Json(json!({ "id": "evt-new", "htmlLink": "https://cal.test/evt-new" })).into_response()
}

async fn update_event(
    State(cal): State<Calendar>,
    AxPath((_c, event_id)): AxPath<(String, String)>,
    Json(_body): Json<Value>,
) -> axum::response::Response {
    let code = cal.fail.load(Ordering::SeqCst);
    if code != 0 {
        return (StatusCode::from_u16(code).unwrap(), "unavailable").into_response();
    }
    cal.updates.fetch_add(1, Ordering::SeqCst);
    Json(json!({ "id": event_id, "htmlLink": "https://cal.test/kept" })).into_response()
}

async fn mock_calendar() -> (String, Calendar) {
    let cal = Calendar::default();
    let router = Router::new()
        .route("/calendars/{calendar_id}/events", post(create_event))
        .route(
            "/calendars/{calendar_id}/events/{event_id}",
            patch(update_event),
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
    config.webinar = Some(WebinarConfig::test_default());
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

async fn user(srv: &Server, connected: bool) -> (Uuid, String) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: "Host".into(),
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
             VALUES ($1, 'ya29.fake', now() + interval '1 hour', 'calendar.events')
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

async fn org(srv: &Server, owner: Uuid) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id, subscription_status, current_period_end)
         VALUES ('Acme', $1, $2, 'active', now() + interval '30 days') RETURNING id",
    )
    .bind(format!("org-{}", Uuid::new_v4().simple()))
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

async fn webinar(http: &Client, srv: &Server, jwt: &str, org_id: Uuid) -> (Uuid, String) {
    let r = http
        .post(format!("{}/api/webinars", base(srv)))
        .bearer_auth(jwt)
        .json(&json!({ "org_id": org_id, "title": "Launch", "source_language": "en" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "create webinar");
    let b: Value = r.json().await.unwrap();
    (
        b["id"].as_str().unwrap().parse().unwrap(),
        b["code"].as_str().unwrap().to_string(),
    )
}

async fn record_transcript(srv: &Server, id: Uuid, on: bool) {
    sqlx::query("UPDATE webinars SET record_transcript = $2 WHERE id = $1")
        .bind(id)
        .bind(on)
        .execute(&srv.pool)
        .await
        .unwrap();
}

async fn seed_lines(srv: &Server, id: Uuid, n: i32) {
    for i in 0..n {
        sqlx::query(
            "INSERT INTO webinar_transcripts
                (webinar_id, original_text, original_lang, translations, spoken_at)
             VALUES ($1, $2, 'en', $3, now() + ($4 || ' seconds')::interval)",
        )
        .bind(id)
        .bind(format!("line {i}"))
        .bind(json!({ "it": format!("riga {i}") }))
        .bind(i.to_string())
        .execute(&srv.pool)
        .await
        .unwrap();
    }
}

// ---------------------------------------------------------------------------
// The public transcript
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_viewer_catching_up_reads_the_transcript_with_its_translations() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv, false).await;
    let o = org(&srv, uid).await;
    let (id, code) = webinar(&http, &srv, &jwt, o).await;
    record_transcript(&srv, id, true).await;
    seed_lines(&srv, id, 3).await;

    let r = http
        .get(format!("{}/api/w/{code}/transcript", base(&srv)))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "a guest can read a public transcript");
    let body: Value = r.json().await.unwrap();
    let rows = body.as_array().unwrap();
    assert_eq!(rows.len(), 3);

    // Chronological: a transcript out of order is not a transcript.
    assert_eq!(rows[0]["original"], "line 0");
    assert_eq!(rows[2]["original"], "line 2");
    assert_eq!(rows[0]["lang"], "en");
    assert_eq!(rows[0]["translations"]["it"], "riga 0");
    assert!(rows[0]["spoken_at"].as_str().unwrap().contains('T'));
}

#[tokio::test]
async fn a_transcript_that_was_never_recorded_is_empty_rather_than_missing() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv, false).await;
    let o = org(&srv, uid).await;
    let (id, code) = webinar(&http, &srv, &jwt, o).await;
    record_transcript(&srv, id, false).await;
    // Rows can exist from before the flag was turned off; the gate is the flag.
    seed_lines(&srv, id, 2).await;

    let body: Value = http
        .get(format!("{}/api/w/{code}/transcript", base(&srv)))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // 200 with nothing in it: the viewer is told there is no transcript, not that
    // the webinar does not exist.
    assert_eq!(body.as_array().map(|a| a.len()), Some(0));
}

#[tokio::test]
async fn the_public_transcript_is_clamped_however_much_is_asked_for() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv, false).await;
    let o = org(&srv, uid).await;
    let (id, code) = webinar(&http, &srv, &jwt, o).await;
    record_transcript(&srv, id, true).await;
    seed_lines(&srv, id, 6).await;

    for (limit, want) in [("2", 2), ("0", 1), ("100000", 6)] {
        let body: Value = http
            .get(format!(
                "{}/api/w/{code}/transcript?limit={limit}",
                base(&srv)
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        // A catch-up view is capped at 200 whatever the client asks for, and a
        // zero is clamped up to one rather than refused — this endpoint never
        // answers a live viewer with an error it cannot do anything about.
        assert_eq!(
            body.as_array().map(|a| a.len()),
            Some(want),
            "limit={limit}"
        );
    }
}

#[tokio::test]
async fn an_unknown_code_is_a_404_not_an_empty_transcript() {
    let srv = srv!();
    let http = Client::new();

    let r = http
        .get(format!("{}/api/w/nosuchwebinar/transcript", base(&srv)))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn a_members_only_transcript_is_not_public() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv, false).await;
    let o = org(&srv, uid).await;
    let (id, code) = webinar(&http, &srv, &jwt, o).await;
    record_transcript(&srv, id, true).await;
    seed_lines(&srv, id, 2).await;
    sqlx::query("UPDATE webinars SET members_only = TRUE WHERE id = $1")
        .bind(id)
        .execute(&srv.pool)
        .await
        .unwrap();

    // The transcript is exactly the content that flag exists to protect: it is
    // everything that was said.
    let guest = http
        .get(format!("{}/api/w/{code}/transcript", base(&srv)))
        .send()
        .await
        .unwrap();
    assert!(
        guest.status().is_client_error(),
        "a guest read a members-only transcript ({})",
        guest.status()
    );

    let member: Value = http
        .get(format!("{}/api/w/{code}/transcript", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(member.as_array().map(|a| a.len()), Some(2));
}

// ---------------------------------------------------------------------------
// The host transcript
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_host_view_returns_the_raw_rows_under_their_own_names() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv, false).await;
    let o = org(&srv, uid).await;
    let (id, _code) = webinar(&http, &srv, &jwt, o).await;
    seed_lines(&srv, id, 2).await;

    let body: Value = http
        .get(format!("{}/api/webinars/{id}/transcripts", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rows = body.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    // Different field names from the public view on purpose: this one feeds the
    // dashboard's export, not a viewer's catch-up panel.
    assert_eq!(rows[0]["original_text"], "line 0");
    assert_eq!(rows[0]["original_lang"], "en");
    assert_eq!(rows[0]["translations"]["it"], "riga 0");
}

#[tokio::test]
async fn another_orgs_member_cannot_read_the_transcript() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv, false).await;
    let o = org(&srv, uid).await;
    let (id, _code) = webinar(&http, &srv, &jwt, o).await;
    seed_lines(&srv, id, 1).await;

    let (outsider, outsider_jwt) = user(&srv, false).await;
    org(&srv, outsider).await;

    let r = http
        .get(format!("{}/api/webinars/{id}/transcripts", base(&srv)))
        .bearer_auth(&outsider_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404, "cross-tenant must not confirm the id");

    let anon = http
        .get(format!("{}/api/webinars/{id}/transcripts", base(&srv)))
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);
}

// ---------------------------------------------------------------------------
// The calendar entry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_scheduled_webinar_becomes_a_calendar_entry_carrying_its_join_link() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv, true).await;
    let o = org(&srv, uid).await;
    let (id, code) = webinar(&http, &srv, &jwt, o).await;
    sqlx::query("UPDATE webinars SET scheduled_start = $2, scheduled_end = $3 WHERE id = $1")
        .bind(id)
        .bind(Utc::now() + Duration::days(2))
        .bind(Utc::now() + Duration::days(2) + Duration::hours(1))
        .execute(&srv.pool)
        .await
        .unwrap();

    let r = http
        .post(format!("{}/api/webinars/{id}/calendar", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["google_event_id"], "evt-new");
    assert!(body["join_url"].as_str().unwrap().contains(&code));

    let sent = srv.calendar.creates.lock().unwrap().clone();
    assert_eq!(sent[0]["summary"], "Launch");
    // The webinar id rides along as a private property so the event can be found
    // again from our side without storing a second mapping.
    let props = &sent[0]["extendedProperties"]["private"];
    assert_eq!(props["webinar_code"], code);

    let stored: Option<String> =
        sqlx::query_scalar("SELECT google_event_id FROM webinars WHERE id = $1")
            .bind(id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(stored.as_deref(), Some("evt-new"));
}

#[tokio::test]
async fn re_syncing_updates_the_event_instead_of_creating_a_second_one() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv, true).await;
    let o = org(&srv, uid).await;
    let (id, _code) = webinar(&http, &srv, &jwt, o).await;
    sqlx::query(
        "UPDATE webinars SET scheduled_start = $2, google_event_id = 'evt-existing' WHERE id = $1",
    )
    .bind(id)
    .bind(Utc::now() + Duration::days(2))
    .execute(&srv.pool)
    .await
    .unwrap();

    let r = http
        .post(format!("{}/api/webinars/{id}/calendar", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // Creating a second event would put the same webinar in the invitees' diary
    // twice, and the first copy would never be updated again.
    assert_eq!(srv.calendar.updates.load(Ordering::SeqCst), 1);
    assert_eq!(srv.calendar.creates.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn a_webinar_with_no_date_has_nothing_to_put_in_a_calendar() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv, true).await;
    let o = org(&srv, uid).await;
    let (id, _code) = webinar(&http, &srv, &jwt, o).await;

    let r = http
        .post(format!("{}/api/webinars/{id}/calendar", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(srv.calendar.creates.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn a_host_with_no_connected_calendar_is_told_to_connect_one() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv, false).await;
    let o = org(&srv, uid).await;
    let (id, _code) = webinar(&http, &srv, &jwt, o).await;
    sqlx::query("UPDATE webinars SET scheduled_start = $2 WHERE id = $1")
        .bind(id)
        .bind(Utc::now() + Duration::days(2))
        .execute(&srv.pool)
        .await
        .unwrap();

    let r = http
        .post(format!("{}/api/webinars/{id}/calendar", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_client_error(), "got {}", r.status());
}

#[tokio::test]
async fn a_calendar_outage_leaves_the_webinar_without_a_stale_event_id() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv, true).await;
    let o = org(&srv, uid).await;
    let (id, _code) = webinar(&http, &srv, &jwt, o).await;
    sqlx::query("UPDATE webinars SET scheduled_start = $2 WHERE id = $1")
        .bind(id)
        .bind(Utc::now() + Duration::days(2))
        .execute(&srv.pool)
        .await
        .unwrap();
    srv.calendar.fail.store(503, Ordering::SeqCst);

    let r = http
        .post(format!("{}/api/webinars/{id}/calendar", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 502);

    let stored: Option<String> =
        sqlx::query_scalar("SELECT google_event_id FROM webinars WHERE id = $1")
            .bind(id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    // An id written for an event that was never created would make every later
    // re-sync patch something that does not exist.
    assert!(stored.is_none());
}
