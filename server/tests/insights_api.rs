//! The admin insights assistant — `POST …/organizations/{org_id}/insights`.
//!
//! `insights_scope.rs` asserts the retrieval predicate as SQL; this drives the
//! endpoint. Grounding needs a real embedding, so the tests stop at that call —
//! which still covers every gate before it: who may ask, what they may ask about,
//! what a malformed request costs (nothing), and that a failure after the charge
//! refunds it.
//!
//! DB-gated: skipped without `DATABASE_URL`.

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::Client;
use serde_json::{json, Value};
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::Config;
use voxtranslate_server::embeddings::OpenAiEmbeddings;
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::{app, db, AppState};

const SECRET: &str = "insights-api-secret";

struct Server {
    addr: SocketAddr,
    pool: db::Pool,
}

async fn setup_with(embeddings: bool) -> Option<Server> {
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
    if embeddings {
        state.embeddings = Some(OpenAiEmbeddings::new(
            "test-key".into(),
            "text-embedding-3-small".into(),
        ));
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    Some(Server { addr, pool })
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

/// An org with `credits` in its pool, owned by a fresh user.
async fn org(srv: &Server, owner: Uuid, credits: i32) -> Uuid {
    let org_id: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id, subscription_status,
                                    current_period_end, credits_balance)
         VALUES ('Insight Co', $1, $2, 'active', now() + interval '30 days', $3) RETURNING id",
    )
    .bind(format!("in-{}", Uuid::new_v4().simple()))
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

async fn member(srv: &Server, org_id: Uuid, role: &str, name: &str) -> (Uuid, String) {
    let (user_id, jwt) = user(srv, name).await;
    sqlx::query("INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, $3)")
        .bind(org_id)
        .bind(user_id)
        .bind(role)
        .execute(&srv.pool)
        .await
        .unwrap();
    (user_id, jwt)
}

/// A team led by `lead`, optionally containing `members`.
async fn team_led_by(srv: &Server, org_id: Uuid, lead: Uuid, members: &[Uuid]) -> Uuid {
    let team_id: Uuid = sqlx::query_scalar(
        "INSERT INTO teams (org_id, name, created_by) VALUES ($1, 'Nord', $2) RETURNING id",
    )
    .bind(org_id)
    .bind(lead)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO team_members (team_id, user_id, role) VALUES ($1, $2, 'lead')")
        .bind(team_id)
        .bind(lead)
        .execute(&srv.pool)
        .await
        .unwrap();
    for m in members {
        sqlx::query("INSERT INTO team_members (team_id, user_id, role) VALUES ($1, $2, 'member')")
            .bind(team_id)
            .bind(m)
            .execute(&srv.pool)
            .await
            .unwrap();
    }
    team_id
}

async fn project_by(srv: &Server, org_id: Uuid, creator: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO projects (org_id, name, created_by) VALUES ($1, 'Nord', $2) RETURNING id",
    )
    .bind(org_id)
    .bind(creator)
    .fetch_one(&srv.pool)
    .await
    .unwrap()
}

fn insights_url(srv: &Server, org_id: Uuid) -> String {
    format!(
        "{}/api/business/organizations/{org_id}/insights",
        base(srv)
    )
}

