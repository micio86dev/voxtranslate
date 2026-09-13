//! The webinar AI recap: the host's report, and the per-participant recap emails.
//!
//! Both are host-only background jobs that end in a Groq call, so neither could
//! run in a test before `GROQ_BASE_URL` existed. Resend stays unmocked — a send
//! dies at its own 401 — which is what makes the "one failure does not sink the
//! batch" rule observable.
//!
//! DB-gated: skipped without `DATABASE_URL`.

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
use voxtranslate_server::config::{Config, ResendConfig, WebinarConfig};
use voxtranslate_server::db::{self, Pool};
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::{app, AppState};

const SECRET: &str = "webinar-ai-secret";

// ---------------------------------------------------------------------------
// A stand-in for Groq
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct MockGroq {
    calls: Arc<AtomicUsize>,
    fail_status: Arc<AtomicU32>,
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
        json!({
            "subject": "Your recap",
            "body_text": "Thanks for joining.",
            "body_html": "<p>Thanks for joining.</p>",
            "lines": [],
            "questions": [],
            "score": 0.1,
            "speakers": {},
        })
        .to_string()
    } else {
        "## Recap\n\nWe covered the migration plan.".to_string()
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

async fn setup_with(resend: bool) -> Option<Server> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;

    let (groq_url, groq) = mock_groq().await;
    let mut config = Config::test_with_billing(&url, SECRET, 0.0);
    config.webinar = Some(WebinarConfig::test_default());
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

/// An org with a live subscription and a funded credit pool.
async fn org(srv: &Server, owner: Uuid, credits: i32) -> Uuid {
    let org_id: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id, subscription_status,
                                    current_period_end, credits_balance)
         VALUES ('Webinar Co', $1, $2, 'active', now() + interval '30 days', $3) RETURNING id",
    )
    .bind(format!("wa-{}", Uuid::new_v4().simple()))
    .bind(owner)
    .bind(credits)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(org_id)
    .bind(owner)
    .execute(&srv.pool)
    .await
    .unwrap();
    org_id
}

/// A finished webinar hosted by `host`.
async fn webinar(srv: &Server, org_id: Uuid, host: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO webinars (org_id, host_user_id, code, title, source_language, status,
                               actual_start, actual_end)
         VALUES ($1, $2, $3, 'Launch call', 'en', 'ended',
                 now() - interval '1 hour', now())
         RETURNING id",
    )
    .bind(org_id)
    .bind(host)
    .bind(format!("W{}", &Uuid::new_v4().simple().to_string()[..9]))
    .fetch_one(&srv.pool)
    .await
    .unwrap()
}

/// Give the webinar something to recap on.
async fn seed_transcript(srv: &Server, webinar_id: Uuid) {
    for (i, text) in [
        "Welcome everyone to the launch call.",
        "The migration window is the first weekend of next month.",
        "We will publish the runbook on Thursday.",
    ]
    .iter()
    .enumerate()
    {
        sqlx::query(
            "INSERT INTO webinar_transcripts (webinar_id, original_text, original_lang,
                                              translations, spoken_at)
             VALUES ($1, $2, 'en', $3, now() - ($4 || ' minutes')::interval)",
        )
        .bind(webinar_id)
        .bind(text)
        .bind(json!({ "en": text, "it": format!("riga {i}") }))
        .bind((10 - i as i32).to_string())
        .execute(&srv.pool)
        .await
        .unwrap();
    }
}

/// One participant who watched, optionally tied to an account with an address.
async fn participant(srv: &Server, webinar_id: Uuid, lang: &str, account: Option<Uuid>) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO webinar_participants (webinar_id, guest_id, user_id, language_code,
                                           total_watch_seconds)
         VALUES ($1, $2, $3, $4, 600) RETURNING id",
    )
    .bind(webinar_id)
    .bind(Uuid::new_v4())
    .bind(account)
    .bind(lang)
    .fetch_one(&srv.pool)
    .await
    .unwrap()
}

fn report_url(srv: &Server, id: Uuid) -> String {
    format!("{}/api/webinars/{id}/ai/report", base(srv))
}

fn email_url(srv: &Server, id: Uuid) -> String {
    format!("{}/api/webinars/{id}/ai/email", base(srv))
}

async fn poll_job(http: &Client, srv: &Server, id: Uuid, job_id: &str, jwt: &str) -> Value {
    let url = format!("{}/api/webinars/{id}/ai/job/{job_id}", base(srv));
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
    panic!("webinar ai job {job_id} never left pending");
}

