//! Invoice index + download authorisation (spec 0109), and the regression that
//! spec 0109 made reachable: with `invoice_creation` now enabled, one-off credit
//! top-ups also emit `invoice.payment_succeeded`, which used to be treated as a
//! subscription renewal and granted a free month of credits.
//!
//! Webhooks are driven with locally-signed events — no live Stripe. The PDF
//! endpoints are exercised only up to the authorisation boundary, since crossing
//! it would call Stripe. DB-gated: no-ops when `DATABASE_URL` is unset.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Client;
use serde_json::{json, Value};
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::{Config, OrgBillingConfig};
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::{app, db, stripe_handler, AppState};

const SECRET: &str = "invoice-test-secret";
const ORG_WHSEC: &str = "whsec_org_invoices";
const USER_WHSEC: &str = "whsec_user_invoices";

struct Server {
    addr: SocketAddr,
    pool: db::Pool,
}

async fn setup() -> Option<Server> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let mut config = Config::test_with_billing(&url, SECRET, 0.0);
    {
        let b = config.billing.as_mut().unwrap();
        b.stripe_secret_key = "sk_test_dummy".into();
        b.stripe_webhook_secret = USER_WHSEC.into();
        b.org_billing = Some(OrgBillingConfig {
            webhook_secret: ORG_WHSEC.into(),
            success_url: "https://app.test/s".into(),
            cancel_url: "https://app.test/c".into(),
            portal_return_url: "https://app.test/p".into(),
            credit_unit_amount_cents: 100,
            business_monthly_price_id: "price_bm".into(),
            business_annual_price_id: "price_ba".into(),
            enterprise_monthly_price_id: "price_em".into(),
            enterprise_annual_price_id: "price_ea".into(),
            business_monthly_credits: 1000,
            enterprise_monthly_credits: 5000,
        });
    }
    let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);
    let mut state = AppState::new(config);
    state.billing = Some(BillingService::new(pool.clone(), min_join));
    state.safety = Some(SafetyService::new(pool.clone()));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    Some(Server { addr, pool })
}

fn base(srv: &Server) -> String {
    format!("http://{}", srv.addr)
}

async fn user(srv: &Server) -> (Uuid, String) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: "Invoicee".into(),
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

async fn make_org(srv: &Server, owner: Uuid, role: &str) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id) VALUES ('Inv Co', $1, $2) RETURNING id",
    )
    .bind(format!("inv-{}", Uuid::new_v4().simple()))
    .bind(owner)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(owner)
        .bind(role)
        .execute(&srv.pool)
        .await
        .unwrap();
    id
}

async fn add_member(srv: &Server, org: Uuid, user_id: Uuid, role: &str) {
    sqlx::query("INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, $3)")
        .bind(org)
        .bind(user_id)
        .bind(role)
        .execute(&srv.pool)
        .await
        .unwrap();
}

async fn post_event(http: &Client, srv: &Server, path: &str, secret: &str, event: &Value) -> u16 {
    let body = serde_json::to_vec(event).unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let sig = stripe_handler::sign_payload(secret, now, &body);
    http.post(format!("{}{path}", base(srv)))
        .header("stripe-signature", sig)
        .body(body)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn org_balance(srv: &Server, org: Uuid) -> i32 {
    sqlx::query_scalar("SELECT credits_balance FROM organizations WHERE id = $1")
        .bind(org)
        .fetch_one(&srv.pool)
        .await
        .unwrap()
}

/// Seed an invoice row directly — the read paths are what these tests exercise,
/// and going through Stripe to create one is not possible offline.
async fn seed_invoice(
    srv: &Server,
    owner_user: Option<Uuid>,
    owner_org: Option<Uuid>,
    iso: &str,
) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO invoices (user_id, org_id, stripe_invoice_id, number, issued_at,
                               subtotal_cents, tax_cents, total_cents, currency, status)
         VALUES ($1, $2, $3, $4, $5::timestamptz, 1000, 220, 1220, 'eur', 'paid')
         RETURNING id",
    )
    .bind(owner_user)
    .bind(owner_org)
    .bind(format!("in_{}", Uuid::new_v4().simple()))
    .bind("VOX-1")
    .bind(iso)
    .fetch_one(&srv.pool)
    .await
    .unwrap()
}

// ---------------------------------------------------------------------------
// The regression spec 0109 introduced the risk of
// ---------------------------------------------------------------------------

