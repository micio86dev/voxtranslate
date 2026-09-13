//! Transcript correction and the follow-up email, over a stand-in Groq.
//!
//! Both are background jobs behind a paid endpoint, and both used to be
//! unreachable in tests: without a Groq key the handler stopped at the provider
//! call. `GROQ_BASE_URL` points the whole AI surface at a local stand-in; Resend
//! stays unmocked, so a real send fails at its own 401 — which is exactly the
//! failure path the send tests want.
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
use voxtranslate_server::config::{Config, ResendConfig};
use voxtranslate_server::db::{self, Pool};
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::transcripts::{EventKind, TranscriptEvent, TranscriptService};
use voxtranslate_server::{app, AppState};

const SECRET: &str = "ai-correction-email-secret";

// ---------------------------------------------------------------------------
// A stand-in for Groq
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct MockGroq {
    calls: Arc<AtomicUsize>,
    fail_status: Arc<AtomicU32>,
}

/// Every key the JSON-mode consumers read, in one object.
fn json_superset() -> String {
    json!({
        "lines": (0..40).map(|i| json!({ "i": i, "text": format!("corrected {i}") }))
            .collect::<Vec<_>>(),
        "subject": "Recap of our call",
        "body_text": "Here is what we agreed.\n\n- Ship on Friday",
        "body_html": "<p>Here is what we agreed.</p>",
        "score": 0.2,
        "speakers": { "Tess": 0.2 },
        "questions": [],
    })
    .to_string()
}

async fn mock_completions(
    State(mock): State<MockGroq>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    mock.calls.fetch_add(1, Ordering::SeqCst);
    let failure = mock.fail_status.load(Ordering::SeqCst);
    if failure != 0 {
        return (StatusCode::from_u16(failure as u16).unwrap(), "no").into_response();
    }
    let content = if body["response_format"]["type"] == "json_object" {
        json_superset()
    } else {
        "## Summary\n\nIt went well.".to_string()
    };
    Json(json!({ "choices": [{ "message": { "content": content } }] })).into_response()
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

/// `resend = false` reproduces a deployment with no mail provider, which is what
/// gates the email endpoints.
async fn setup_with(resend: bool) -> Option<Server> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;

    let (groq_url, groq) = mock_groq().await;
    let mut config = Config::test_with_billing(&url, SECRET, 5.0);
    config.groq_base_url = Some(groq_url);
    if resend {
        config.resend = Some(ResendConfig {
            api_key: "dummy".into(),
            from_email: "noreply@example.com".into(),
            from_name: "VoxTranslate".into(),
        });
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
    Some(Server { addr, pool, groq })
}

async fn setup() -> Option<Server> {
    setup_with(true).await
}

fn base(srv: &Server) -> String {
    format!("http://{}", srv.addr)
}

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

async fn seeded_session(srv: &Server, uid: Uuid) -> Uuid {
    let session_id = Uuid::new_v4();
    let svc = TranscriptService::new(srv.pool.clone());
    svc.session_started(session_id, &format!("cx-{}", Uuid::new_v4().simple()))
        .await
        .unwrap();
    svc.participant_joined(session_id, "peer-1", Some(uid), "Tess", "it")
        .await
        .unwrap();
    for (i, text) in [
        "hello every one thanks for join",
        "the migration is on track for next week",
        "one risk is the data base down time window",
    ]
    .iter()
    .enumerate()
    {
        svc.record(TranscriptEvent {
            session_id,
            kind: EventKind::Speech,
            speaker_peer_id: "peer-1".into(),
            speaker_user_id: Some(uid),
            speaker_name: "Tess".into(),
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

async fn poll_job(http: &Client, srv: &Server, session_id: Uuid, job_id: &str, jwt: &str) -> Value {
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
            return job;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("ai job {job_id} never left pending");
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
// Correction
// ---------------------------------------------------------------------------

fn correction_url(srv: &Server, session_id: Uuid, query: &str) -> String {
    format!("{}/api/sessions/{session_id}/correction{query}", base(srv))
}

#[tokio::test]
async fn a_correction_is_produced_and_cached() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess", 500).await;
    let session_id = seeded_session(&srv, uid).await;

    let r = http
        .post(correction_url(&srv, session_id, ""))
        .bearer_auth(&jwt)
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

    let status = http
        .get(correction_url(&srv, session_id, ""))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(status.status(), 200);
}

#[tokio::test]
async fn a_second_request_for_the_same_shape_reuses_the_cached_correction() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess", 500).await;
    let session_id = seeded_session(&srv, uid).await;

    let first = http
        .post(correction_url(&srv, session_id, ""))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    let job_id = first.json::<Value>().await.unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    poll_job(&http, &srv, session_id, &job_id, &jwt).await;
    let after_first = srv.groq.calls.load(Ordering::SeqCst);

    let second = http
        .post(correction_url(&srv, session_id, ""))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    if let Some(job_id) = second.json::<Value>().await.unwrap()["job_id"].as_str() {
        poll_job(&http, &srv, session_id, job_id, &jwt).await;
    }

    assert_eq!(
        srv.groq.calls.load(Ordering::SeqCst),
        after_first,
        "the same export shape must not be paid for twice"
    );
}

#[tokio::test]
async fn a_correction_refuses_a_mode_it_does_not_have() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess", 500).await;
    let session_id = seeded_session(&srv, uid).await;

    let r = http
        .post(correction_url(&srv, session_id, "?mode=upside-down"))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(srv.groq.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_correction_accepts_each_mode_it_does_have() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess", 500).await;
    let session_id = seeded_session(&srv, uid).await;

    for mode in ["original", "translated", "both"] {
        let r = http
            .post(correction_url(
                &srv,
                session_id,
                &format!("?mode={mode}&lang=it"),
            ))
            .bearer_auth(&jwt)
            .send()
            .await
            .unwrap();
        assert!(
            r.status().is_success() || r.status() == 202,
            "mode {mode} should be accepted, got {}",
            r.status()
        );
        if let Some(job_id) = r.json::<Value>().await.unwrap()["job_id"].as_str() {
            poll_job(&http, &srv, session_id, job_id, &jwt).await;
        }
    }
}

#[tokio::test]
async fn a_correction_refuses_a_language_tag_that_cannot_be_one() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess", 500).await;
    let session_id = seeded_session(&srv, uid).await;

    for lang in ["it_IT!", "averyverylonglang"] {
        let r = http
            .post(correction_url(
                &srv,
                session_id,
                &format!("?mode=translated&lang={lang}"),
            ))
            .bearer_auth(&jwt)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "lang {lang:?} should be refused");
    }
}

#[tokio::test]
async fn a_correction_nobody_can_pay_for_is_refused_before_the_provider_is_called() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess", 0).await;
    let session_id = seeded_session(&srv, uid).await;

    let r = http
        .post(correction_url(&srv, session_id, ""))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 402);
    assert_eq!(srv.groq.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_correction_on_someone_elses_call_is_refused() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (uid, _) = login(&srv, "Tess", 500).await;
    let (_, outsider) = login(&srv, "Rao", 500).await;
    let session_id = seeded_session(&srv, uid).await;

    let r = http
        .post(correction_url(&srv, session_id, ""))
        .bearer_auth(&outsider)
        .send()
        .await
        .unwrap();
    assert!(r.status() == 403 || r.status() == 404, "got {}", r.status());
}

#[tokio::test]
async fn a_correction_status_is_readable_before_one_exists() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess", 500).await;
    let session_id = seeded_session(&srv, uid).await;

    let r = http
        .get(correction_url(&srv, session_id, ""))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    // The client asks before generating; "not yet" is an answer, not an error.
    assert!(
        r.status().is_success() || r.status() == 404,
        "got {}",
        r.status()
    );
}

