//! Self-service completion of Telnyx regulatory requirements for a purchased number
//! (spec 0119): discovery, submission, group reuse and status refresh.
//!
//! DB-gated: skipped without `DATABASE_URL`.

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::Client;
use serde_json::Value;
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::{Config, VoipConfig};
use voxtranslate_server::db::{self, Pool};
use voxtranslate_server::telephony::mock::MockTelephonyProvider;
use voxtranslate_server::{app, AppState};

const SECRET: &str = "voip-requirements-secret";

struct Server {
    addr: SocketAddr,
    pool: Pool,
}

async fn setup() -> Option<Server> {
    let url = db::test_database_url()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let mut config = Config::test_with_billing(&url, SECRET, 0.0);
    config.voip = Some(VoipConfig::test_default());
    let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);
    let mut state = AppState::new(config);
    state.billing = Some(BillingService::new(pool.clone(), min_join));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);
    // A later slice of this feature scripts the mock directly (provider failure mapping,
    // status transitions); discovery here only needs one enabled and reachable.
    state.telephony = Some(Arc::new(MockTelephonyProvider::default()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    Some(Server { addr, pool })
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
        "INSERT INTO organizations (name, slug, owner_id, subscription_status,
                                    current_period_end, credits_balance)
         VALUES ('Phone Co', $1, $2, 'active', now() + interval '30 days', 100000) RETURNING id",
    )
    .bind(format!("vnr-{}", Uuid::new_v4().simple()))
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

fn fresh_e164() -> String {
    let n = Uuid::new_v4().as_u128() % 100_000_000;
    format!("+3312{n:08}")
}

/// A number this org owns, purchased with a regulatory requirement and linked to a
/// sub-order — the state `buy()` (Phase 1) leaves a regulated purchase in.
async fn regulated_number(srv: &Server, org_id: Uuid, sub_order_id: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO voip_numbers
            (org_id, provider, provider_number_id, e164, country, status,
             outbound_enabled, regulatory_requirement, provider_order_id,
             provider_sub_order_id, number_kind)
         VALUES ($1, 'mock', $2, $3, 'FR', 'pending_regulatory', FALSE,
                 'proof of address required', $4, $4, 'mobile')
         RETURNING id",
    )
    .bind(org_id)
    .bind(format!("prov-{}", Uuid::new_v4().simple()))
    .bind(fresh_e164())
    .bind(sub_order_id)
    .fetch_one(&srv.pool)
    .await
    .unwrap()
}

/// A legacy `pending_regulatory` row from before this feature: it HAD a requirement, but
/// was purchased before order/sub-order ids were persisted (Phase 1 amends `buy()`).
async fn legacy_unlinked_number(srv: &Server, org_id: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO voip_numbers
            (org_id, provider, provider_number_id, e164, country, status,
             outbound_enabled, regulatory_requirement)
         VALUES ($1, 'mock', $2, $3, 'FR', 'pending_regulatory', FALSE,
                 'proof of address required')
         RETURNING id",
    )
    .bind(org_id)
    .bind(format!("prov-{}", Uuid::new_v4().simple()))
    .bind(fresh_e164())
    .fetch_one(&srv.pool)
    .await
    .unwrap()
}

/// A number that never needed any paperwork at all.
async fn plain_number(srv: &Server, org_id: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO voip_numbers (org_id, provider, provider_number_id, e164, country, status)
         VALUES ($1, 'mock', $2, $3, 'IT', 'active') RETURNING id",
    )
    .bind(org_id)
    .bind(format!("prov-{}", Uuid::new_v4().simple()))
    .bind(fresh_e164())
    .fetch_one(&srv.pool)
    .await
    .unwrap()
}

fn requirements_url(srv: &Server, org_id: Uuid, number_id: Uuid, suffix: &str) -> String {
    format!(
        "{}/api/business/organizations/{org_id}/voip/numbers/{number_id}/requirements{suffix}",
        base(srv)
    )
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
// Discovery — GET
// ---------------------------------------------------------------------------

#[tokio::test]
async fn discovering_requirements_returns_the_providers_fixture_list() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-1").await;

    let r = http
        .get(requirements_url(&srv, org_id, number_id, ""))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["status"], "pending_regulatory");
    assert_eq!(body["group"]["reused"], false);
    let reqs = body["requirements"].as_array().unwrap();
    assert_eq!(reqs.len(), 3, "body: {body}");
    assert!(reqs.iter().any(|r| r["id"] == "business_name"));
    assert!(reqs.iter().any(|r| r["kind"] == "document"));
}

#[tokio::test]
async fn a_second_number_in_the_same_combination_reuses_the_first_groups_row() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let first = regulated_number(&srv, org_id, "so-a").await;
    let second = regulated_number(&srv, org_id, "so-b").await;

    let r1 = http
        .get(requirements_url(&srv, org_id, first, ""))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r1.status(), 200);
    let body1: Value = r1.json().await.unwrap();
    assert_eq!(body1["group"]["reused"], false);

    let r2 = http
        .get(requirements_url(&srv, org_id, second, ""))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r2.status(), 200);
    let body2: Value = r2.json().await.unwrap();
    assert_eq!(
        body2["group"]["reused"], true,
        "the second number in the same org+country+kind combination must reuse the group"
    );

    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM voip_requirement_groups
          WHERE org_id = $1 AND country = 'FR' AND number_kind = 'mobile'",
    )
    .bind(org_id)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(rows, 1, "one group row, shared by both numbers");
}

#[tokio::test]
async fn a_non_regulated_number_refuses_discovery() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = plain_number(&srv, org_id).await;

    let r = http
        .get(requirements_url(&srv, org_id, number_id, ""))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "regulatory_not_required");
}

#[tokio::test]
async fn a_legacy_row_without_a_sub_order_id_is_unlinked_not_fabricated() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = legacy_unlinked_number(&srv, org_id).await;

    let r = http
        .get(requirements_url(&srv, org_id, number_id, ""))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "regulatory_unlinked");
}

#[tokio::test]
async fn discovering_a_number_the_org_does_not_own_is_a_404() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let (other_owner, _) = user(&srv, "Other").await;
    let other_org = org(&srv, other_owner).await;
    let their_number = regulated_number(&srv, other_org, "so-theirs").await;

    let r = http
        .get(requirements_url(&srv, org_id, their_number, ""))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        404,
        "must not confirm the other org's number exists"
    );
}

#[tokio::test]
async fn viewing_requirements_is_an_administrative_act() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-member").await;
    let (_, member_jwt) = member(&srv, org_id, "member").await;

    let r = http
        .get(requirements_url(&srv, org_id, number_id, ""))
        .bearer_auth(&member_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
}

#[tokio::test]
async fn a_claim_still_being_created_is_reported_busy() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-busy").await;

    // The exact state a real second caller would land on mid-race (design D7).
    sqlx::query(
        "INSERT INTO voip_requirement_groups (org_id, provider, country, number_kind, action)
         VALUES ($1, 'mock', 'FR', 'mobile', 'ordering')",
    )
    .bind(org_id)
    .execute(&srv.pool)
    .await
    .unwrap();

    let r = http
        .get(requirements_url(&srv, org_id, number_id, ""))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "requirements_busy");
}
