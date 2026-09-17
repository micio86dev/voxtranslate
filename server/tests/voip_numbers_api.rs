//! Buying, routing and keeping telephone numbers (spec 0115).
//!
//! These are the endpoints that spend the organisation's money and decide where a
//! stranger's call lands, so nearly all of this is about refusals: who may look,
//! who may buy, and which misconfigurations are caught while somebody is looking
//! at the screen rather than at 3am.
//!
//! DB-gated: skipped without `DATABASE_URL`.

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::Client;
use serde_json::{json, Value};
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::{Config, VoipConfig};
use voxtranslate_server::db::{self, Pool};
use voxtranslate_server::telephony::mock::MockTelephonyProvider;
use voxtranslate_server::telephony::{OrderStatus, RequirementsStatus, SubOrderId, SubOrderState};
use voxtranslate_server::voip::regulatory;
use voxtranslate_server::{app, AppState};

const SECRET: &str = "voip-numbers-secret";

struct Server {
    addr: SocketAddr,
    pool: Pool,
}

/// `voip = false` reproduces a deployment with telephony switched off.
async fn setup_with(voip: bool) -> Option<Server> {
    let url = voxtranslate_server::db::test_database_url()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let mut config = Config::test_with_billing(&url, SECRET, 0.0);
    if voip {
        config.voip = Some(VoipConfig::test_default());
    }
    let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);
    let mut state = AppState::new(config);
    state.billing = Some(BillingService::new(pool.clone(), min_join));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);
    if voip {
        state.telephony = Some(Arc::new(MockTelephonyProvider::default()));
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

async fn org(srv: &Server, owner: Uuid, credits: i32) -> Uuid {
    let org_id: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id, subscription_status,
                                    current_period_end, credits_balance)
         VALUES ('Phone Co', $1, $2, 'active', now() + interval '30 days', $3) RETURNING id",
    )
    .bind(format!("vn-{}", Uuid::new_v4().simple()))
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

/// A distinct E.164. `voip_numbers.e164` is globally UNIQUE — a telephone number
/// belongs to one organisation — so every test needs its own.
fn fresh_e164() -> String {
    let n = Uuid::new_v4().as_u128() % 100_000_000;
    format!("+3902{n:08}")
}

/// A number the org already owns, inserted directly.
async fn own_number(srv: &Server, org_id: Uuid, e164: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO voip_numbers (org_id, provider, provider_number_id, e164, country, status)
         VALUES ($1, 'mock', $2, $3, 'IT', 'active') RETURNING id",
    )
    .bind(org_id)
    .bind(format!("prov-{}", Uuid::new_v4().simple()))
    .bind(e164)
    .fetch_one(&srv.pool)
    .await
    .unwrap()
}

fn numbers_url(srv: &Server, org_id: Uuid, suffix: &str) -> String {
    format!(
        "{}/api/business/organizations/{org_id}/voip/numbers{suffix}",
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

/// An org with an owner and one owned number.
async fn ready(srv: &Server) -> (Uuid, Uuid, String) {
    let (owner, jwt) = user(srv, "Owner").await;
    let org_id = org(srv, owner, 100_000).await;
    let number_id = own_number(srv, org_id, &fresh_e164()).await;
    (org_id, number_id, jwt)
}

/// The E.164 of a number this org owns.
async fn e164_of(srv: &Server, number_id: Uuid) -> String {
    sqlx::query_scalar("SELECT e164 FROM voip_numbers WHERE id = $1")
        .bind(number_id)
        .fetch_one(&srv.pool)
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// Listing and searching
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_organisations_own_numbers_are_listed_in_full() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, number_id, jwt) = ready(&srv).await;
    let e164 = e164_of(&srv, number_id).await;

    let r = http
        .get(numbers_url(&srv, org_id, ""))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    // Masking is about the RECIPIENT's number, never the org's own.
    assert_eq!(body["numbers"][0]["e164"], e164);
}

#[tokio::test]
async fn looking_is_a_member_action() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 100_000).await;
    let (_, member_jwt) = member(&srv, org_id, "member").await;

    let r = http
        .get(numbers_url(&srv, org_id, "/search?country=IT"))
        .bearer_auth(&member_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "a member may look at what is available");
}

