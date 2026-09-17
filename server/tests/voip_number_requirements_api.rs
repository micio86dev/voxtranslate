//! Self-service completion of Telnyx regulatory requirements for a purchased number
//! (spec 0119): discovery, submission, group reuse and status refresh.
//!
//! DB-gated: skipped without `DATABASE_URL`.

use std::net::SocketAddr;
use std::sync::Arc;

use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use reqwest::Client;
use serde_json::{json, Value};
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::{Config, TelnyxConfig, VoipConfig};
use voxtranslate_server::db::{self, Pool};
use voxtranslate_server::telephony::mock::MockTelephonyProvider;
use voxtranslate_server::telephony::telnyx::TelnyxProvider;
use voxtranslate_server::telephony::{
    FieldValue, ProviderError, RequirementGroupId, SubOrderId, TelephonyProvider,
};
use voxtranslate_server::{app, AppState};

const SECRET: &str = "voip-requirements-secret";

struct Server {
    addr: SocketAddr,
    pool: Pool,
    provider: Arc<MockTelephonyProvider>,
    /// A clone of the serving state, for tests that drive `run_sweep` directly (no HTTP).
    state: AppState,
}

async fn setup() -> Option<Server> {
    setup_with_reconcile(true).await
}

/// `reconcile = false` reproduces `VOIP_REGULATORY_RECONCILE=false`.
async fn setup_with_reconcile(reconcile: bool) -> Option<Server> {
    let url = db::test_database_url()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let mut config = Config::test_with_billing(&url, SECRET, 0.0);
    config.voip = Some(VoipConfig {
        regulatory_reconcile: reconcile,
        ..VoipConfig::test_default()
    });
    let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);
    let mut state = AppState::new(config);
    state.billing = Some(BillingService::new(pool.clone(), min_join));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);
    let provider = Arc::new(MockTelephonyProvider::default());
    state.telephony = Some(provider.clone());
    let state_for_sweep = state.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    Some(Server {
        addr,
        pool,
        provider,
        state: state_for_sweep,
    })
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

fn requirement_document_url(srv: &Server, org_id: Uuid, number_id: Uuid) -> String {
    requirements_url(srv, org_id, number_id, "/documents")
}

/// Real magic bytes for the two formats these tests exercise — enough for the sniff,
/// never a full valid document (this module never needs one to be valid).
const PDF_BYTES: &[u8] = b"%PDF-1.4 minimal test bytes for the sniff";
const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\nrest of a fake png payload";