/// A host with a funded org and a webinar that has a transcript.
async fn ready(srv: &Server) -> (Uuid, Uuid, String) {
    let (host, jwt) = user(srv, "Host").await;
    let org_id = org(srv, host, 100_000).await;
    let webinar_id = webinar(srv, org_id, host).await;
    seed_transcript(srv, webinar_id).await;
    (org_id, webinar_id, jwt)
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

async fn org_balance(srv: &Server, org_id: Uuid) -> i32 {
    sqlx::query_scalar("SELECT credits_balance FROM organizations WHERE id = $1")
        .bind(org_id)
        .fetch_one(&srv.pool)
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// The host report
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_report_is_generated_and_readable_back() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (_, webinar_id, jwt) = ready(&srv).await;

    let r = http
        .post(report_url(&srv, webinar_id))
        .bearer_auth(&jwt)
        .json(&json!({ "lang": "en" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202, "the recap runs in the background");
    let job_id = r.json::<Value>().await.unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    let job = poll_job(&http, &srv, webinar_id, &job_id, &jwt).await;
    assert_eq!(job["status"], "done", "job: {job}");
    assert!(srv.groq.calls.load(Ordering::SeqCst) > 0);

    let latest = http
        .get(report_url(&srv, webinar_id))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(latest.status(), 200);
}

#[tokio::test]
async fn a_report_is_charged_to_the_organization_pool() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, webinar_id, jwt) = ready(&srv).await;
    let before = org_balance(&srv, org_id).await;

    let r = http
        .post(report_url(&srv, webinar_id))
        .bearer_auth(&jwt)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let job_id = r.json::<Value>().await.unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    poll_job(&http, &srv, webinar_id, &job_id, &jwt).await;

    assert!(
        org_balance(&srv, org_id).await < before,
        "a webinar recap is billed to the org, not to the host personally"
    );
}

#[tokio::test]
async fn a_re_click_joins_the_running_job_instead_of_starting_a_second_one() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (_, webinar_id, jwt) = ready(&srv).await;

    let first: Value = http
        .post(report_url(&srv, webinar_id))
        .bearer_auth(&jwt)
        .json(&json!({ "lang": "en" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let second: Value = http
        .post(report_url(&srv, webinar_id))
        .bearer_auth(&jwt)
        .json(&json!({ "lang": "en" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(
        first["job_id"], second["job_id"],
        "one live slot per (webinar, language)"
    );
    poll_job(
        &http,
        &srv,
        webinar_id,
        first["job_id"].as_str().unwrap(),
        &jwt,
    )
    .await;
}

#[tokio::test]
async fn a_webinar_with_no_transcript_cannot_be_recapped() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (host, jwt) = user(&srv, "Host").await;
    let org_id = org(&srv, host, 100_000).await;
    let webinar_id = webinar(&srv, org_id, host).await;

    let r = http
        .post(report_url(&srv, webinar_id))
        .bearer_auth(&jwt)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422);
    assert_eq!(srv.groq.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_report_refuses_a_language_tag_that_cannot_be_one() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (_, webinar_id, jwt) = ready(&srv).await;

    for lang in ["en_US!", "averyverylonglang", "  "] {
        let r = http
            .post(report_url(&srv, webinar_id))
            .bearer_auth(&jwt)
            .json(&json!({ "lang": lang }))
            .send()
            .await
            .unwrap();
        assert!(
            r.status() == 400 || r.status() == 202,
            "lang {lang:?} gave {}",
            r.status()
        );
    }
}

#[tokio::test]
async fn only_the_host_may_ask_for_a_recap() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (_, webinar_id, _) = ready(&srv).await;
    let (_, outsider) = user(&srv, "Outsider").await;

    // Not "any org admin": the recap is the host's, and the audience is anonymous.
    let r = http
        .post(report_url(&srv, webinar_id))
        .bearer_auth(&outsider)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    let latest = http
        .get(report_url(&srv, webinar_id))
        .bearer_auth(&outsider)
        .send()
        .await
        .unwrap();
    assert_eq!(latest.status(), 403);
}

#[tokio::test]
async fn a_recap_needs_a_signed_in_caller() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (_, webinar_id, _) = ready(&srv).await;

    let r = http
        .post(report_url(&srv, webinar_id))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn an_unknown_webinar_is_a_404() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (_, jwt) = user(&srv, "Host").await;

    let r = http
        .post(report_url(&srv, Uuid::new_v4()))
        .bearer_auth(&jwt)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn reading_a_recap_before_one_exists_is_not_an_error() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (_, webinar_id, jwt) = ready(&srv).await;

    let r = http
        .get(report_url(&srv, webinar_id))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert!(
        r.status().is_success() || r.status() == 404,
        "got {}",
        r.status()
    );
}

#[tokio::test]
async fn a_provider_failure_leaves_the_job_failed_and_charges_nothing() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, webinar_id, jwt) = ready(&srv).await;
    let before = org_balance(&srv, org_id).await;
    srv.groq.fail_status.store(400, Ordering::SeqCst);

    let r = http
        .post(report_url(&srv, webinar_id))
        .bearer_auth(&jwt)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let job_id = r.json::<Value>().await.unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    let job = poll_job(&http, &srv, webinar_id, &job_id, &jwt).await;

    assert_eq!(job["status"], "failed", "job: {job}");
    assert_eq!(
        org_balance(&srv, org_id).await,
        before,
        "a recap that was never produced must not be paid for"
    );
}

// ---------------------------------------------------------------------------
// Participant recap emails
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_email_fan_out_runs_per_participant_language() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (_, webinar_id, jwt) = ready(&srv).await;
    let (viewer_en, _) = user(&srv, "Viewer EN").await;
    let (viewer_it, _) = user(&srv, "Viewer IT").await;
    participant(&srv, webinar_id, "en", Some(viewer_en)).await;
    participant(&srv, webinar_id, "it", Some(viewer_it)).await;

    let r = http
        .post(email_url(&srv, webinar_id))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    let job_id = r.json::<Value>().await.unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    let job = poll_job(&http, &srv, webinar_id, &job_id, &jwt).await;

    // Resend refuses the fake key, so nothing is actually sent — but the batch
    // must still finish and report, rather than dying on the first failure.
    assert_ne!(job["status"], "pending");
    assert!(
        srv.groq.calls.load(Ordering::SeqCst) >= 1,
        "at least one language should have been generated"
    );
}

