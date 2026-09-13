//! The paid AI features over a stand-in Groq.
//!
//! Report, quiz, sentiment, correction and email draft all funnel through one
//! `Groq::chat` call, so a single endpoint override (`GROQ_BASE_URL`, see
//! `config::Config::groq_base_url`) is what lets them run at all without a live
//! key — until now every one of these handlers stopped at the provider call.
//!
//! The mock answers JSON-mode requests with a superset object carrying the keys
//! each consumer reads (`questions`, `score`/`speakers`, `lines`,
//! `subject`/`body_text`) and plain-text requests with markdown, so one stand-in
//! serves every feature without pretending to be a model.
//!
//! DB-gated: skipped without `DATABASE_URL`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
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
use voxtranslate_server::config::Config;
use voxtranslate_server::db::{self, Pool};
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::transcripts::{EventKind, TranscriptEvent, TranscriptService};
use voxtranslate_server::{app, AppState};

const SECRET: &str = "ai-features-secret";

// ---------------------------------------------------------------------------
// A stand-in for Groq
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct MockGroq {
    calls: Arc<AtomicUsize>,
    /// When non-zero, the next N calls answer with this status instead.
    fail_status: Arc<AtomicU32>,
    /// Answer 200 with an empty completion — what a reasoning model does when it
    /// spends its whole budget on reasoning.
    empty_completion: Arc<AtomicU32>,
}

/// Everything the JSON-mode consumers read, in one object. Each reads only the
/// keys it knows, so one answer serves quiz, sentiment, correction and email.
fn json_superset() -> String {
    json!({
        "questions": (0..10).map(|i| json!({
            "q": format!("Question {i}?"),
            "options": ["alpha", "beta", "gamma", "delta"],
            "answer": 0,
        })).collect::<Vec<_>>(),
        "score": 0.4,
        "speakers": { "Tess": 0.4, "Rao": -0.2 },
        "lines": (0..20).map(|i| json!({ "i": i, "text": format!("corrected line {i}") }))
            .collect::<Vec<_>>(),
        "subject": "Recap of our call",
        "body_text": "Here is what we agreed.",
        "body_html": "<p>Here is what we agreed.</p>",
    })
    .to_string()
}

async fn mock_completions(
    State(mock): State<MockGroq>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    mock.calls.fetch_add(1, Ordering::SeqCst);

    let failures = mock.fail_status.load(Ordering::SeqCst);
    if failures != 0 {
        let status = StatusCode::from_u16(failures as u16).unwrap();
        return (status, "upstream said no").into_response();
    }

    let content = if mock.empty_completion.load(Ordering::SeqCst) == 1 {
        String::new()
    } else if body["response_format"]["type"] == "json_object" {
        json_superset()
    } else {
        "## Summary\n\nThe call went well.\n\n- One point\n- Another point".to_string()
    };

    Json(json!({
        "choices": [{ "message": { "role": "assistant", "content": content } }],
    }))
    .into_response()
}

async fn mock_groq() -> (String, MockGroq) {
    let mock = MockGroq::default();
    let router = Router::new()
        .route("/chat/completions", post(mock_completions))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{addr}/chat/completions"), mock)
}

// ---------------------------------------------------------------------------
// Server harness
// ---------------------------------------------------------------------------

struct Server {
    addr: SocketAddr,
    pool: Pool,
    groq: MockGroq,
}

async fn setup() -> Option<Server> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;

    let (groq_url, groq) = mock_groq().await;
    let mut config = Config::test_with_billing(&url, SECRET, 5.0);
    config.groq_base_url = Some(groq_url);
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
    Some(Server { addr, pool, groq })
}

fn base(srv: &Server) -> String {
    format!("http://{}", srv.addr)
}