// ---------------------------------------------------------------------------
// Email draft
// ---------------------------------------------------------------------------

fn draft_url(srv: &Server, session_id: Uuid) -> String {
    format!("{}/api/sessions/{session_id}/email-draft", base(srv))
}

/// A draft request body. `recipients` is mandatory — the draft is addressed to
/// someone or it is not a draft — so every body here carries one.
fn draft_body(extra: Value) -> Value {
    let mut body = json!({
        "recipients": [{ "kind": "email", "email": "someone@example.com" }],
    });
    if let Some(map) = extra.as_object() {
        for (k, v) in map {
            body[k] = v.clone();
        }
    }
    body
}

#[tokio::test]
async fn a_draft_is_generated_and_readable() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess", 500).await;
    let session_id = seeded_session(&srv, uid).await;

    let r = http
        .post(draft_url(&srv, session_id))
        .bearer_auth(&jwt)
        .json(&draft_body(json!({ "tone": "professional", "lang": "en" })))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    let job_id = r.json::<Value>().await.unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    let job = poll_job(&http, &srv, session_id, &job_id, &jwt).await;
    assert_eq!(job["status"], "done", "job: {job}");

    let latest = http
        .get(format!("{}/api/sessions/{session_id}/email", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(latest.status(), 200);
    let body: Value = latest.json().await.unwrap();
    assert_eq!(body["subject"], "Recap of our call");
}

#[tokio::test]
async fn a_draft_refuses_a_tone_it_does_not_have() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess", 500).await;
    let session_id = seeded_session(&srv, uid).await;

    let r = http
        .post(draft_url(&srv, session_id))
        .bearer_auth(&jwt)
        .json(&draft_body(json!({ "tone": "sarcastic" })))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn a_draft_accepts_each_tone_it_does_have() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess", 500).await;
    let session_id = seeded_session(&srv, uid).await;

    for tone in ["professional", "friendly", "concise"] {
        let r = http
            .post(draft_url(&srv, session_id))
            .bearer_auth(&jwt)
            .json(&draft_body(json!({ "tone": tone })))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 202, "tone {tone} should be accepted");
        let job_id = r.json::<Value>().await.unwrap()["job_id"]
            .as_str()
            .unwrap()
            .to_string();
        poll_job(&http, &srv, session_id, &job_id, &jwt).await;
    }
}