async fn ask(http: &Client, srv: &Server, jwt: &str, org_id: Uuid, body: Value) -> reqwest::Response {
    http.post(insights_url(srv, org_id))
        .bearer_auth(jwt)
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn balance(srv: &Server, org_id: Uuid) -> i32 {
    sqlx::query_scalar("SELECT credits_balance FROM organizations WHERE id = $1")
        .bind(org_id)
        .fetch_one(&srv.pool)
        .await
        .unwrap()
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
// Who may ask
// ---------------------------------------------------------------------------

#[tokio::test]
async fn asking_needs_a_signed_in_caller() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 10_000).await;

    let r = http
        .post(insights_url(&srv, org_id))
        .json(&json!({ "mode": "qa", "question": "how are we doing?" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn an_outsider_is_not_told_the_organization_exists() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 10_000).await;
    let (_, outsider) = user(&srv, "Outsider").await;

    let r = ask(
        &http,
        &srv,
        &outsider,
        org_id,
        json!({ "mode": "qa", "question": "anything?" }),
    )
    .await;
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn a_member_who_leads_nothing_is_refused() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 10_000).await;
    let (_, member_jwt) = member(&srv, org_id, "member", "Plain").await;

    // Insights read other people's transcripts: leads and owners only.
    let r = ask(
        &http,
        &srv,
        &member_jwt,
        org_id,
        json!({ "mode": "qa", "question": "how is the team?" }),
    )
    .await;
    assert_eq!(r.status(), 403);
    assert_eq!(
        balance(&srv, org_id).await,
        10_000,
        "a refused question must not be charged"
    );
}

#[tokio::test]
async fn without_an_embeddings_provider_insights_say_so() {
    let srv = skip_without_db!(setup_with(false).await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 10_000).await;

    let r = ask(
        &http,
        &srv,
        &jwt,
        org_id,
        json!({ "mode": "qa", "question": "anything?" }),
    )
    .await;
    assert_eq!(r.status(), 503);
    assert_eq!(balance(&srv, org_id).await, 10_000);
}

// ---------------------------------------------------------------------------
// What may be asked
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_mode_is_refused_and_costs_nothing() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 10_000).await;

    for mode in ["", "summary", "QA", "project-report"] {
        let r = ask(&http, &srv, &jwt, org_id, json!({ "mode": mode })).await;
        assert_eq!(r.status(), 400, "mode {mode:?} should be refused");
    }
    assert_eq!(balance(&srv, org_id).await, 10_000);
}

#[tokio::test]
async fn a_question_mode_needs_a_question() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 10_000).await;

    for body in [
        json!({ "mode": "qa" }),
        json!({ "mode": "qa", "question": "" }),
        json!({ "mode": "qa", "question": "   " }),
    ] {
        assert_eq!(ask(&http, &srv, &jwt, org_id, body).await.status(), 400);
    }
}

#[tokio::test]
async fn a_project_report_needs_a_project_and_a_member_report_needs_a_member() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 10_000).await;

    assert_eq!(
        ask(&http, &srv, &jwt, org_id, json!({ "mode": "project_report" }))
            .await
            .status(),
        400
    );
    assert_eq!(
        ask(&http, &srv, &jwt, org_id, json!({ "mode": "member_report" }))
            .await
            .status(),
        400
    );
}

#[tokio::test]
async fn a_report_about_something_in_another_org_is_refused() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 10_000).await;
    let (other_owner, _) = user(&srv, "Other").await;
    let other_org = org(&srv, other_owner, 10_000).await;
    let other_project = project_by(&srv, other_org, other_owner).await;

    // An owner has whole-org scope, but "whole org" is still THIS org.
    assert_eq!(
        ask(
            &http,
            &srv,
            &jwt,
            org_id,
            json!({ "mode": "project_report", "project_id": other_project }),
        )
        .await
        .status(),
        403
    );
    assert_eq!(
        ask(
            &http,
            &srv,
            &jwt,
            org_id,
            json!({ "mode": "member_report", "member_id": other_owner }),
        )
        .await
        .status(),
        403
    );
    assert_eq!(balance(&srv, org_id).await, 10_000);
}

#[tokio::test]
async fn a_report_about_something_that_does_not_exist_is_refused() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 10_000).await;

    assert_eq!(
        ask(
            &http,
            &srv,
            &jwt,
            org_id,
            json!({ "mode": "project_report", "project_id": Uuid::new_v4() }),
        )
        .await
        .status(),
        403
    );
}

// ---------------------------------------------------------------------------
// Team-lead scope
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_lead_may_report_on_their_own_team_members() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 10_000).await;
    let (lead_id, lead_jwt) = member(&srv, org_id, "member", "Lead").await;
    let (teammate, _) = member(&srv, org_id, "member", "Teammate").await;
    team_led_by(&srv, org_id, lead_id, &[teammate]).await;

    // Past every gate, so it reaches the charge and then the embedding, which
    // fails on a fake key — the 502 is what proves the scope check passed.
    let r = ask(
        &http,
        &srv,
        &lead_jwt,
        org_id,
        json!({ "mode": "member_report", "member_id": teammate }),
    )
    .await;
    assert_eq!(r.status(), 502);
}

