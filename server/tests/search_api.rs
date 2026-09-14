//! Semantic transcript search, and the internal embeddings backfill behind it.
//!
//! `search_scope.rs` asserts the KNN predicate as SQL; this drives the endpoint.
//! Embeddings cannot be produced with a fake key, so the tests stop where the real
//! vector would be needed — which still covers every gate that matters: who may
//! search, which projects they may reach, and the short-circuit that answers a
//! member who can see nothing without spending an embedding call at all.
//!
//! DB-gated: skipped without `DATABASE_URL`.

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::Client;
use serde_json::Value;
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::Config;
use voxtranslate_server::embeddings::OpenAiEmbeddings;
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::{app, db, AppState};

const SECRET: &str = "search-api-secret";
const BACKFILL_SECRET: &str = "backfill-secret-value";

struct Server {
    addr: SocketAddr,
    pool: db::Pool,
}

async fn setup_with(embeddings: bool, backfill_secret: Option<&str>) -> Option<Server> {
    let url = voxtranslate_server::db::test_database_url()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let mut config = Config::test_with_billing(&url, SECRET, 0.0);
    config.embeddings_backfill_secret = backfill_secret.map(str::to_string);
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
    setup_with(true, Some(BACKFILL_SECRET)).await
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

async fn org(srv: &Server, owner: Uuid) -> Uuid {
    let org_id: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id, subscription_status, current_period_end)
         VALUES ('Search Co', $1, $2, 'active', now() + interval '30 days') RETURNING id",
    )
    .bind(format!("se-{}", Uuid::new_v4().simple()))
    .bind(owner)
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

async fn member(srv: &Server, org_id: Uuid, role: &str) -> (Uuid, String) {
    let (user_id, jwt) = user(srv, "Member").await;
    sqlx::query("INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, $3)")
        .bind(org_id)
        .bind(user_id)
        .bind(role)
        .execute(&srv.pool)
        .await
        .unwrap();
    (user_id, jwt)
}

fn search_url(srv: &Server, org_id: Uuid, query: &str) -> String {
    format!(
        "{}/api/business/organizations/{org_id}/search{query}",
        base(srv)
    )
}

async fn search(
    http: &Client,
    srv: &Server,
    jwt: &str,
    org_id: Uuid,
    q: &str,
) -> reqwest::Response {
    http.get(search_url(srv, org_id, q))
        .bearer_auth(jwt)
        .send()
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
// Who may search
// ---------------------------------------------------------------------------

#[tokio::test]
async fn searching_needs_a_signed_in_caller() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;

    let r = http
        .get(search_url(&srv, org_id, "?q=latency"))
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
    let org_id = org(&srv, owner).await;
    let (_, outsider) = user(&srv, "Outsider").await;

    assert_eq!(
        search(&http, &srv, &outsider, org_id, "?q=latency")
            .await
            .status(),
        404
    );
}

// ---------------------------------------------------------------------------
// Query validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_search_with_no_query_is_refused() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;

    for query in ["", "?q=", "?q=%20%20"] {
        assert_eq!(
            search(&http, &srv, &jwt, org_id, query).await.status(),
            400,
            "{query:?} should be refused"
        );
    }
}

#[tokio::test]
async fn without_an_embeddings_provider_search_says_so() {
    let srv = skip_without_db!(setup_with(false, None).await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;

    assert_eq!(
        search(&http, &srv, &jwt, org_id, "?q=latency")
            .await
            .status(),
        503
    );
}

#[tokio::test]
async fn an_embedding_failure_is_reported_as_a_bad_gateway() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;

    // The owner has whole-org scope, so nothing short-circuits and the fake key
    // fails at the embedding call — which is the caller's problem to retry, not
    // a 500.
    assert_eq!(
        search(&http, &srv, &jwt, org_id, "?q=latency")
            .await
            .status(),
        502
    );
}

// ---------------------------------------------------------------------------
// Scope
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_member_who_can_see_nothing_gets_an_empty_answer_without_an_embedding_call() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let (_, member_jwt) = member(&srv, org_id, "member").await;

    // No projects created, none participated in, no webinars hosted. The handler
    // answers before spending an embedding — a 502 here would prove it did not.
    let r = search(&http, &srv, &member_jwt, org_id, "?q=latency").await;
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["results"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn a_member_who_created_a_project_is_taken_past_the_short_circuit() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let (member_id, member_jwt) = member(&srv, org_id, "member").await;
    sqlx::query("INSERT INTO projects (org_id, name, created_by) VALUES ($1, 'Nord', $2)")
        .bind(org_id)
        .bind(member_id)
        .execute(&srv.pool)
        .await
        .unwrap();

    // Now there IS something to search, so the embedding is attempted.
    assert_eq!(
        search(&http, &srv, &member_jwt, org_id, "?q=latency")
            .await
            .status(),
        502
    );
}

