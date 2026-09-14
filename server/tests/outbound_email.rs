//! Everything this server puts in somebody's inbox: bug reports, the Business
//! contact form, room invites and the call recap.
//!
//! All four go through one `Resend::send`, so a single endpoint override
//! (`RESEND_BASE_URL`) is what makes them testable — until now every one of these
//! handlers stopped at the provider call, and the only thing a test could assert
//! was the 503 when mail was unconfigured.
//!
//! DB-gated: skipped without `DATABASE_URL`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use reqwest::Client;
use serde_json::{json, Value};
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::{Config, ResendConfig};
use voxtranslate_server::db::{self, Pool};
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::transcripts::{EventKind, TranscriptEvent, TranscriptService};
use voxtranslate_server::{app, AppState};

const SECRET: &str = "outbound-email-secret";

// ---------------------------------------------------------------------------
// A stand-in for Resend
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct MockResend {
    /// Every message the server tried to send, as Resend received it.
    sent: Arc<Mutex<Vec<Value>>>,
    fail_status: Arc<AtomicU32>,
}

impl MockResend {
    fn messages(&self) -> Vec<Value> {
        self.sent.lock().unwrap().clone()
    }
}

async fn mock_send(
    State(mock): State<MockResend>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let failure = mock.fail_status.load(Ordering::SeqCst);
    if failure != 0 {
        return (
            StatusCode::from_u16(failure as u16).unwrap(),
            r#"{"message":"refused"}"#,
        )
            .into_response();
    }
    mock.sent.lock().unwrap().push(body);
    Json(json!({ "id": Uuid::new_v4().to_string() })).into_response()
}

async fn mock_resend() -> (String, MockResend) {
    let mock = MockResend::default();
    let router = Router::new()
        .route("/emails", post(mock_send))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{addr}/emails"), mock)
}