/// A signed-in user with enough credit for the paid features.
async fn login(srv: &Server, name: &str, credits: i64) -> (Uuid, String) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: name.into(),
        avatar_url: None,
    };
    let (user, _) = upsert_google_user(
        &srv.pool,
        &identity,
        rust_decimal::Decimal::new(credits, 2),
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

/// Seed a finished session with a handful of speech events.
async fn seeded_session(srv: &Server, uid: Uuid, name: &str) -> Uuid {
    let session_id = Uuid::new_v4();
    let room = format!("ai-{}", Uuid::new_v4().simple());
    let svc = TranscriptService::new(srv.pool.clone());
    svc.session_started(session_id, &room).await.unwrap();
    svc.participant_joined(session_id, "peer-1", Some(uid), name, "it")
        .await
        .unwrap();
    for (i, text) in [
        "Hello everyone, thanks for joining.",
        "The migration is on track for next week.",
        "One risk is the database downtime window.",
        "Let us agree to meet again on Friday.",
    ]
    .iter()
    .enumerate()
    {
        svc.record(TranscriptEvent {
            session_id,
            kind: EventKind::Speech,
            speaker_peer_id: "peer-1".into(),
            speaker_user_id: Some(uid),
            speaker_name: name.into(),
            original_text: (*text).into(),
            original_lang: "en".into(),
            translations: HashMap::from([("it".to_string(), format!("riga {i}"))]),
            ts: chrono::Utc::now() + chrono::Duration::seconds(i as i64 * 10),
        });
    }
    svc.flush().await;
    sqlx::query("UPDATE call_sessions SET ended_at = now() WHERE id = $1")
        .bind(session_id)
        .execute(&srv.pool)
        .await
        .unwrap();
    session_id
}

/// Poll a background AI job until it leaves `pending`.
async fn poll_job(http: &Client, srv: &Server, session_id: Uuid, job_id: &str, jwt: &str) -> Value {
    let url = format!("{}/api/sessions/{session_id}/ai-job/{job_id}", base(srv));
    for _ in 0..100 {
        let r = http.get(&url).bearer_auth(jwt).send().await.unwrap();
        assert_eq!(r.status(), 200, "an ai job must stay readable");
        let job: Value = r.json().await.unwrap();
        if job["status"] != "pending" {
            return job;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("ai job {job_id} never left pending");
}

macro_rules! skip_without_db {
    () => {
        match setup().await {
            Some(srv) => srv,
            None => {
                eprintln!("skipping — no DATABASE_URL");
                return;
            }
        }
    };
}

async fn balance(srv: &Server, uid: Uuid) -> f64 {
    let b: rust_decimal::Decimal =
        sqlx::query_scalar("SELECT balance FROM users WHERE id = $1")
            .bind(uid)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    b.to_string().parse().unwrap()
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_report_is_generated_and_charged() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess", 500).await;
    let session_id = seeded_session(&srv, uid, "Tess").await;
    let before = balance(&srv, uid).await;

    let r = http
        .post(format!("{}/api/sessions/{session_id}/report", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "format": "structured", "lang": "en" }))
        .send()
        .await
        .unwrap();

    assert_eq!(r.status(), 202, "the heavy call returns a job handle");
    let job_id = r.json::<Value>().await.unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    let job = poll_job(&http, &srv, session_id, &job_id, &jwt).await;

    assert_eq!(job["status"], "done", "job: {job}");
    assert!(srv.groq.calls.load(Ordering::SeqCst) > 0);
    assert!(
        balance(&srv, uid).await < before,
        "a produced report is paid for"
    );
}

#[tokio::test]
async fn a_report_can_be_read_back_after_it_is_generated() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess", 500).await;
    let session_id = seeded_session(&srv, uid, "Tess").await;

    let r = http
        .post(format!("{}/api/sessions/{session_id}/report", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let job_id = r.json::<Value>().await.unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    poll_job(&http, &srv, session_id, &job_id, &jwt).await;

    let latest = http
        .get(format!("{}/api/sessions/{session_id}/report", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(latest.status(), 200);
    let body: Value = latest.json().await.unwrap();
    assert!(body["markdown"].as_str().is_some_and(|m| !m.is_empty()));
}

#[tokio::test]
async fn a_report_refuses_a_format_it_does_not_have() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess", 500).await;
    let session_id = seeded_session(&srv, uid, "Tess").await;

    let r = http
        .post(format!("{}/api/sessions/{session_id}/report", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "format": "powerpoint" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(srv.groq.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_report_refuses_guidelines_long_enough_to_be_a_payload() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess", 500).await;
    let session_id = seeded_session(&srv, uid, "Tess").await;

    let r = http
        .post(format!("{}/api/sessions/{session_id}/report", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "guidelines": "x".repeat(2001) }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn a_report_refuses_a_language_tag_that_cannot_be_one() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess", 500).await;
    let session_id = seeded_session(&srv, uid, "Tess").await;

    for lang in ["en_US!", "averylonglang"] {
        let r = http
            .post(format!("{}/api/sessions/{session_id}/report", base(&srv)))
            .bearer_auth(&jwt)
            .json(&json!({ "lang": lang }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "lang {lang:?} should be refused");
    }
}

#[tokio::test]
async fn a_session_with_nothing_said_cannot_be_reported_on() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess", 500).await;
    let session_id = Uuid::new_v4();
    let svc = TranscriptService::new(srv.pool.clone());
    svc.session_started(session_id, "empty-room").await.unwrap();
    svc.participant_joined(session_id, "peer-1", Some(uid), "Tess", "it")
        .await
        .unwrap();
    svc.flush().await;

    let r = http
        .post(format!("{}/api/sessions/{session_id}/report", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422);
    assert_eq!(srv.groq.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_user_who_cannot_pay_is_told_before_the_provider_is_called() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess", 0).await;
    let session_id = seeded_session(&srv, uid, "Tess").await;

    let r = http
        .post(format!("{}/api/sessions/{session_id}/report", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({}))
        .send()
        .await
        .unwrap();

    assert_eq!(r.status(), 402);
    assert_eq!(
        srv.groq.calls.load(Ordering::SeqCst),
        0,
        "an expensive call must not be burned on a request that cannot be paid for"
    );
}

#[tokio::test]
async fn a_session_that_is_not_yours_cannot_be_reported_on() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (uid, _) = login(&srv, "Tess", 500).await;
    let (_, outsider) = login(&srv, "Rao", 500).await;
    let session_id = seeded_session(&srv, uid, "Tess").await;

    let r = http
        .post(format!("{}/api/sessions/{session_id}/report", base(&srv)))
        .bearer_auth(&outsider)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert!(
        r.status() == 403 || r.status() == 404,
        "an outsider must not reach someone else's call, got {}",
        r.status()
    );
}

#[tokio::test]
async fn a_provider_failure_leaves_the_job_failed_and_refunds_the_charge() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess", 500).await;
    let session_id = seeded_session(&srv, uid, "Tess").await;
    let before = balance(&srv, uid).await;
    srv.groq.fail_status.store(400, Ordering::SeqCst);

    let r = http
        .post(format!("{}/api/sessions/{session_id}/report", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    let job_id = r.json::<Value>().await.unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    let job = poll_job(&http, &srv, session_id, &job_id, &jwt).await;

    assert_eq!(job["status"], "failed", "job: {job}");
    assert_eq!(
        balance(&srv, uid).await,
        before,
        "a report that was never produced must not be paid for"
    );
}

#[tokio::test]
async fn an_empty_completion_is_retried_rather_than_failing_outright() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess", 500).await;
    let session_id = seeded_session(&srv, uid, "Tess").await;
    srv.groq.empty_completion.store(1, Ordering::SeqCst);

    let r = http
        .post(format!("{}/api/sessions/{session_id}/report", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let job_id = r.json::<Value>().await.unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    poll_job(&http, &srv, session_id, &job_id, &jwt).await;

    // A reasoning model that spends its budget on reasoning returns nothing
    // visible; that is transient, so the client retries before giving up.
    assert!(
        srv.groq.calls.load(Ordering::SeqCst) > 1,
        "an empty completion should have been retried"
    );
}

// ---------------------------------------------------------------------------
// Sentiment
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sentiment_is_produced_over_the_transcript() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess", 500).await;
    let session_id = seeded_session(&srv, uid, "Tess").await;

    let r = http
        .post(format!("{}/api/sessions/{session_id}/sentiment", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({}))
        .send()
        .await
        .unwrap();

    assert!(
        r.status() == 200 || r.status() == 202,
        "unexpected status {}",
        r.status()
    );
    let body: Value = r.json().await.unwrap();
    if let Some(job_id) = body["job_id"].as_str() {
        let job = poll_job(&http, &srv, session_id, job_id, &jwt).await;
        assert_eq!(job["status"], "done", "job: {job}");
    }

    let latest = http
        .get(format!("{}/api/sessions/{session_id}/sentiment", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(latest.status(), 200);
}

// ---------------------------------------------------------------------------
// Quiz
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_quiz_is_generated_from_a_topic() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (_, jwt) = login(&srv, "Tess", 500).await;

    let r = http
        .post(format!("{}/api/quiz/generate", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "prompt": "database migrations", "count": 5, "langs": ["en"] }))
        .send()
        .await
        .unwrap();

    assert!(
        r.status().is_success(),
        "quiz generation failed: {} {}",
        r.status(),
        r.text().await.unwrap_or_default()
    );
    assert!(srv.groq.calls.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn a_quiz_needs_a_topic() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (_, jwt) = login(&srv, "Tess", 500).await;

    let r = http
        .post(format!("{}/api/quiz/generate", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "prompt": "", "count": 5 }))
        .send()
        .await
        .unwrap();
    // 422, not 400: the request parsed fine, it just asks for nothing.
    assert_eq!(r.status(), 422);
}

// ---------------------------------------------------------------------------
// Pricing — the one AI endpoint that needs no provider at all
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ai_pricing_is_published_without_a_session() {
    let srv = skip_without_db!();
    let http = Client::new();

    let r = http
        .get(format!("{}/api/billing/ai-pricing", base(&srv)))
        .send()
        .await
        .unwrap();

    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body.is_object(), "pricing should be an object: {body}");
}