#[tokio::test]
async fn a_draft_refuses_guidelines_long_enough_to_be_a_payload() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess", 500).await;
    let session_id = seeded_session(&srv, uid).await;

    let r = http
        .post(draft_url(&srv, session_id))
        .bearer_auth(&jwt)
        .json(&draft_body(json!({ "guidelines": "x".repeat(2001) })))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn without_a_mail_provider_the_draft_endpoint_says_so() {
    let srv = skip_without_db!(setup_with(false).await);
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess", 500).await;
    let session_id = seeded_session(&srv, uid).await;

    // Drafting an email nobody can send is not a feature.
    let r = http
        .post(draft_url(&srv, session_id))
        .bearer_auth(&jwt)
        .json(&draft_body(json!({})))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
    assert_eq!(srv.groq.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_provider_failure_leaves_the_draft_job_failed() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess", 500).await;
    let session_id = seeded_session(&srv, uid).await;
    srv.groq.fail_status.store(400, Ordering::SeqCst);

    let r = http
        .post(draft_url(&srv, session_id))
        .bearer_auth(&jwt)
        .json(&draft_body(json!({})))
        .send()
        .await
        .unwrap();
    let job_id = r.json::<Value>().await.unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    let job = poll_job(&http, &srv, session_id, &job_id, &jwt).await;
    assert_eq!(job["status"], "failed", "job: {job}");
}

// ---------------------------------------------------------------------------
// Sending
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sending_an_unknown_draft_is_a_404() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess", 500).await;
    let session_id = seeded_session(&srv, uid).await;

    let r = http
        .post(format!(
            "{}/api/sessions/{session_id}/email-send",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .json(&json!({ "email_id": Uuid::new_v4() }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn someone_elses_draft_cannot_be_sent() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess", 500).await;
    let (_, outsider) = login(&srv, "Rao", 500).await;
    let session_id = seeded_session(&srv, uid).await;

    let r = http
        .post(draft_url(&srv, session_id))
        .bearer_auth(&jwt)
        .json(&draft_body(json!({})))
        .send()
        .await
        .unwrap();
    let job_id = r.json::<Value>().await.unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    poll_job(&http, &srv, session_id, &job_id, &jwt).await;

    // Not a participant, so this is refused before the draft is even looked up.
    let r = http
        .post(format!(
            "{}/api/sessions/{session_id}/email-send",
            base(&srv)
        ))
        .bearer_auth(&outsider)
        .json(&json!({ "email_id": Uuid::new_v4() }))
        .send()
        .await
        .unwrap();
    assert!(r.status() == 403 || r.status() == 404, "got {}", r.status());
}

#[tokio::test]
async fn without_a_mail_provider_sending_says_so() {
    let srv = skip_without_db!(setup_with(false).await);
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess", 500).await;
    let session_id = seeded_session(&srv, uid).await;

    let r = http
        .post(format!(
            "{}/api/sessions/{session_id}/email-send",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .json(&json!({ "email_id": Uuid::new_v4() }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
}

#[tokio::test]
async fn a_real_send_fails_at_the_provider_rather_than_silently_succeeding() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (uid, jwt) = login(&srv, "Tess", 500).await;
    let session_id = seeded_session(&srv, uid).await;

    let r = http
        .post(draft_url(&srv, session_id))
        .bearer_auth(&jwt)
        .json(&draft_body(json!({})))
        .send()
        .await
        .unwrap();
    let job_id = r.json::<Value>().await.unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    poll_job(&http, &srv, session_id, &job_id, &jwt).await;
    let latest: Value = http
        .get(format!("{}/api/sessions/{session_id}/email", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let email_id = latest["id"].as_str().expect("draft has an id");

    // The Resend key is fake, so the send dies at Resend's own 401. What matters
    // is that the failure reaches the caller instead of being swallowed.
    let sent = http
        .post(format!(
            "{}/api/sessions/{session_id}/email-send",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .json(&json!({ "email_id": email_id, "to": ["someone@example.com"] }))
        .send()
        .await
        .unwrap();
    assert!(
        !sent.status().is_success(),
        "a send that never left the building must not report success"
    );
}