#[tokio::test]
async fn a_search_quotes_the_customer_price_and_never_the_carrier_cost() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, _, jwt) = ready(&srv).await;

    let r = http
        .get(numbers_url(&srv, org_id, "/search?country=IT&limit=3"))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let offer = &body["offers"][0];
    assert!(offer["e164"].as_str().is_some(), "body: {body}");
    assert!(offer["monthly"].as_str().is_some());
    assert!(offer["setup"].as_str().is_some());
    // What the carrier charges us is ours: only the customer price crosses.
    assert!(offer.get("monthly_cost").is_none());
    assert!(offer.get("cost").is_none());
    // One offer in the set needs paperwork, so the state that blocks a number is
    // carried through rather than silently dropped.
    assert!(body["offers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|o| o["regulatory_requirement"].is_string()));
}

#[tokio::test]
async fn a_search_needs_a_country() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, _, jwt) = ready(&srv).await;

    let r = http
        .get(numbers_url(&srv, org_id, "/search"))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn with_telephony_switched_off_the_numbers_api_is_absent() {
    let srv = skip_without_db!(setup_with(false).await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 100_000).await;

    let r = http
        .get(numbers_url(&srv, org_id, "/search?country=IT"))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert!(r.status() == 404 || r.status() == 503, "got {}", r.status());
}

#[tokio::test]
async fn an_outsider_reaches_nothing() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, number_id, _) = ready(&srv).await;
    let (_, outsider) = user(&srv, "Outsider").await;

    for path in [
        String::new(),
        "/search?country=IT".to_string(),
        format!("/{number_id}/routing"),
        format!("/{number_id}/hours"),
    ] {
        let r = http
            .get(numbers_url(&srv, org_id, &path))
            .bearer_auth(&outsider)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404, "{path} should not confirm the org exists");
    }
}