/// A Groq stand-in, so the recap that precedes a send can be produced.
async fn mock_groq() -> String {
    let router = Router::new().route(
        "/chat/completions",
        post(|Json(body): Json<Value>| async move {
            let content = if body["response_format"]["type"] == "json_object" {
                json!({
                    "subject": "Recap of our call",
                    "body_text": "Here is what we agreed.",
                    "body_html": "<p>Here is what we agreed.</p>",
                })
                .to_string()
            } else {
                "## Summary\n\nIt went well.".to_string()
            };
            Json(json!({ "choices": [{ "message": { "content": content } }] }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    format!("http://{addr}/chat/completions")
}

// ---------------------------------------------------------------------------
// Server harness
// ---------------------------------------------------------------------------

struct Server {
    addr: SocketAddr,
    pool: Pool,
    mail: MockResend,
}

/// `resend = false` reproduces a deployment with no mail provider.
async fn setup_with(resend: bool) -> Option<Server> {
    let url = voxtranslate_server::db::test_database_url()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;

    let (resend_url, mail) = mock_resend().await;
    let mut config = Config::test_with_billing(&url, SECRET, 5.0);
    config.groq_base_url = Some(mock_groq().await);
    config.bug_report_to = "ops@example.com".into();
    if resend {
        config.resend = Some(ResendConfig {
            api_key: "dummy".into(),
            from_email: "noreply@example.com".into(),
            from_name: "VoxTranslate".into(),
        });
        config.resend_base_url = Some(resend_url);
    }
    let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);
    let mut state = AppState::new(config);
    state.billing = Some(BillingService::new(pool.clone(), min_join));
    state.safety = Some(SafetyService::new(pool.clone()));
    state.transcripts = Some(TranscriptService::new(pool.clone()));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    Some(Server { addr, pool, mail })
}

async fn setup() -> Option<Server> {
    setup_with(true).await
}

fn base(srv: &Server) -> String {
    format!("http://{}", srv.addr)
}

async fn login(srv: &Server, name: &str) -> (Uuid, String) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: name.into(),
        avatar_url: None,
    };
    let (user, _) = upsert_google_user(
        &srv.pool,
        &identity,
        rust_decimal::Decimal::new(500, 2),
        None,
        None,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE users SET age_confirmed = TRUE, consent_tos_at = now() WHERE id = $1")
        .bind(user.id)
        .execute(&srv.pool)
        .await
        .unwrap();
    let jwt = issue_jwt(SECRET, &user.id, &user.email, &user.name, 168).unwrap();
    (user.id, jwt)
}

async fn seeded_session(srv: &Server, uid: Uuid) -> Uuid {
    let session_id = Uuid::new_v4();
    let svc = TranscriptService::new(srv.pool.clone());
    svc.session_started(session_id, &format!("em-{}", Uuid::new_v4().simple()))
        .await
        .unwrap();
    svc.participant_joined(session_id, "peer-1", Some(uid), "Tess", "en")
        .await
        .unwrap();
    svc.record(TranscriptEvent {
        session_id,
        kind: EventKind::Speech,
        speaker_peer_id: "peer-1".into(),
        speaker_user_id: Some(uid),
        speaker_name: "Tess".into(),
        original_text: "We agreed to ship on Friday.".into(),
        original_lang: "en".into(),
        translations: HashMap::new(),
        ts: chrono::Utc::now(),
    });
    svc.flush().await;
    sqlx::query("UPDATE call_sessions SET ended_at = now() WHERE id = $1")
        .bind(session_id)
        .execute(&srv.pool)
        .await
        .unwrap();
    session_id
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

/// Every address a message went to, flattened.
fn recipients(msg: &Value) -> Vec<String> {
    msg["to"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Bug reports
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_bug_report_reaches_the_inbox_it_is_addressed_to() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (_, jwt) = login(&srv, "Tess").await;

    let r = http
        .post(format!("{}/api/bug-report", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({
            "message": "Subtitles stop after ten minutes.",
            "page_url": "https://app.voxtranslate.app/?room=x",
        }))
        .send()
        .await
        .unwrap();

    assert!(r.status().is_success(), "got {} ", r.status());
    let sent = srv.mail.messages();
    assert_eq!(sent.len(), 1, "one report, one email");
    assert!(recipients(&sent[0]).contains(&"ops@example.com".to_string()));
    assert!(
        sent[0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("Subtitles stop after ten minutes")
            || sent[0]["html"]
                .as_str()
                .unwrap_or_default()
                .contains("Subtitles stop after ten minutes"),
        "the report body must carry what the user wrote: {:?}",
        sent[0]
    );
}

#[tokio::test]
async fn an_empty_bug_report_is_refused_before_anything_is_sent() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (_, jwt) = login(&srv, "Tess").await;

    let r = http
        .post(format!("{}/api/bug-report", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "message": "   " }))
        .send()
        .await
        .unwrap();

    assert!(!r.status().is_success(), "got {}", r.status());
    assert!(srv.mail.messages().is_empty());
}

#[tokio::test]
async fn a_bug_report_long_enough_to_be_a_payload_is_refused() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (_, jwt) = login(&srv, "Tess").await;

    let r = http
        .post(format!("{}/api/bug-report", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "message": "x".repeat(100_000) }))
        .send()
        .await
        .unwrap();

    assert!(!r.status().is_success(), "got {}", r.status());
    assert!(srv.mail.messages().is_empty());
}

#[tokio::test]
async fn a_bug_report_that_the_mail_provider_refuses_is_reported_as_such() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (_, jwt) = login(&srv, "Tess").await;
    srv.mail.fail_status.store(401, Ordering::SeqCst);

    let r = http
        .post(format!("{}/api/bug-report", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "message": "Something broke." }))
        .send()
        .await
        .unwrap();

    // The report is still kept: the email is best-effort, the triage row is not.
    assert!(r.status().is_success(), "got {}", r.status());
    assert!(srv.mail.messages().is_empty());
}

#[tokio::test]
async fn a_bug_report_is_kept_even_when_mail_is_unconfigured() {
    let srv = skip_without_db!(setup_with(false).await);
    let http = Client::new();
    let (_, jwt) = login(&srv, "Tess").await;

    // The email is best-effort; the triage row is not. Losing the report because
    // the mail provider is down would be the worse of the two failures.
    // Unique text: the test database is shared, so a count on a fixed string
    // would be counting other tests' rows too.
    let message = format!("Something broke ({}).", Uuid::new_v4());
    let r = http
        .post(format!("{}/api/bug-report", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "message": message }))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "got {}", r.status());

    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM bug_reports WHERE message = $1")
        .bind(&message)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert_eq!(stored, 1);
}

// ---------------------------------------------------------------------------
// The Business contact form
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_contact_enquiry_is_delivered_to_sales() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();

    // Unauthenticated by design: this is the form on the marketing site.
    let r = http
        .post(format!("{}/api/contact", base(&srv)))
        .json(&json!({
            "name": "Ada Lovelace",
            "email": "ada@example.com",
            "company": "Analytical Engines",
            "message": "We run multilingual all-hands for 400 people.",
        }))
        .send()
        .await
        .unwrap();

    assert!(r.status().is_success(), "got {}", r.status());
    let sent = srv.mail.messages();
    assert_eq!(sent.len(), 1);
    // The enquiry goes to the support inbox (SUPPORT_EMAIL), not to whoever
    // receives bug reports.
    assert_eq!(recipients(&sent[0]).len(), 1);
    assert!(recipients(&sent[0])[0].contains('@'));
    assert!(sent[0]["subject"]
        .as_str()
        .unwrap_or_default()
        .contains("Ada Lovelace"));
}