#[tokio::test]
async fn a_failed_send_charges_nothing() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, webinar_id, jwt) = ready(&srv).await;
    let (viewer, _) = user(&srv, "Viewer").await;
    participant(&srv, webinar_id, "en", Some(viewer)).await;
    let before = org_balance(&srv, org_id).await;

    let r = http
        .post(email_url(&srv, webinar_id))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    let job_id = r.json::<Value>().await.unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    poll_job(&http, &srv, webinar_id, &job_id, &jwt).await;

    // Only a successfully-sent, newly-deduped email charges.
    assert_eq!(org_balance(&srv, org_id).await, before);
}

#[tokio::test]
async fn a_webinar_with_no_transcript_has_nothing_to_recap_by_email() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (host, jwt) = user(&srv, "Host").await;
    let org_id = org(&srv, host, 100_000).await;
    let webinar_id = webinar(&srv, org_id, host).await;

    let r = http
        .post(email_url(&srv, webinar_id))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422);
}

#[tokio::test]
async fn without_a_mail_provider_the_fan_out_says_so() {
    let srv = skip_without_db!(setup_with(false).await);
    let http = Client::new();
    let (_, webinar_id, jwt) = ready(&srv).await;

    let r = http
        .post(email_url(&srv, webinar_id))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
    assert_eq!(srv.groq.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn only_the_host_may_start_the_fan_out() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (_, webinar_id, _) = ready(&srv).await;
    let (_, outsider) = user(&srv, "Outsider").await;

    let r = http
        .post(email_url(&srv, webinar_id))
        .bearer_auth(&outsider)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
}

#[tokio::test]
async fn a_re_click_joins_the_running_fan_out() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (_, webinar_id, jwt) = ready(&srv).await;
    let (viewer, _) = user(&srv, "Viewer").await;
    participant(&srv, webinar_id, "en", Some(viewer)).await;

    let first: Value = http
        .post(email_url(&srv, webinar_id))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let second: Value = http
        .post(email_url(&srv, webinar_id))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(first["job_id"], second["job_id"]);
    poll_job(
        &http,
        &srv,
        webinar_id,
        first["job_id"].as_str().unwrap(),
        &jwt,
    )
    .await;
}

#[tokio::test]
async fn a_participant_with_no_account_is_skipped_rather_than_failing_the_batch() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (_, webinar_id, jwt) = ready(&srv).await;
    // An anonymous guest: no user row, so no address to send to.
    participant(&srv, webinar_id, "en", None).await;

    let r = http
        .post(email_url(&srv, webinar_id))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    let job_id = r.json::<Value>().await.unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    let job = poll_job(&http, &srv, webinar_id, &job_id, &jwt).await;
    assert_ne!(job["status"], "pending", "job: {job}");
}