// ---------------------------------------------------------------------------
// Buying — spending the organisation's money is an administrative act
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_member_may_not_spend_the_organisations_money() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 100_000).await;
    let (_, member_jwt) = member(&srv, org_id, "member").await;

    let r = http
        .post(numbers_url(&srv, org_id, ""))
        .bearer_auth(&member_jwt)
        .json(&json!({
            "e164": fresh_e164(),
            "country": "IT",
            "purchase_key": Uuid::new_v4(),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
}

/// The offer the mock scripts as needing paperwork — the second offer of any batch of at
/// least two (spec 0119 D4/D7's regulated purchase path).
async fn regulated_offer(srv: &Server, http: &Client, org_id: Uuid, jwt: &str) -> String {
    let body: Value = http
        .get(numbers_url(srv, org_id, "/search?country=IT&limit=3"))
        .bearer_auth(jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    body["offers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["regulatory_requirement"].is_string())
        .expect("the mock always scripts one regulated offer per search")["e164"]
        .as_str()
        .unwrap()
        .to_string()
}

/// The first offer of a batch — never the scripted regulated one.
async fn plain_offer(srv: &Server, http: &Client, org_id: Uuid, jwt: &str) -> String {
    let body: Value = http
        .get(numbers_url(srv, org_id, "/search?country=IT&limit=3"))
        .bearer_auth(jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    body["offers"][0]["e164"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn a_regulated_purchase_withholds_caller_id_and_keeps_the_orders_ids() {
    // D4: before this, a purchase was inserted `outbound_enabled = TRUE` regardless of
    // status, so `resolve_caller_id` would present a number the regulator had not
    // cleared. This proves the fix, and that the order/sub-order ids the sweep and
    // discovery need are actually persisted (spec 0119 R1/R7).
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 1_000_000).await;
    let e164 = regulated_offer(&srv, &http, org_id, &jwt).await;

    let r = http
        .post(numbers_url(&srv, org_id, ""))
        .bearer_auth(&jwt)
        .json(&json!({
            "e164": e164,
            "country": "IT",
            "purchase_key": Uuid::new_v4().to_string(),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["status"], "pending_regulatory");

    let row: (bool, Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT outbound_enabled, provider_order_id, provider_sub_order_id, number_kind
           FROM voip_numbers WHERE org_id = $1 AND e164 = $2",
    )
    .bind(org_id)
    .bind(&e164)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert!(
        !row.0,
        "a pending_regulatory number must never be presentable as caller id"
    );
    assert!(
        row.1.is_some(),
        "the order id must be persisted for the sweep and discovery to address"
    );
    assert!(
        row.2.is_some(),
        "the sub-order id must be persisted for the sweep and discovery to address"
    );
    assert!(row.3.is_some(), "the number kind must be persisted");
    // A regulated purchase now also schedules the sweep's first check (design D8, this
    // change). Left alone the row becomes "due" for real in ~2 minutes and could pollute
    // a LATER, unrelated `reconcile_due` test elsewhere in a long-running suite.
    sqlx::query("DELETE FROM voip_numbers WHERE org_id = $1 AND e164 = $2")
        .bind(org_id)
        .bind(&e164)
        .execute(&srv.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn an_unblocked_purchase_still_enables_caller_id() {
    // Triangulation: D4 must not simply hardcode `outbound_enabled = FALSE` — a purchase
    // that comes back `active` (no paperwork needed) still enables caller id immediately.
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 1_000_000).await;
    let e164 = plain_offer(&srv, &http, org_id, &jwt).await;

    let r = http
        .post(numbers_url(&srv, org_id, ""))
        .bearer_auth(&jwt)
        .json(&json!({
            "e164": e164,
            "country": "IT",
            "purchase_key": Uuid::new_v4().to_string(),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["status"], "active");

    let outbound_enabled: bool = sqlx::query_scalar(
        "SELECT outbound_enabled FROM voip_numbers WHERE org_id = $1 AND e164 = $2",
    )
    .bind(org_id)
    .bind(&e164)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert!(
        outbound_enabled,
        "an active number must be usable as caller id right away"
    );
}

// ---------------------------------------------------------------------------
// Scheduling the sweep's first check (spec 0119, design D8) — before this, nothing set
// `regulatory_next_check_at` at purchase time, so `reconcile_due` (which only ever picks
// rows with that column `IS NOT NULL`) never discovered a freshly bought regulated number
// until an admin opened the requirements panel: a Telnyx deadline cancellation never
// failed it, and an already-approved reusable group never auto-attached.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_regulated_purchase_schedules_the_sweeps_first_check() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 1_000_000).await;
    let e164 = regulated_offer(&srv, &http, org_id, &jwt).await;

    let before = chrono::Utc::now();
    let r = http
        .post(numbers_url(&srv, org_id, ""))
        .bearer_auth(&jwt)
        .json(&json!({
            "e164": e164,
            "country": "IT",
            "purchase_key": Uuid::new_v4().to_string(),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    let next_check: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT regulatory_next_check_at FROM voip_numbers WHERE org_id = $1 AND e164 = $2",
    )
    .bind(org_id)
    .bind(&e164)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    let scheduled =
        next_check.expect("a regulated purchase must be scheduled for the sweep's first look");
    let delta = (scheduled - before).num_seconds();
    assert!((90..=150).contains(&delta), "expected ~2min, got {delta}s");
    // Left alone, this row becomes "due" for real in ~2 minutes and would pollute a
    // LATER, separate invocation of this binary's `reconcile_due`-calling tests.
    sqlx::query("DELETE FROM voip_numbers WHERE org_id = $1 AND e164 = $2")
        .bind(org_id)
        .bind(&e164)
        .execute(&srv.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn an_unregulated_purchase_leaves_no_regulatory_check_scheduled() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 1_000_000).await;
    let e164 = plain_offer(&srv, &http, org_id, &jwt).await;

    let r = http
        .post(numbers_url(&srv, org_id, ""))
        .bearer_auth(&jwt)
        .json(&json!({
            "e164": e164,
            "country": "IT",
            "purchase_key": Uuid::new_v4().to_string(),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    let next_check: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT regulatory_next_check_at FROM voip_numbers WHERE org_id = $1 AND e164 = $2",
    )
    .bind(org_id)
    .bind(&e164)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert!(
        next_check.is_none(),
        "an unregulated purchase has nothing for the sweep to check"
    );
}

/// Speed a just-scheduled row's own ~2min initial check into "already due", so the test
/// can call `reconcile_due` synchronously instead of sleeping for real minutes.
async fn make_due_now(srv: &Server, number_id: Uuid) {
    sqlx::query(
        "UPDATE voip_numbers SET regulatory_next_check_at = now() - interval '1 second'
          WHERE id = $1",
    )
    .bind(number_id)
    .execute(&srv.pool)
    .await
    .unwrap();
}

/// `regulatory::reconcile_due` claims EVERY due row in the shared test database, not just
/// the one row a given test made due — unlike every other test in this file, which scopes
/// its own assertions to one `org_id`. Cargo runs this binary's tests concurrently against
/// the SAME database, so the two tests below racing each other (or a leftover row from an
/// earlier invocation of this same binary) would otherwise claim and count each other's
/// rows — the identical class of flake `voip::regulatory`'s own test suite hit and fixed
/// with this same pattern. This mutex serialises only the two tests that call
/// `reconcile_due`; every other test in the file is unaffected.
static SWEEP_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Delete a test's own number row once its assertions are done, so a row this test moved
/// to a future `regulatory_next_check_at` (e.g. `regulatory_review`, +5min) cannot become
/// "due" again and pollute a LATER, separate invocation of this same test binary.
async fn forget_number(srv: &Server, number_id: Uuid) {
    sqlx::query("DELETE FROM voip_numbers WHERE id = $1")
        .bind(number_id)
        .execute(&srv.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn after_purchase_the_sweep_attaches_an_already_approved_reusable_group() {
    // design D8: the reuse the owner actually asked for — "don't make a customer redo
    // paperwork a previous number in the same combination already cleared" — only works
    // end to end once buy() schedules the row for the sweep to find in the first place.
    let _guard = SWEEP_TEST_LOCK.lock().await;
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 1_000_000).await;
    let e164 = regulated_offer(&srv, &http, org_id, &jwt).await;

    let r = http
        .post(numbers_url(&srv, org_id, ""))
        .bearer_auth(&jwt)
        .json(&json!({
            "e164": e164,
            "country": "IT",
            "purchase_key": Uuid::new_v4().to_string(),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    let number_id: Uuid =
        sqlx::query_scalar("SELECT id FROM voip_numbers WHERE org_id = $1 AND e164 = $2")
            .bind(org_id)
            .bind(&e164)
            .fetch_one(&srv.pool)
            .await
            .unwrap();

    // `search_numbers` (no `kind` query param) defaults to `NumberKind::Local`, and the
    // search above asked for `country=IT` — matching the combination the reused group
    // below must key on.
    let group_row_id: Uuid = sqlx::query_scalar(
        "INSERT INTO voip_requirement_groups
            (org_id, provider, country, number_kind, action, provider_group_id, status)
         VALUES ($1, 'mock', 'IT', 'local', 'ordering', $2, 'approved') RETURNING id",
    )
    .bind(org_id)
    .bind(format!("mock-group-approved-{}", Uuid::new_v4().simple()))
    .fetch_one(&srv.pool)
    .await
    .unwrap();

    make_due_now(&srv, number_id).await;

    let provider = MockTelephonyProvider::default();
    let summary = regulatory::reconcile_due(&srv.pool, &provider, 25)
        .await
        .unwrap();
    // `>=`, not `==`: `reconcile_due` is a global scan across the whole shared test
    // database, and an unrelated regulated purchase made elsewhere in this suite can
    // legitimately become "due" and get swept up alongside this test's own row. The row
    // THIS test cares about is checked specifically below.
    assert!(
        summary.reconciled >= 1,
        "expected at least our own row: {summary:?}"
    );

    let (status, group_id): (String, Option<Uuid>) =
        sqlx::query_as("SELECT status, requirement_group_id FROM voip_numbers WHERE id = $1")
            .bind(number_id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert!(group_id.is_some(), "the reusable group must be linked");
    assert_eq!(
        status, "regulatory_review",
        "attaching a group starts the provider's review"
    );
    forget_number(&srv, number_id).await;
    sqlx::query("DELETE FROM voip_requirement_groups WHERE id = $1")
        .bind(group_row_id)
        .execute(&srv.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn after_purchase_a_provider_deadline_cancellation_fails_the_number_via_the_sweep() {
    let _guard = SWEEP_TEST_LOCK.lock().await;
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 1_000_000).await;
    let e164 = regulated_offer(&srv, &http, org_id, &jwt).await;

    let r = http
        .post(numbers_url(&srv, org_id, ""))
        .bearer_auth(&jwt)
        .json(&json!({
            "e164": e164,
            "country": "IT",
            "purchase_key": Uuid::new_v4().to_string(),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    let (number_id, sub_order_id): (Uuid, Option<String>) = sqlx::query_as(
        "SELECT id, provider_sub_order_id FROM voip_numbers WHERE org_id = $1 AND e164 = $2",
    )
    .bind(org_id)
    .bind(&e164)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    let sub_order_id =
        sub_order_id.expect("a regulated purchase always persists a sub-order id (D4)");

    make_due_now(&srv, number_id).await;

    let provider = MockTelephonyProvider::default();
    provider.set_sub_order_state(
        &SubOrderId(sub_order_id),
        SubOrderState {
            order: OrderStatus::Cancelled,
            requirements: RequirementsStatus::UnderReview,
            group: None,
        },
    );

    let summary = regulatory::reconcile_due(&srv.pool, &provider, 25)
        .await
        .unwrap();
    assert!(
        summary.reconciled >= 1,
        "expected at least our own row: {summary:?}"
    );

    let status: String = sqlx::query_scalar("SELECT status FROM voip_numbers WHERE id = $1")
        .bind(number_id)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert_eq!(
        status, "failed",
        "a provider deadline-miss cancellation must fail the number — reachable now that \
         buy() schedules the sweep's first check"
    );
    forget_number(&srv, number_id).await;
}

#[tokio::test]
async fn the_same_purchase_key_buys_the_same_number_once() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 1_000_000).await;
    let key = Uuid::new_v4().to_string();
    let wanted = fresh_e164();
    let body = json!({ "e164": wanted, "country": "IT", "purchase_key": key });

    // However many times a flaky network makes the client retry.
    let first = http
        .post(numbers_url(&srv, org_id, ""))
        .bearer_auth(&jwt)
        .json(&body)
        .send()
        .await
        .unwrap();
    let second = http
        .post(numbers_url(&srv, org_id, ""))
        .bearer_auth(&jwt)
        .json(&body)
        .send()
        .await
        .unwrap();

    if first.status().is_success() {
        assert!(second.status().is_success(), "a retry must not fail");
        let owned: i64 =
            sqlx::query_scalar("SELECT count(*) FROM voip_numbers WHERE org_id = $1 AND e164 = $2")
                .bind(org_id)
                .bind(&wanted)
                .fetch_one(&srv.pool)
                .await
                .unwrap();
        assert_eq!(owned, 1, "one number, however many retries");
    }
}

// ---------------------------------------------------------------------------
// Routing — where a stranger's call lands
// ---------------------------------------------------------------------------

#[tokio::test]
async fn routing_round_trips() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, number_id, jwt) = ready(&srv).await;
    let path = format!("/{number_id}/routing");

    let put = http
        .put(numbers_url(&srv, org_id, &path))
        .bearer_auth(&jwt)
        .json(&json!({
            "ring_mode": "owners",
            "no_answer_action": "voicemail",
            "ring_seconds": 30,
            "stranger_language": "it",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 204);

    let got: Value = http
        .get(numbers_url(&srv, org_id, &path))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["ring_mode"], "owners");
    assert_eq!(got["no_answer_action"], "voicemail");
    assert_eq!(got["ring_seconds"], 30);
    assert_eq!(got["stranger_language"], "it");
}

#[tokio::test]
async fn routing_is_replaced_rather_than_duplicated() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, number_id, jwt) = ready(&srv).await;
    let path = format!("/{number_id}/routing");

    for mode in ["owners", "users"] {
        let r = http
            .put(numbers_url(&srv, org_id, &path))
            .bearer_auth(&jwt)
            .json(&json!({ "ring_mode": mode, "no_answer_action": "refuse" }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 204);
    }

    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM voip_number_routing WHERE number_id = $1")
            .bind(number_id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(rows, 1, "one routing row per number");
}

#[tokio::test]
async fn a_ring_mode_or_action_that_does_not_exist_is_refused() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, number_id, jwt) = ready(&srv).await;
    let path = format!("/{number_id}/routing");

    let bad_mode = http
        .put(numbers_url(&srv, org_id, &path))
        .bearer_auth(&jwt)
        .json(&json!({ "ring_mode": "everyone", "no_answer_action": "refuse" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_mode.status(), 400);

    let bad_action = http
        .put(numbers_url(&srv, org_id, &path))
        .bearer_auth(&jwt)
        .json(&json!({ "ring_mode": "owners", "no_answer_action": "hang_up" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_action.status(), 400);
}

#[tokio::test]
async fn a_forward_with_nowhere_to_forward_to_is_refused_at_configuration_time() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, number_id, jwt) = ready(&srv).await;
    let path = format!("/{number_id}/routing");

    // A call that dies silently at the moment it matters most.
    let missing = http
        .put(numbers_url(&srv, org_id, &path))
        .bearer_auth(&jwt)
        .json(&json!({ "ring_mode": "owners", "no_answer_action": "forward" }))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 400);

    let not_a_number = http
        .put(numbers_url(&srv, org_id, &path))
        .bearer_auth(&jwt)
        .json(&json!({
            "ring_mode": "owners",
            "no_answer_action": "forward",
            "forward_to": "reception",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(not_a_number.status(), 400);

    let ok = http
        .put(numbers_url(&srv, org_id, &path))
        .bearer_auth(&jwt)
        .json(&json!({
            "ring_mode": "owners",
            "no_answer_action": "forward",
            "forward_to": "+390698765432",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 204);
}

#[tokio::test]
async fn the_ring_timeout_is_clamped_to_something_a_caller_will_wait_for() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, number_id, jwt) = ready(&srv).await;
    let path = format!("/{number_id}/routing");

    http.put(numbers_url(&srv, org_id, &path))
        .bearer_auth(&jwt)
        .json(&json!({
            "ring_mode": "owners",
            "no_answer_action": "refuse",
            "ring_seconds": 9999,
        }))
        .send()
        .await
        .unwrap();

    let got: Value = http
        .get(numbers_url(&srv, org_id, &path))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["ring_seconds"], 120);
}

#[tokio::test]
async fn changing_where_calls_go_is_an_administrative_act() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 100_000).await;
    let number_id = own_number(&srv, org_id, &fresh_e164()).await;
    let (_, member_jwt) = member(&srv, org_id, "member").await;
    let path = format!("/{number_id}/routing");

    // A member may read it …
    assert_eq!(
        http.get(numbers_url(&srv, org_id, &path))
            .bearer_auth(&member_jwt)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    // … and may not change it.
    assert_eq!(
        http.put(numbers_url(&srv, org_id, &path))
            .bearer_auth(&member_jwt)
            .json(&json!({ "ring_mode": "owners", "no_answer_action": "refuse" }))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
}

#[tokio::test]
async fn routing_a_number_the_org_does_not_own_is_a_404() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, _, jwt) = ready(&srv).await;
    let path = format!("/{}/routing", Uuid::new_v4());

    let r = http
        .put(numbers_url(&srv, org_id, &path))
        .bearer_auth(&jwt)
        .json(&json!({ "ring_mode": "owners", "no_answer_action": "refuse" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

// ---------------------------------------------------------------------------
// Opening hours
// ---------------------------------------------------------------------------

#[tokio::test]
async fn opening_hours_round_trip_and_pad_a_short_week_to_closed() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, number_id, jwt) = ready(&srv).await;
    let path = format!("/{number_id}/hours");

    let put = http
        .put(numbers_url(&srv, org_id, &path))
        .bearer_auth(&jwt)
        .json(&json!({
            "timezone": "Europe/Rome",
            "opens_at": [540, 540, 540],
            "closes_at": [1080, 1080, 1080],
            "closed_action": "voicemail",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 204);

    let got: Value = http
        .get(numbers_url(&srv, org_id, &path))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["timezone"], "Europe/Rome");
    // A short week is stored as what it means rather than left to interpretation.
    assert_eq!(got["opens_at"], json!([540, 540, 540, -1, -1, -1, -1]));
    assert_eq!(got["closes_at"], json!([1080, 1080, 1080, -1, -1, -1, -1]));
}

#[tokio::test]
async fn a_minute_outside_the_day_is_stored_as_closed() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, number_id, jwt) = ready(&srv).await;
    let path = format!("/{number_id}/hours");

    http.put(numbers_url(&srv, org_id, &path))
        .bearer_auth(&jwt)
        .json(&json!({
            "timezone": "UTC",
            "opens_at": [540, 1440, -5, 99999, 0, 0, 0],
            "closes_at": [1080, 1080, 1080, 1080, 1080, 1080, 1080],
            "closed_action": "refuse",
        }))
        .send()
        .await
        .unwrap();

    let got: Value = http
        .get(numbers_url(&srv, org_id, &path))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["opens_at"], json!([540, -1, -1, -1, 0, 0, 0]));
}

#[tokio::test]
async fn a_timezone_typo_is_refused_while_somebody_is_looking_at_it() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, number_id, jwt) = ready(&srv).await;
    let path = format!("/{number_id}/hours");

    // Rather than discovered at 3am by `is_open` falling back to UTC.
    let r = http
        .put(numbers_url(&srv, org_id, &path))
        .bearer_auth(&jwt)
        .json(&json!({
            "timezone": "Europe/Milano",
            "opens_at": [540, 540, 540, 540, 540, 540, 540],
            "closes_at": [1080, 1080, 1080, 1080, 1080, 1080, 1080],
            "closed_action": "refuse",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn a_closed_forward_with_nowhere_to_forward_to_is_refused() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, number_id, jwt) = ready(&srv).await;
    let path = format!("/{number_id}/hours");

    let missing = http
        .put(numbers_url(&srv, org_id, &path))
        .bearer_auth(&jwt)
        .json(&json!({
            "timezone": "UTC",
            "opens_at": [540, 540, 540, 540, 540, 540, 540],
            "closes_at": [1080, 1080, 1080, 1080, 1080, 1080, 1080],
            "closed_action": "forward",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 400);

    let bad = http
        .put(numbers_url(&srv, org_id, &path))
        .bearer_auth(&jwt)
        .json(&json!({
            "timezone": "UTC",
            "opens_at": [540, 540, 540, 540, 540, 540, 540],
            "closes_at": [1080, 1080, 1080, 1080, 1080, 1080, 1080],
            "closed_action": "forward",
            "closed_forward_to": "the front desk",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
}

#[tokio::test]
async fn clearing_the_hours_goes_back_to_always_open() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, number_id, jwt) = ready(&srv).await;
    let path = format!("/{number_id}/hours");

    http.put(numbers_url(&srv, org_id, &path))
        .bearer_auth(&jwt)
        .json(&json!({
            "timezone": "UTC",
            "opens_at": [540, 540, 540, 540, 540, 540, 540],
            "closes_at": [1080, 1080, 1080, 1080, 1080, 1080, 1080],
            "closed_action": "refuse",
        }))
        .send()
        .await
        .unwrap();

    let cleared = http
        .delete(numbers_url(&srv, org_id, &path))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert!(cleared.status().is_success(), "got {}", cleared.status());

    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM voip_business_hours WHERE number_id = $1")
            .bind(number_id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn setting_hours_is_an_administrative_act() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 100_000).await;
    let number_id = own_number(&srv, org_id, &fresh_e164()).await;
    let (_, member_jwt) = member(&srv, org_id, "member").await;
    let path = format!("/{number_id}/hours");

    assert_eq!(
        http.put(numbers_url(&srv, org_id, &path))
            .bearer_auth(&member_jwt)
            .json(&json!({
                "timezone": "UTC",
                "opens_at": [540, 540, 540, 540, 540, 540, 540],
                "closes_at": [1080, 1080, 1080, 1080, 1080, 1080, 1080],
                "closed_action": "refuse",
            }))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
}

// ---------------------------------------------------------------------------
// Verification and release
// ---------------------------------------------------------------------------

#[tokio::test]
async fn verifying_a_number_the_org_does_not_own_is_a_404() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, _, jwt) = ready(&srv).await;

    let r = http
        .post(numbers_url(
            &srv,
            org_id,
            &format!("/{}/verify", Uuid::new_v4()),
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn checking_a_verification_that_was_never_started_says_so() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, number_id, jwt) = ready(&srv).await;

    let r = http
        .post(numbers_url(
            &srv,
            org_id,
            &format!("/{number_id}/verify/check"),
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn verification_is_an_administrative_act() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 100_000).await;
    let number_id = own_number(&srv, org_id, &fresh_e164()).await;
    let (_, member_jwt) = member(&srv, org_id, "member").await;

    assert_eq!(
        http.post(numbers_url(&srv, org_id, &format!("/{number_id}/verify")))
            .bearer_auth(&member_jwt)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
}

#[tokio::test]
async fn releasing_a_number_the_org_does_not_own_is_a_404() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, _, jwt) = ready(&srv).await;

    let r = http
        .delete(numbers_url(&srv, org_id, &format!("/{}", Uuid::new_v4())))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn giving_a_number_back_is_an_administrative_act() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 100_000).await;
    let number_id = own_number(&srv, org_id, &fresh_e164()).await;
    let (_, member_jwt) = member(&srv, org_id, "member").await;

    assert_eq!(
        http.delete(numbers_url(&srv, org_id, &format!("/{number_id}")))
            .bearer_auth(&member_jwt)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
}

#[tokio::test]
async fn an_owner_can_give_a_number_back() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner, 100_000).await;
    let number_id = own_number(&srv, org_id, &fresh_e164()).await;

    let r = http
        .delete(numbers_url(&srv, org_id, &format!("/{number_id}")))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert!(
        r.status().is_success() || r.status() == 202,
        "got {} {}",
        r.status(),
        r.text().await.unwrap_or_default()
    );
}