/// A top-up invoice reaches `invoice.payment_succeeded` now that top-ups are
/// invoiced. It must record the document and grant NOTHING — the credits were
/// already granted by the checkout event.
#[tokio::test]
async fn topup_invoice_does_not_grant_subscription_credits() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, _) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    let customer = format!("cus_{}", Uuid::new_v4().simple());

    // The top-up checkout: grants 50 credits and claims the Stripe customer.
    let status = post_event(
        &http,
        &srv,
        "/api/business/stripe/webhook",
        ORG_WHSEC,
        &json!({
            "id": format!("evt-{}", Uuid::new_v4()),
            "type": "checkout.session.completed",
            "data": { "object": {
                "mode": "payment",
                "payment_status": "paid",
                "customer": customer,
                "metadata": { "org_id": org.to_string(), "credits": "50" }
            }}
        }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(org_balance(&srv, org).await, 50);

    // The invoice Stripe finalises for that same top-up. No `subscription` link,
    // and an inline price with no id — exactly the shape that used to fall
    // through to the monthly-grant default.
    let status = post_event(
        &http,
        &srv,
        "/api/business/stripe/webhook",
        ORG_WHSEC,
        &json!({
            "id": format!("evt-{}", Uuid::new_v4()),
            "type": "invoice.payment_succeeded",
            "data": { "object": {
                "id": format!("in_{}", Uuid::new_v4().simple()),
                "customer": customer,
                "number": "VOX-0002",
                "created": 1_780_000_000,
                "status": "paid",
                "currency": "usd",
                "subtotal": 5000, "total": 5000,
                "lines": { "data": [ { "price": { "id": null } } ] }
            }}
        }),
    )
    .await;
    assert_eq!(status, 200);

    assert_eq!(
        org_balance(&srv, org).await,
        50,
        "a top-up invoice must not also grant a month of subscription credits"
    );

    // …but the document itself IS recorded.
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM invoices WHERE org_id = $1")
        .bind(org)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "the top-up invoice is still indexed");
}

/// The subscription path must keep working: a real renewal invoice still grants.
#[tokio::test]
async fn subscription_invoice_still_grants_and_is_recorded() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, _) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    let customer = format!("cus_{}", Uuid::new_v4().simple());

    sqlx::query("UPDATE organizations SET stripe_customer_id = $2 WHERE id = $1")
        .bind(org)
        .bind(&customer)
        .execute(&srv.pool)
        .await
        .unwrap();

    let status = post_event(
        &http,
        &srv,
        "/api/business/stripe/webhook",
        ORG_WHSEC,
        &json!({
            "id": format!("evt-{}", Uuid::new_v4()),
            "type": "invoice.payment_succeeded",
            "data": { "object": {
                "id": format!("in_{}", Uuid::new_v4().simple()),
                "customer": customer,
                "subscription": "sub_123",
                "number": "VOX-0003",
                "created": 1_780_000_000,
                "status": "paid",
                "currency": "usd",
                "subtotal": 9900, "total": 9900,
                "lines": { "data": [ { "price": { "id": "price_bm" }, "period": { "end": 1_782_000_000 } } ] }
            }}
        }),
    )
    .await;
    assert_eq!(status, 200);

    assert_eq!(
        org_balance(&srv, org).await,
        1000,
        "business monthly renewal grants its monthly credits"
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM invoices WHERE org_id = $1")
        .bind(org)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

/// R1: a redelivered invoice event updates in place instead of duplicating.
#[tokio::test]
async fn replayed_invoice_event_does_not_duplicate_the_row() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, _) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    let customer = format!("cus_{}", Uuid::new_v4().simple());
    let invoice_id = format!("in_{}", Uuid::new_v4().simple());

    sqlx::query("UPDATE organizations SET stripe_customer_id = $2 WHERE id = $1")
        .bind(org)
        .bind(&customer)
        .execute(&srv.pool)
        .await
        .unwrap();

    // Same invoice, two distinct events: finalized (open) then paid.
    let finalized = json!({
        "id": format!("evt-{}", Uuid::new_v4()),
        "type": "invoice.finalized",
        "data": { "object": {
            "id": invoice_id, "customer": customer, "number": "VOX-0004",
            "created": 1_780_000_000, "status": "open",
            "currency": "eur", "subtotal": 1000, "total": 1220
        }}
    });
    let paid = json!({
        "id": format!("evt-{}", Uuid::new_v4()),
        "type": "invoice.payment_succeeded",
        "data": { "object": {
            "id": invoice_id, "customer": customer, "number": "VOX-0004",
            "created": 1_780_000_000, "status": "paid",
            "currency": "eur", "subtotal": 1000, "total": 1220,
            "lines": { "data": [ { "price": { "id": null } } ] }
        }}
    });

    for ev in [&finalized, &paid, &finalized] {
        assert_eq!(
            post_event(&http, &srv, "/api/business/stripe/webhook", ORG_WHSEC, ev).await,
            200
        );
    }

    let rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT count(*), max(status) FROM invoices WHERE org_id = $1")
            .bind(org)
            .fetch_all(&srv.pool)
            .await
            .unwrap();
    assert_eq!(rows[0].0, 1, "one row for one Stripe invoice");
}