fn pdf_part(bytes: Vec<u8>) -> reqwest::multipart::Part {
    reqwest::multipart::Part::bytes(bytes)
        .file_name("ignored.pdf")
        .mime_str("application/pdf")
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
// Kill switch (spec 0119 R7 "Kill switch disables automatic reconciliation") — the sweep
// and the webhook nudge below both stand down; the routes above never do.
// ---------------------------------------------------------------------------

/// Covers 3 of the 4 gate scenarios in one purchase: the sweep leaves a due row and a
/// stale group claim untouched, but `/refresh` on that SAME number still works — proving
/// the switch gates the background jobs only, never the requirements routes.
#[tokio::test]
async fn with_the_kill_switch_off_the_sweep_stands_down_but_refresh_still_works() {
    let srv = skip_without_db!(setup_with_reconcile(false).await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-gate").await;
    sqlx::query(
        "UPDATE voip_numbers SET regulatory_next_check_at = now() - interval '1 minute'
          WHERE id = $1",
    )
    .bind(number_id)
    .execute(&srv.pool)
    .await
    .unwrap();
    // A stale claim (design D7), backdated in the same INSERT rather than a second UPDATE.
    let group_id: Uuid = sqlx::query_scalar(
        "INSERT INTO voip_requirement_groups
            (org_id, provider, country, number_kind, action, created_at)
         VALUES ($1, 'mock', 'FR', 'mobile', 'ordering', now() - interval '11 minutes')
         RETURNING id",
    )
    .bind(org_id)
    .fetch_one(&srv.pool)
    .await
    .unwrap();

    let query = "SELECT status, regulatory_next_check_at, regulatory_failures
                   FROM voip_numbers WHERE id = $1";
    let before: (String, chrono::DateTime<chrono::Utc>, i32) = sqlx::query_as(query)
        .bind(number_id)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    let handle = tokio::spawn(voxtranslate_server::voip::webhook::run_sweep(
        srv.state.clone(),
        std::time::Duration::from_millis(30),
        25,
    ));
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    handle.abort();
    let after: (String, chrono::DateTime<chrono::Utc>, i32) = sqlx::query_as(query)
        .bind(number_id)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert_eq!(
        before, after,
        "the kill switch must leave a due row completely untouched — not even the claim step"
    );
    let still_there: i64 =
        sqlx::query_scalar("SELECT count(*) FROM voip_requirement_groups WHERE id = $1")
            .bind(group_id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(
        still_there, 1,
        "the kill switch must not reclaim stale groups either"
    );

    // Routes are never gated — /refresh still queries the provider directly on demand.
    srv.provider.set_sub_order_state(
        &SubOrderId("so-gate".into()),
        voxtranslate_server::telephony::SubOrderState {
            order: voxtranslate_server::telephony::OrderStatus::Pending,
            requirements: voxtranslate_server::telephony::RequirementsStatus::UnderReview,
            group: None,
        },
    );
    let r = http
        .post(requirements_url(&srv, org_id, number_id, "/refresh"))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        200,
        "the kill switch must never gate the requirements routes"
    );
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["status"], "regulatory_review");

    // This group is genuinely stale (backdated `created_at`) and was deliberately left
    // unreclaimed by the switch above — clean it up so a LATER, unrelated invocation of
    // `reclaim_stale_groups` elsewhere in the suite never counts it.
    sqlx::query("DELETE FROM voip_requirement_groups WHERE id = $1")
        .bind(group_id)
        .execute(&srv.pool)
        .await
        .unwrap();
}

/// Call `inbound_webhook` directly with a correctly-signed `number_order.complete` body
/// (only the real Telnyx `normalise()` understands it); returns the resulting
/// `regulatory_next_check_at`.
async fn number_order_webhook_next_check(reconcile: bool) -> Option<chrono::DateTime<chrono::Utc>> {
    let url = db::test_database_url()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let mut config = Config::test_with_billing(&url, SECRET, 0.0);
    config.voip = Some(VoipConfig {
        regulatory_reconcile: reconcile,
        ..VoipConfig::test_default()
    });
    let mut state = AppState::new(config);
    state.pool = Some(pool.clone());
    let signing_key = SigningKey::from_bytes(&[7u8; 32]);
    let public_key_b64 =
        base64::engine::general_purpose::STANDARD.encode(signing_key.verifying_key().to_bytes());
    state.telephony = Some(Arc::new(TelnyxProvider::new(
        TelnyxConfig {
            api_key: "test-key".into(),
            api_base: "https://example.invalid".into(),
            connection_id: "conn-1".into(),
            outbound_voice_profile_id: None,
            public_key_b64,
            default_caller_id: None,
            media_anchor: "Frankfurt, Germany".into(),
        },
        300,
    )));

    // A user + org in one round trip — no `Server`/JWT needed, webhooks are unauthenticated.
    let org_id: Uuid = sqlx::query_scalar(
        "WITH u AS (
             INSERT INTO users (google_id, email, name, balance)
             VALUES ($1, $2, 'Owner', 0) RETURNING id
         )
         INSERT INTO organizations (name, slug, owner_id, credits_balance)
         SELECT 'Phone Co', $3, id, 0 FROM u RETURNING id",
    )
    .bind(format!("g-{}", Uuid::new_v4()))
    .bind(format!("{}@example.test", Uuid::new_v4()))
    .bind(format!("wh-{}", Uuid::new_v4().simple()))
    .fetch_one(&pool)
    .await
    .unwrap();
    // Scheduled an hour out — only a genuine nudge could move it into the near future.
    let sub_order_id = format!("so-gate-{}", Uuid::new_v4().simple());
    let number_id: Uuid = sqlx::query_scalar(
        "INSERT INTO voip_numbers
            (org_id, provider, provider_number_id, e164, country, status,
             outbound_enabled, regulatory_requirement, provider_order_id,
             provider_sub_order_id, number_kind, regulatory_next_check_at)
         VALUES ($1, 'telnyx', $2, $3, 'FR', 'pending_regulatory', FALSE,
                 'proof of address required', $4, $4, 'mobile', now() + interval '1 hour')
         RETURNING id",
    )
    .bind(org_id)
    .bind(format!("prov-{}", Uuid::new_v4().simple()))
    .bind(fresh_e164())
    .bind(&sub_order_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    let ts = chrono::Utc::now().timestamp().to_string();
    let body = serde_json::to_vec(&json!({
        "data": {
            "id": "evt-gate",
            "event_type": "number_order.complete",
            "occurred_at": chrono::Utc::now().to_rfc3339(),
            "payload": { "id": "order-1", "sub_number_orders_ids": [sub_order_id] },
        }
    }))
    .unwrap();
    let mut msg = ts.clone().into_bytes();
    msg.push(b'|');
    msg.extend_from_slice(&body);
    let sig = base64::engine::general_purpose::STANDARD.encode(signing_key.sign(&msg).to_bytes());
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("telnyx-signature-ed25519", sig.parse().unwrap());
    headers.insert("telnyx-timestamp", ts.parse().unwrap());

    let result = voxtranslate_server::voip::routes::inbound_webhook(
        axum::extract::State(state),
        axum::extract::Path("telnyx".to_string()),
        headers,
        axum::body::Bytes::from(body),
    )
    .await;
    assert!(
        result.is_ok(),
        "a validly signed webhook must always be accepted"
    );

    Some(
        sqlx::query_scalar("SELECT regulatory_next_check_at FROM voip_numbers WHERE id = $1")
            .bind(number_id)
            .fetch_one(&pool)
            .await
            .unwrap(),
    )
}

#[tokio::test]
async fn with_the_kill_switch_off_a_signed_webhook_is_accepted_but_does_not_nudge() {
    let Some(next_check) = number_order_webhook_next_check(false).await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    assert!(
        next_check > chrono::Utc::now() + chrono::Duration::minutes(30),
        "the kill switch must stop the webhook nudge — expected the untouched +1h schedule"
    );
}

#[tokio::test]
async fn with_the_kill_switch_on_the_same_webhook_does_nudge() {
    // The control: proves the gate is real rather than a dead path that never nudges.
    let Some(next_check) = number_order_webhook_next_check(true).await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    assert!(
        next_check <= chrono::Utc::now() + chrono::Duration::seconds(5),
        "with the switch on, the webhook must nudge the next check to now"
    );
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

// ---------------------------------------------------------------------------
// Submission — PUT
// ---------------------------------------------------------------------------

#[tokio::test]
async fn put_round_trips_a_valid_textual_and_address_value() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-put").await;

    let r = http
        .put(requirements_url(&srv, org_id, number_id, ""))
        .bearer_auth(&jwt)
        .json(&json!({
            "values": [
                { "requirement_id": "business_name", "value": "Acme SRL" },
                { "requirement_id": "registered_address", "value": {
                    "first_name": "Jane",
                    "last_name": "Doe",
                    "business_name": "Acme SRL",
                    "street_address": "1 Rue de Paris",
                    "locality": "Paris",
                    "postal_code": "75001",
                    "country_code": "FR",
                }},
            ],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "body: {}", r.text().await.unwrap());
}

#[tokio::test]
async fn putting_an_unknown_requirement_id_is_refused() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-unknown").await;

    let r = http
        .put(requirements_url(&srv, org_id, number_id, ""))
        .bearer_auth(&jwt)
        .json(&json!({ "values": [{ "requirement_id": "not_a_real_field", "value": "x" }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "requirement_unknown");
}

#[tokio::test]
async fn putting_a_non_string_value_for_a_textual_field_is_a_kind_mismatch() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-mismatch").await;

    let r = http
        .put(requirements_url(&srv, org_id, number_id, ""))
        .bearer_auth(&jwt)
        .json(&json!({ "values": [{ "requirement_id": "business_name", "value": 42 }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "requirement_kind_mismatch");
}

#[tokio::test]
async fn putting_a_document_value_is_refused_documents_go_through_the_upload_route() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-doc").await;

    let r = http
        .put(requirements_url(&srv, org_id, number_id, ""))
        .bearer_auth(&jwt)
        .json(&json!({ "values": [{ "requirement_id": "proof_of_address", "value": "doc-1" }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "requirement_kind_mismatch");
}

#[tokio::test]
async fn putting_a_value_over_the_length_bound_is_refused() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-long").await;

    let r = http
        .put(requirements_url(&srv, org_id, number_id, ""))
        .bearer_auth(&jwt)
        .json(&json!({
            "values": [{ "requirement_id": "business_name", "value": "x".repeat(501) }],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "value_too_long");
}

#[tokio::test]
async fn a_non_admin_may_not_edit_requirements() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-put-member").await;
    let (_, member_jwt) = member(&srv, org_id, "member").await;

    let r = http
        .put(requirements_url(&srv, org_id, number_id, ""))
        .bearer_auth(&member_jwt)
        .json(&json!({ "values": [{ "requirement_id": "business_name", "value": "x" }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
}

// ---------------------------------------------------------------------------
// Submit
// ---------------------------------------------------------------------------

async fn fill_every_field(srv: &Server, http: &Client, org_id: Uuid, number_id: Uuid, jwt: &str) {
    // The mock does not accept a document value via this route by design — its own
    // `submit_requirement_values` will simply never see `proof_of_address` filled here.
    // The completeness gate is therefore expected to still refuse a plain PUT-only fill;
    // callers that need a genuinely complete group script the document slot directly.
    let r = http
        .put(requirements_url(srv, org_id, number_id, ""))
        .bearer_auth(jwt)
        .json(&json!({
            "values": [
                { "requirement_id": "business_name", "value": "Acme SRL" },
                { "requirement_id": "registered_address", "value": {
                    "first_name": "Jane",
                    "last_name": "Doe",
                    "business_name": "Acme SRL",
                    "street_address": "1 Rue de Paris",
                    "locality": "Paris",
                    "postal_code": "75001",
                    "country_code": "FR",
                }},
            ],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "body: {}", r.text().await.unwrap());
}

#[tokio::test]
async fn submitting_before_every_field_is_filled_is_refused() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-incomplete").await;
    fill_every_field(&srv, &http, org_id, number_id, &jwt).await;

    let r = http
        .post(requirements_url(&srv, org_id, number_id, "/submit"))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "requirements_incomplete");
}

/// Directly scripts the mock's document slot filled, the way a real `POST /documents`
/// upload (Phase 6) would leave it — this test file's scope is the routes this PR builds,
/// not the still-unbuilt upload route.
async fn fill_document_slot_directly(srv: &Server, org_id: Uuid, number_id: Uuid) {
    let row_id: Uuid =
        sqlx::query_scalar("SELECT requirement_group_id FROM voip_numbers WHERE id = $1")
            .bind(number_id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    let provider_group_id: String =
        sqlx::query_scalar("SELECT provider_group_id FROM voip_requirement_groups WHERE id = $1")
            .bind(row_id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    srv.provider
        .submit_requirement_values(
            &RequirementGroupId(provider_group_id),
            &[(
                "proof_of_address".to_string(),
                FieldValue::Document("mock-doc-1".to_string()),
            )],
        )
        .await
        .unwrap();
    let _ = org_id;
}

#[tokio::test]
async fn submitting_a_fully_filled_group_moves_the_number_into_review() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-submit").await;
    fill_every_field(&srv, &http, org_id, number_id, &jwt).await;
    fill_document_slot_directly(&srv, org_id, number_id).await;

    let r = http
        .post(requirements_url(&srv, org_id, number_id, "/submit"))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202, "body: {}", r.text().await.unwrap());
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["status"], "regulatory_review");

    let (status, outbound): (String, bool) =
        sqlx::query_as("SELECT status, outbound_enabled FROM voip_numbers WHERE id = $1")
            .bind(number_id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(status, "regulatory_review");
    assert!(!outbound);

    // A second submit before any rejection is refused, not silently re-forwarded.
    let again = http
        .post(requirements_url(&srv, org_id, number_id, "/submit"))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 409);
    let body: Value = again.json().await.unwrap();
    assert_eq!(body["error"], "submission_already_pending");
}

#[tokio::test]
async fn a_non_admin_may_not_submit() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-submit-member").await;
    let (_, member_jwt) = member(&srv, org_id, "member").await;

    let r = http
        .post(requirements_url(&srv, org_id, number_id, "/submit"))
        .bearer_auth(&member_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
}

// ---------------------------------------------------------------------------
// Refresh
// ---------------------------------------------------------------------------

#[tokio::test]
async fn refresh_is_rate_limited_to_once_per_window() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-refresh").await;

    let first = http
        .post(requirements_url(&srv, org_id, number_id, "/refresh"))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200, "body: {}", first.text().await.unwrap());

    let second = http
        .post(requirements_url(&srv, org_id, number_id, "/refresh"))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 429);
    let body: Value = second.json().await.unwrap();
    assert_eq!(body["error"], "refresh_too_soon");
}

#[tokio::test]
async fn a_member_may_refresh_status() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-refresh-member").await;
    let (_, member_jwt) = member(&srv, org_id, "member").await;

    let r = http
        .post(requirements_url(&srv, org_id, number_id, "/refresh"))
        .bearer_auth(&member_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

// ---------------------------------------------------------------------------
// Provider failure mapping
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_provider_4xx_on_submission_is_surfaced_without_provider_prose() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-4xx").await;
    // Ensure the group exists first (GET creates it), then script the very next
    // `submit_requirement_values` call to fail the way a malformed-address 422 would.
    http.get(requirements_url(&srv, org_id, number_id, ""))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    srv.provider.fail_next(
        "submit_requirement_values",
        ProviderError::DestinationRefused,
    );

    let r = http
        .put(requirements_url(&srv, org_id, number_id, ""))
        .bearer_auth(&jwt)
        .json(&json!({ "values": [{ "requirement_id": "business_name", "value": "Acme SRL" }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "provider_rejected_value");
}

#[tokio::test]
async fn a_provider_5xx_on_submission_is_reported_as_a_retryable_gateway_failure() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-5xx").await;
    http.get(requirements_url(&srv, org_id, number_id, ""))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    srv.provider.fail_next(
        "submit_requirement_values",
        ProviderError::Unavailable {
            detail: "carrier outage".into(),
        },
    );

    let r = http
        .put(requirements_url(&srv, org_id, number_id, ""))
        .bearer_auth(&jwt)
        .json(&json!({ "values": [{ "requirement_id": "business_name", "value": "Acme SRL" }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 502);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "provider_unavailable");
}

/// A number in `regulatory_review` gains a decisive status once the mock scripts a
/// terminal sub-order state — proof `/refresh` actually calls `reconcile_one`, not just
/// that it answers 200.
#[tokio::test]
async fn refresh_applies_a_transition_read_from_the_provider() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-transition").await;
    srv.provider.set_sub_order_state(
        &SubOrderId("so-transition".into()),
        voxtranslate_server::telephony::SubOrderState {
            order: voxtranslate_server::telephony::OrderStatus::Pending,
            requirements: voxtranslate_server::telephony::RequirementsStatus::UnderReview,
            group: None,
        },
    );

    let r = http
        .post(requirements_url(&srv, org_id, number_id, "/refresh"))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["status"], "regulatory_review");

    let status: String = sqlx::query_scalar("SELECT status FROM voip_numbers WHERE id = $1")
        .bind(number_id)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert_eq!(status, "regulatory_review");
}

// ---------------------------------------------------------------------------
// Document upload — POST …/requirements/documents (Phase 6, spec 0119 "Document
// Stream-Through"). D9 (true streaming), D10 (size/type limits), D11 (field order +
// same-request link), D12 (no PII logging, no filename stored).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn uploading_a_document_links_it_and_shows_up_in_discovery() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-doc-upload").await;

    let form = reqwest::multipart::Form::new()
        .text("requirement_id", "proof_of_address")
        .part("file", pdf_part(PDF_BYTES.to_vec()));
    let r = http
        .post(requirement_document_url(&srv, org_id, number_id))
        .bearer_auth(&jwt)
        .multipart(form)
        .send()
        .await
        .unwrap();
    let status = r.status();
    let body: Value = r.json().await.unwrap();
    assert_eq!(status, 201, "body: {body}");
    assert_eq!(body["requirement_id"], "proof_of_address");
    // The mock's real Telnyx-shaped default (`scanned`) normalises to `passed` — task 6.8.
    assert_eq!(body["document"]["av_scan_status"], "passed");

    // The provider actually received the streamed bytes — proves the upload reached
    // `upload_document` for real, not that the route only pretended to succeed.
    let uploads = srv.provider.uploaded_documents();
    assert!(
        uploads
            .iter()
            .any(|(ct, size)| *ct == "application/pdf" && *size == PDF_BYTES.len() as u64),
        "got: {uploads:?}"
    );

    // GET reflects the same document status back (D12: av_scan_status only, no id).
    let get: Value = http
        .get(requirements_url(&srv, org_id, number_id, ""))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entry = get["requirements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "proof_of_address")
        .expect("the document requirement must be present");
    assert_eq!(entry["document"]["av_scan_status"], "passed");
}

#[tokio::test]
async fn missing_requirement_id_field_is_a_malformed_upload() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-doc-missing-reqid").await;

    let form = reqwest::multipart::Form::new().part("file", pdf_part(PDF_BYTES.to_vec()));
    let r = http
        .post(requirement_document_url(&srv, org_id, number_id))
        .bearer_auth(&jwt)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "malformed_upload");
    assert!(
        srv.provider.uploaded_documents().is_empty(),
        "the provider must never be called without a validated requirement_id"
    );
}

#[tokio::test]
async fn a_late_requirement_id_field_is_a_malformed_upload() {
    // D11: `requirement_id` must be the FIRST field. A client that sends the file first
    // is refused before any file byte is forwarded to the provider — not merely warned.
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-doc-late-reqid").await;

    let form = reqwest::multipart::Form::new()
        .part("file", pdf_part(PDF_BYTES.to_vec()))
        .text("requirement_id", "proof_of_address");
    let r = http
        .post(requirement_document_url(&srv, org_id, number_id))
        .bearer_auth(&jwt)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "malformed_upload");
    assert!(srv.provider.uploaded_documents().is_empty());
}

#[tokio::test]
async fn uploading_against_an_unknown_requirement_id_is_refused() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-doc-unknown-reqid").await;

    let form = reqwest::multipart::Form::new()
        .text("requirement_id", "no-such-requirement")
        .part("file", pdf_part(PDF_BYTES.to_vec()));
    let r = http
        .post(requirement_document_url(&srv, org_id, number_id))
        .bearer_auth(&jwt)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "requirement_unknown");
}

#[tokio::test]
async fn uploading_against_a_non_document_requirement_is_refused() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-doc-wrong-kind").await;

    let form = reqwest::multipart::Form::new()
        .text("requirement_id", "business_name")
        .part("file", pdf_part(PDF_BYTES.to_vec()));
    let r = http
        .post(requirement_document_url(&srv, org_id, number_id))
        .bearer_auth(&jwt)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "requirement_kind_mismatch");
    assert!(srv.provider.uploaded_documents().is_empty());
}

#[tokio::test]
async fn an_infected_scan_result_is_surfaced_as_failed_never_passed() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-doc-infected").await;
    srv.provider.set_next_document_scan_status("infected");

    let form = reqwest::multipart::Form::new()
        .text("requirement_id", "proof_of_address")
        .part("file", pdf_part(PDF_BYTES.to_vec()));
    let r = http
        .post(requirement_document_url(&srv, org_id, number_id))
        .bearer_auth(&jwt)
        .multipart(form)
        .send()
        .await
        .unwrap();
    let status = r.status();
    let body: Value = r.json().await.unwrap();
    assert_eq!(status, 201, "body: {body}");
    // `infected` must never normalise onto `passed` (task 6.8) — the requirement stays
    // unsatisfied.
    assert_eq!(body["document"]["av_scan_status"], "failed");
}

#[tokio::test]
async fn a_provider_4xx_on_document_upload_is_surfaced_without_provider_prose() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-doc-4xx").await;
    srv.provider
        .fail_next("upload_document", ProviderError::DestinationRefused);

    let form = reqwest::multipart::Form::new()
        .text("requirement_id", "proof_of_address")
        .part("file", pdf_part(PDF_BYTES.to_vec()));
    let r = http
        .post(requirement_document_url(&srv, org_id, number_id))
        .bearer_auth(&jwt)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "provider_rejected_value");
}

#[tokio::test]
async fn a_failed_link_after_a_successful_upload_is_a_gateway_failure_with_no_retry() {
    // D11: the document already reached Telnyx (it expires unlinked after 30 min); a
    // failure to attach it must not retry and must not claim a satisfied requirement.
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-doc-link-fail").await;
    srv.provider.fail_next(
        "submit_requirement_values",
        ProviderError::Unavailable {
            detail: "carrier outage".into(),
        },
    );

    let form = reqwest::multipart::Form::new()
        .text("requirement_id", "proof_of_address")
        .part("file", pdf_part(PDF_BYTES.to_vec()));
    let r = http
        .post(requirement_document_url(&srv, org_id, number_id))
        .bearer_auth(&jwt)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 502);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "document_link_failed");

    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM voip_requirement_documents WHERE requirement_id = 'proof_of_address'
          AND org_id = $1",
    )
    .bind(org_id)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(
        count, 0,
        "a failed link must not persist a document row nobody earned"
    );
}