#[tokio::test]
async fn a_lead_may_not_report_on_someone_outside_their_team() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 10_000).await;
    let (lead_id, lead_jwt) = member(&srv, org_id, "member", "Lead").await;
    let (stranger, _) = member(&srv, org_id, "member", "Stranger").await;
    team_led_by(&srv, org_id, lead_id, &[]).await;

    let r = ask(
        &http,
        &srv,
        &lead_jwt,
        org_id,
        json!({ "mode": "member_report", "member_id": stranger }),
    )
    .await;
    assert_eq!(r.status(), 403);
    assert_eq!(balance(&srv, org_id).await, 10_000);
}

#[tokio::test]
async fn a_lead_may_not_report_on_a_project_outside_their_team() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 10_000).await;
    let (lead_id, lead_jwt) = member(&srv, org_id, "member", "Lead").await;
    team_led_by(&srv, org_id, lead_id, &[]).await;
    let owners_project = project_by(&srv, org_id, owner).await;

    let r = ask(
        &http,
        &srv,
        &lead_jwt,
        org_id,
        json!({ "mode": "project_report", "project_id": owners_project }),
    )
    .await;
    assert_eq!(r.status(), 403);
}

#[tokio::test]
async fn a_lead_may_report_on_a_project_a_team_member_created() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 10_000).await;
    let (lead_id, lead_jwt) = member(&srv, org_id, "member", "Lead").await;
    let (teammate, _) = member(&srv, org_id, "member", "Teammate").await;
    team_led_by(&srv, org_id, lead_id, &[teammate]).await;
    let their_project = project_by(&srv, org_id, teammate).await;

    let r = ask(
        &http,
        &srv,
        &lead_jwt,
        org_id,
        json!({ "mode": "project_report", "project_id": their_project }),
    )
    .await;
    assert_eq!(r.status(), 502, "scope allowed it; only the embedding failed");
}

#[tokio::test]
async fn a_scoped_question_is_checked_against_the_same_scope_as_a_report() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 10_000).await;
    let (lead_id, lead_jwt) = member(&srv, org_id, "member", "Lead").await;
    team_led_by(&srv, org_id, lead_id, &[]).await;
    let owners_project = project_by(&srv, org_id, owner).await;

    // `qa` narrowed to a project or a member must not become a way round the
    // scope the report modes enforce.
    let r = ask(
        &http,
        &srv,
        &lead_jwt,
        org_id,
        json!({ "mode": "qa", "question": "how is it going?", "project_id": owners_project }),
    )
    .await;
    assert_eq!(r.status(), 403);
}

// ---------------------------------------------------------------------------
// Paying for it
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_org_that_cannot_pay_is_told_the_price() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 0).await;

    let r = ask(
        &http,
        &srv,
        &jwt,
        org_id,
        json!({ "mode": "qa", "question": "how are we doing?" }),
    )
    .await;

    assert_eq!(r.status(), 402);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "insufficient_org_credits");
    assert_eq!(body["balance"], 0);
    // Without `required` the dashboard can only say "not enough".
    assert!(body["required"].as_i64().unwrap() > 0);
}

#[tokio::test]
async fn a_failure_after_the_charge_refunds_it() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 10_000).await;

    let r = ask(
        &http,
        &srv,
        &jwt,
        org_id,
        json!({ "mode": "qa", "question": "how are we doing?" }),
    )
    .await;

    assert_eq!(r.status(), 502, "the embedding cannot be produced here");
    assert_eq!(
        balance(&srv, org_id).await,
        10_000,
        "an insight that was never produced must not be paid for"
    );
}

#[tokio::test]
async fn the_refund_is_recorded_rather_than_quietly_adjusting_the_balance() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 10_000).await;

    ask(
        &http,
        &srv,
        &jwt,
        org_id,
        json!({ "mode": "qa", "question": "how are we doing?" }),
    )
    .await;

    // Both legs are in the ledger: the charge and the refund.
    let entries: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM organization_credits_transactions
          WHERE org_id = $1 AND type = 'insight'",
    )
    .bind(org_id)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(entries, 2);
}