#[tokio::test]
async fn a_member_who_hosts_a_webinar_can_search_even_with_no_projects() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let (member_id, member_jwt) = member(&srv, org_id, "member").await;
    sqlx::query(
        "INSERT INTO webinars (org_id, host_user_id, title, code, source_language)
         VALUES ($1, $2, 'Launch', $3, 'en')",
    )
    .bind(org_id)
    .bind(member_id)
    .bind(format!("W{}", &Uuid::new_v4().simple().to_string()[..8]))
    .execute(&srv.pool)
    .await
    .unwrap();

    // A host must be able to find their own webinar even when it is bound to no
    // project — so the short-circuit must not fire.
    assert_eq!(
        search(&http, &srv, &member_jwt, org_id, "?q=latency")
            .await
            .status(),
        502
    );
}

#[tokio::test]
async fn an_archived_project_does_not_keep_a_member_in_scope() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let (member_id, member_jwt) = member(&srv, org_id, "member").await;
    sqlx::query(
        "INSERT INTO projects (org_id, name, created_by, archived_at)
         VALUES ($1, 'Old', $2, now())",
    )
    .bind(org_id)
    .bind(member_id)
    .execute(&srv.pool)
    .await
    .unwrap();

    let r = search(&http, &srv, &member_jwt, org_id, "?q=latency").await;
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.json::<Value>().await.unwrap()["results"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[tokio::test]
async fn an_admin_searches_the_whole_org_without_resolving_a_project_set() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let (_, admin_jwt) = member(&srv, org_id, "admin").await;

    // No projects at all, yet an admin still reaches the embedding step: their
    // scope is the org, not a resolved list.
    assert_eq!(
        search(&http, &srv, &admin_jwt, org_id, "?q=latency")
            .await
            .status(),
        502
    );
}

// ---------------------------------------------------------------------------
// The internal backfill
// ---------------------------------------------------------------------------

fn backfill_url(srv: &Server, query: &str) -> String {
    format!("{}/internal/embeddings/backfill{query}", base(srv))
}

#[tokio::test]
async fn the_backfill_endpoint_looks_absent_when_it_is_not_configured() {
    let srv = skip_without_db!(setup_with(true, None).await);
    let http = Client::new();

    // 404, not 401: an unconfigured internal endpoint should not advertise itself.
    let r = http
        .post(backfill_url(&srv, ""))
        .bearer_auth(BACKFILL_SECRET)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn the_backfill_endpoint_refuses_a_missing_or_wrong_secret() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();

    assert_eq!(
        http.post(backfill_url(&srv, ""))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        http.post(backfill_url(&srv, ""))
            .bearer_auth("wrong-secret")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    // A secret of the right length but wrong content must also fail.
    let same_length = "x".repeat(BACKFILL_SECRET.len());
    assert_eq!(
        http.post(backfill_url(&srv, ""))
            .bearer_auth(&same_length)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
}

#[tokio::test]
async fn the_backfill_endpoint_refuses_a_non_bearer_authorization_header() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();

    let r = http
        .post(backfill_url(&srv, ""))
        .header("authorization", format!("Token {BACKFILL_SECRET}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn the_backfill_needs_an_embeddings_provider() {
    let srv = skip_without_db!(setup_with(false, Some(BACKFILL_SECRET)).await);
    let http = Client::new();

    let r = http
        .post(backfill_url(&srv, ""))
        .bearer_auth(BACKFILL_SECRET)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
}

#[tokio::test]
async fn the_backfill_reports_what_it_did_and_what_is_left() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();

    let r = http
        .post(backfill_url(&srv, "?limit=1"))
        .bearer_auth(BACKFILL_SECRET)
        .send()
        .await
        .unwrap();

    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    // Resumable by design: run it until `remaining` is 0.
    assert!(body["remaining"].is_number());
    assert!(body["embedded"].is_number());
}

#[tokio::test]
async fn the_backfill_batch_size_is_clamped() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();

    for query in ["?limit=0", "?limit=100000", "?limit=-5"] {
        let r = http
            .post(backfill_url(&srv, query))
            .bearer_auth(BACKFILL_SECRET)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{query} should be clamped, not refused");
    }
}