#[tokio::test]
async fn a_contact_enquiry_with_no_usable_address_is_refused() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();

    for email in ["", "not-an-address", "a b@example.com"] {
        let r = http
            .post(format!("{}/api/contact", base(&srv)))
            .json(&json!({ "name": "Ada", "email": email, "message": "Hello" }))
            .send()
            .await
            .unwrap();
        assert!(
            !r.status().is_success(),
            "{email:?} should be refused, got {}",
            r.status()
        );
    }
    assert!(srv.mail.messages().is_empty());
}

#[tokio::test]
async fn a_contact_enquiry_with_nothing_in_it_is_refused() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();

    let r = http
        .post(format!("{}/api/contact", base(&srv)))
        .json(&json!({ "name": "Ada", "email": "ada@example.com", "message": "  " }))
        .send()
        .await
        .unwrap();
    assert!(!r.status().is_success(), "got {}", r.status());
}

#[tokio::test]
async fn the_contact_form_is_throttled() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();

    // An unauthenticated endpoint that sends mail has to have a ceiling.
    let mut throttled = false;
    for i in 0..20 {
        let r = http
            .post(format!("{}/api/contact", base(&srv)))
            .json(&json!({
                "name": "Ada",
                "email": format!("ada{i}@example.com"),
                "message": "Hello",
            }))
            .send()
            .await
            .unwrap();
        if r.status() == 429 {
            throttled = true;
            break;
        }
    }
    assert!(throttled, "the form must not be an open relay");
}

// ---------------------------------------------------------------------------
// Room invites
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_invite_is_sent_to_each_address() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (_, jwt) = login(&srv, "Tess").await;

    let r = http
        .post(format!("{}/api/rooms/standup/invite", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "emails": ["one@example.com", "two@example.com"] }))
        .send()
        .await
        .unwrap();

    assert!(r.status().is_success(), "got {}", r.status());
    let sent = srv.mail.messages();
    assert!(!sent.is_empty(), "nothing was sent");
    let all: Vec<String> = sent.iter().flat_map(recipients).collect();
    assert!(all.iter().any(|a| a == "one@example.com"));
    assert!(all.iter().any(|a| a == "two@example.com"));
}

#[tokio::test]
async fn an_invite_carries_a_link_into_the_room() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (_, jwt) = login(&srv, "Tess").await;

    http.post(format!("{}/api/rooms/standup/invite", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "emails": ["one@example.com"] }))
        .send()
        .await
        .unwrap();

    let sent = srv.mail.messages();
    let body = format!("{} {}", sent[0]["html"], sent[0]["text"]);
    assert!(
        body.contains("standup"),
        "an invite with no way in is not an invite: {body}"
    );
}

#[tokio::test]
async fn an_invite_needs_a_signed_in_caller() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();

    let r = http
        .post(format!("{}/api/rooms/standup/invite", base(&srv)))
        .json(&json!({ "emails": ["one@example.com"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    assert!(srv.mail.messages().is_empty());
}

#[tokio::test]
async fn an_invite_to_nobody_is_refused() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (_, jwt) = login(&srv, "Tess").await;

    let r = http
        .post(format!("{}/api/rooms/standup/invite", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "emails": [] }))
        .send()
        .await
        .unwrap();
    assert!(!r.status().is_success(), "got {}", r.status());
    assert!(srv.mail.messages().is_empty());
}

#[tokio::test]
async fn an_invite_refuses_an_address_that_cannot_be_one() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (_, jwt) = login(&srv, "Tess").await;

    let r = http
        .post(format!("{}/api/rooms/standup/invite", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "emails": ["not-an-address"] }))
        .send()
        .await
        .unwrap();
    assert!(!r.status().is_success(), "got {}", r.status());
}

#[tokio::test]
async fn a_flood_of_invites_is_throttled() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (_, jwt) = login(&srv, "Tess").await;

    // Protects the sending domain's reputation from one enthusiastic user.
    let mut throttled = false;
    for i in 0..20 {
        let r = http
            .post(format!("{}/api/rooms/standup/invite", base(&srv)))
            .bearer_auth(&jwt)
            .json(&json!({ "emails": [format!("guest{i}@example.com")] }))
            .send()
            .await
            .unwrap();
        if r.status() == 429 {
            throttled = true;
            break;
        }
    }
    assert!(throttled);
}