#[tokio::test]
async fn uploading_a_document_for_a_number_the_org_does_not_own_is_a_404() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner_a, _) = user(&srv, "Owner A").await;
    let org_a = org(&srv, owner_a).await;
    let number_id = regulated_number(&srv, org_a, "so-doc-cross-org").await;
    let (owner_b, jwt_b) = user(&srv, "Owner B").await;
    let org_b = org(&srv, owner_b).await;

    let form = reqwest::multipart::Form::new()
        .text("requirement_id", "proof_of_address")
        .part("file", pdf_part(PDF_BYTES.to_vec()));
    let r = http
        .post(requirement_document_url(&srv, org_b, number_id))
        .bearer_auth(&jwt_b)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn a_non_admin_may_not_upload_documents() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-doc-member").await;
    let (_, member_jwt) = member(&srv, org_id, "member").await;

    let form = reqwest::multipart::Form::new()
        .text("requirement_id", "proof_of_address")
        .part("file", pdf_part(PDF_BYTES.to_vec()));
    let r = http
        .post(requirement_document_url(&srv, org_id, number_id))
        .bearer_auth(&member_jwt)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
}
// ---------------------------------------------------------------------------
// Document upload — size cap (task 6.6, D10) and MIME sniff (task 6.7, D10).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn uploading_past_the_ten_mib_cap_is_refused_413() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-doc-oversize").await;

    let mut oversized = b"%PDF-1.4 ".to_vec();
    oversized.resize(10 * 1024 * 1024 + 4096, b'a');
    let form = reqwest::multipart::Form::new()
        .text("requirement_id", "proof_of_address")
        .part("file", pdf_part(oversized));
    let r = http
        .post(requirement_document_url(&srv, org_id, number_id))
        .bearer_auth(&jwt)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "document_too_large");
    assert!(
        srv.provider.uploaded_documents().is_empty(),
        "an aborted upload must never be recorded as a completed one"
    );
}