// ---------------------------------------------------------------------------
// R2 / R3 — listing and authorisation
// ---------------------------------------------------------------------------

/// R2 + R5: a consumer sees only their own invoices, grouped newest month first.
#[tokio::test]
async fn consumer_sees_only_their_own_invoices_grouped_by_month() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (alice, alice_jwt) = user(&srv).await;
    let (bob, _) = user(&srv).await;

    seed_invoice(&srv, Some(alice), None, "2026-08-03T10:00:00Z").await;
    seed_invoice(&srv, Some(alice), None, "2026-07-28T10:00:00Z").await;
    seed_invoice(&srv, Some(alice), None, "2026-07-02T10:00:00Z").await;
    seed_invoice(&srv, Some(bob), None, "2026-08-05T10:00:00Z").await;

    let body: Value = http
        .get(format!("{}/api/billing/invoices", base(&srv)))
        .bearer_auth(&alice_jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let months = body["months"].as_array().unwrap();
    assert_eq!(months.len(), 2, "August and July");
    assert_eq!(months[0]["month"], "2026-08");
    assert_eq!(months[0]["invoices"].as_array().unwrap().len(), 1);
    assert_eq!(months[1]["month"], "2026-07");
    assert_eq!(
        months[1]["invoices"].as_array().unwrap().len(),
        2,
        "Bob's invoice never appears in Alice's list"
    );
}

/// R2: another user's invoice is indistinguishable from a non-existent one.
#[tokio::test]
async fn another_users_invoice_is_not_downloadable_or_enumerable() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (alice, _) = user(&srv).await;
    let (_bob, bob_jwt) = user(&srv).await;
    let alice_invoice = seed_invoice(&srv, Some(alice), None, "2026-08-03T10:00:00Z").await;

    let owned = http
        .get(format!(
            "{}/api/billing/invoices/{alice_invoice}/pdf",
            base(&srv)
        ))
        .bearer_auth(&bob_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(owned.status(), 404);

    // A random id gives the SAME answer — so ids cannot be probed for existence.
    let missing = http
        .get(format!(
            "{}/api/billing/invoices/{}/pdf",
            base(&srv),
            Uuid::new_v4()
        ))
        .bearer_auth(&bob_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
}

#[tokio::test]
async fn invoice_endpoints_require_authentication() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let r = Client::new()
        .get(format!("{}/api/billing/invoices", base(&srv)))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

/// R3: org invoices are admin/owner-only — a plain member is refused.
#[tokio::test]
async fn org_invoices_are_admin_only() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, owner_jwt) = user(&srv).await;
    let (member, member_jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    add_member(&srv, org, member, "member").await;
    seed_invoice(&srv, None, Some(org), "2026-08-03T10:00:00Z").await;

    let denied = http
        .get(format!(
            "{}/api/business/organizations/{org}/invoices",
            base(&srv)
        ))
        .bearer_auth(&member_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 403);

    let allowed = http
        .get(format!(
            "{}/api/business/organizations/{org}/invoices",
            base(&srv)
        ))
        .bearer_auth(&owner_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(allowed.status(), 200);
    let body: Value = allowed.json().await.unwrap();
    assert_eq!(body["months"].as_array().unwrap().len(), 1);
}

/// An org admin cannot reach another org's invoice by id: the role check fires
/// before the ownership check, and a non-member gets 404 rather than 403 — the
/// existence of someone else's org is not something to confirm.
#[tokio::test]
async fn org_admin_cannot_reach_another_orgs_invoice() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner_a, _) = user(&srv).await;
    let (owner_b, jwt_b) = user(&srv).await;
    let org_a = make_org(&srv, owner_a, "owner").await;
    let org_b = make_org(&srv, owner_b, "owner").await;
    let invoice_a = seed_invoice(&srv, None, Some(org_a), "2026-08-03T10:00:00Z").await;

    // Asking under org A → 404: to a non-member, org A does not exist.
    let cross = http
        .get(format!(
            "{}/api/business/organizations/{org_a}/invoices/{invoice_a}/pdf",
            base(&srv)
        ))
        .bearer_auth(&jwt_b)
        .send()
        .await
        .unwrap();
    assert_eq!(cross.status(), 404);

    // Asking under their OWN org for A's invoice id → 404, never A's document.
    let laundered = http
        .get(format!(
            "{}/api/business/organizations/{org_b}/invoices/{invoice_a}/pdf",
            base(&srv)
        ))
        .bearer_auth(&jwt_b)
        .send()
        .await
        .unwrap();
    assert_eq!(laundered.status(), 404);
}