#[tokio::test]
async fn without_a_mail_provider_invites_say_so() {
    let srv = skip_without_db!(setup_with(false).await);
    let http = Client::new();
    let (_, jwt) = login(&srv, "Tess").await;

    let r = http
        .post(format!("{}/api/rooms/standup/invite", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "emails": ["one@example.com"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
}

// ---------------------------------------------------------------------------
// The call recap
// ---------------------------------------------------------------------------

async fn draft_for(http: &Client, srv: &Server, session_id: Uuid, jwt: &str) -> String {
    let r = http
        .post(format!(
            "{}/api/sessions/{session_id}/email-draft",
            base(srv)
        ))
        .bearer_auth(jwt)
        .json(&json!({
            "recipients": [{ "kind": "email", "email": "guest@example.com" }],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202, "draft should be accepted");
    let job_id = r.json::<Value>().await.unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string();

    let url = format!("{}/api/sessions/{session_id}/ai-job/{job_id}", base(srv));
    for _ in 0..100 {
        let job: Value = http
            .get(&url)
            .bearer_auth(jwt)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if job["status"] != "pending" {
            assert_eq!(job["status"], "done", "job: {job}");
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let latest: Value = http
        .get(format!("{}/api/sessions/{session_id}/email", base(srv)))
        .bearer_auth(jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    latest["id"].as_str().expect("draft has an id").to_string()
}

#[tokio::test]
async fn a_call_recap_is_drafted_and_then_actually_sent() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess").await;
    let session_id = seeded_session(&srv, uid).await;
    let email_id = draft_for(&http, &srv, session_id, &jwt).await;

    let sent = http
        .post(format!(
            "{}/api/sessions/{session_id}/email-send",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .json(&json!({ "email_id": email_id }))
        .send()
        .await
        .unwrap();

    assert!(sent.status().is_success(), "got {}", sent.status());
    let messages = srv.mail.messages();
    assert_eq!(messages.len(), 1);
    assert!(recipients(&messages[0]).contains(&"guest@example.com".to_string()));
    assert_eq!(messages[0]["subject"], "Recap of our call");
}

#[tokio::test]
async fn a_recap_cannot_be_sent_twice() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess").await;
    let session_id = seeded_session(&srv, uid).await;
    let email_id = draft_for(&http, &srv, session_id, &jwt).await;
    let url = format!("{}/api/sessions/{session_id}/email-send", base(&srv));

    let first = http
        .post(&url)
        .bearer_auth(&jwt)
        .json(&json!({ "email_id": email_id }))
        .send()
        .await
        .unwrap();
    assert!(first.status().is_success());

    let again = http
        .post(&url)
        .bearer_auth(&jwt)
        .json(&json!({ "email_id": email_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 409, "a sent draft is not a draft any more");
    assert_eq!(srv.mail.messages().len(), 1);
}

#[tokio::test]
async fn a_pre_send_edit_replaces_the_subject_and_body() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess").await;
    let session_id = seeded_session(&srv, uid).await;
    let email_id = draft_for(&http, &srv, session_id, &jwt).await;

    http.post(format!(
        "{}/api/sessions/{session_id}/email-send",
        base(&srv)
    ))
    .bearer_auth(&jwt)
    .json(&json!({
        "email_id": email_id,
        "subject": "Notes from today",
        "body_text": "Shipping Friday. — Tess",
    }))
    .send()
    .await
    .unwrap();

    let messages = srv.mail.messages();
    assert_eq!(messages[0]["subject"], "Notes from today");
    assert!(messages[0]["text"]
        .as_str()
        .unwrap()
        .contains("Shipping Friday"));
    // The HTML is always rebuilt from the edited text, never left stale.
    assert!(messages[0]["html"]
        .as_str()
        .unwrap()
        .contains("Shipping Friday"));
}

#[tokio::test]
async fn a_provider_refusal_leaves_the_draft_unsent_rather_than_marked_sent() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess").await;
    let session_id = seeded_session(&srv, uid).await;
    let email_id = draft_for(&http, &srv, session_id, &jwt).await;
    srv.mail.fail_status.store(422, Ordering::SeqCst);

    let r = http
        .post(format!(
            "{}/api/sessions/{session_id}/email-send",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .json(&json!({ "email_id": email_id }))
        .send()
        .await
        .unwrap();
    assert!(!r.status().is_success(), "got {}", r.status());

    let status: String = sqlx::query_scalar("SELECT status FROM session_emails WHERE id = $1")
        .bind(Uuid::parse_str(&email_id).unwrap())
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert_ne!(status, "sent", "a refused send must stay retryable");
}