#[tokio::test]
async fn a_declared_pdf_with_png_magic_bytes_is_refused_415() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-doc-spoofed").await;

    let form = reqwest::multipart::Form::new()
        .text("requirement_id", "proof_of_address")
        .part(
            "file",
            // Declared as `application/pdf`, but the bytes are a PNG — exactly the
            // spoof design D10's declared-type-AND-magic-bytes check exists to catch.
            reqwest::multipart::Part::bytes(PNG_BYTES.to_vec())
                .mime_str("application/pdf")
                .unwrap(),
        );
    let r = http
        .post(requirement_document_url(&srv, org_id, number_id))
        .bearer_auth(&jwt)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 415);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "document_type_unsupported");
    assert!(
        srv.provider.uploaded_documents().is_empty(),
        "a spoofed type must never reach the provider"
    );
}

#[tokio::test]
async fn an_unsupported_declared_type_is_refused_415() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let number_id = regulated_number(&srv, org_id, "so-doc-badtype").await;

    let form = reqwest::multipart::Form::new()
        .text("requirement_id", "proof_of_address")
        .part(
            "file",
            reqwest::multipart::Part::bytes(b"just some text".to_vec())
                .mime_str("text/plain")
                .unwrap(),
        );
    let r = http
        .post(requirement_document_url(&srv, org_id, number_id))
        .bearer_auth(&jwt)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 415);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "document_type_unsupported");
}
