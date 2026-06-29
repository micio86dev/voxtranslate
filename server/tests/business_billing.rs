//! Integration tests for org billing (spec 0106, Phase 2 PR-C): the Stripe
//! webhook (signature, idempotent grants, subscription lifecycle) and the
//! subscribe/portal/purchase/credits endpoint guards. No live Stripe calls — the
//! webhook is driven with locally-signed events, and endpoint tests stop before
//! any outbound Stripe request. DB-gated.

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

const SECRET: &str = "biz-billing-secret";
const WHSEC: &str = "whsec_org_test";

struct Server {
    addr: SocketAddr,
    pool: db::Pool,
}

fn org_billing_cfg() -> OrgBillingConfig {
    OrgBillingConfig {
        webhook_secret: WHSEC.into(),
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
    }
}

async fn setup() -> Option<Server> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let mut config = Config::test_with_billing(&url, SECRET, 0.0);
    {
        let b = config.billing.as_mut().unwrap();
        b.stripe_secret_key = "sk_test_dummy".into();
        b.org_billing = Some(org_billing_cfg());
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
        name: "Biller".into(),
        avatar_url: None,
    };
    let u = upsert_google_user(
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

async fn make_org(srv: &Server, owner: Uuid) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id) VALUES ('Bill Co', $1, $2) RETURNING id",
    )
    .bind(format!("bill-{}", Uuid::new_v4().simple()))
    .bind(owner)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(id)
    .bind(owner)
    .execute(&srv.pool)
    .await
    .unwrap();
    id
}

/// POST a Stripe event to the org webhook, signed with the org webhook secret.
async fn post_event(http: &Client, srv: &Server, event: &Value) -> reqwest::Response {
    let body = serde_json::to_vec(event).unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let sig = stripe_handler::sign_payload(WHSEC, now, &body);
    http.post(format!("{}/api/business/stripe/webhook", base(srv)))
        .header("stripe-signature", sig)
        .body(body)
        .send()
        .await
        .unwrap()
}

async fn org_balance(srv: &Server, org: Uuid) -> i32 {
    sqlx::query_scalar("SELECT credits_balance FROM organizations WHERE id = $1")
        .bind(org)
        .fetch_one(&srv.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn webhook_signature_and_idempotent_purchase_grant() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, _) = user(&srv).await;
    let org = make_org(&srv, owner).await;

    let eid = format!("evt-{}", Uuid::new_v4());
    let event = json!({
        "id": eid,
        "type": "checkout.session.completed",
        "data": { "object": {
            "mode": "payment",
            "metadata": { "org_id": org.to_string(), "credits": "50" }
        }}
    });
    let body = serde_json::to_vec(&event).unwrap();

    // Bad signature → 400, nothing granted.
    let bad = http
        .post(format!("{}/api/business/stripe/webhook", base(&srv)))
        .header("stripe-signature", "t=1,v1=deadbeef")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    assert_eq!(org_balance(&srv, org).await, 0);

    // Valid signature → 200, +50 credits.
    assert_eq!(post_event(&http, &srv, &event).await.status(), 200);
    assert_eq!(org_balance(&srv, org).await, 50);

    // Replay same event id → 200, no double grant.
    assert_eq!(post_event(&http, &srv, &event).await.status(), 200);
    assert_eq!(org_balance(&srv, org).await, 50);
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM organization_credits_transactions WHERE org_id = $1",
    )
    .bind(org)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(rows, 1, "idempotent: exactly one ledger row");
}

#[tokio::test]
async fn subscription_lifecycle() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, _) = user(&srv).await;
    let org = make_org(&srv, owner).await;
    let customer = format!("cus_{}", Uuid::new_v4().simple());
    let subscription = format!("sub_{}", Uuid::new_v4().simple());

    // 1) Checkout completed (subscription) → link customer/subscription, active.
    post_event(
        &http,
        &srv,
        &json!({
            "id": format!("evt-{}", Uuid::new_v4()),
            "type": "checkout.session.completed",
            "data": { "object": {
                "mode": "subscription",
                "customer": customer,
                "subscription": subscription,
                "metadata": { "org_id": org.to_string(), "plan": "business", "interval": "month" }
            }}
        }),
    )
    .await;
    let (cust, sub, status, plan): (Option<String>, Option<String>, String, String) =
        sqlx::query_as(
            "SELECT stripe_customer_id, stripe_subscription_id, subscription_status, plan
             FROM organizations WHERE id = $1",
        )
        .bind(org)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert_eq!(cust.as_deref(), Some(customer.as_str()));
    assert_eq!(sub.as_deref(), Some(subscription.as_str()));
    assert_eq!(status, "active");
    assert_eq!(plan, "business");

    // 2) First invoice paid → grant 1000 (business monthly), period end set.
    let invoice = json!({
        "id": format!("evt-{}", Uuid::new_v4()),
        "type": "invoice.payment_succeeded",
        "data": { "object": {
            "customer": customer,
            "lines": { "data": [ { "price": { "id": "price_bm" }, "period": { "end": 1_735_689_600 } } ] }
        }}
    });
    assert_eq!(post_event(&http, &srv, &invoice).await.status(), 200);
    assert_eq!(org_balance(&srv, org).await, 1000);
    // Replay → no double grant.
    assert_eq!(post_event(&http, &srv, &invoice).await.status(), 200);
    assert_eq!(org_balance(&srv, org).await, 1000);

    // 3) Subscription updated → cancel-at-period-end flips on (auto-renew off).
    post_event(
        &http,
        &srv,
        &json!({
            "id": format!("evt-{}", Uuid::new_v4()),
            "type": "customer.subscription.updated",
            "data": { "object": {
                "id": subscription,
                "status": "active",
                "cancel_at_period_end": true,
                "current_period_end": 1_735_689_600
            }}
        }),
    )
    .await;
    let cancel: bool =
        sqlx::query_scalar("SELECT cancel_at_period_end FROM organizations WHERE id = $1")
            .bind(org)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert!(cancel, "cancel_at_period_end synced");

    // 4) Payment failed → past_due.
    post_event(
        &http,
        &srv,
        &json!({
            "id": format!("evt-{}", Uuid::new_v4()),
            "type": "invoice.payment_failed",
            "data": { "object": { "customer": customer } }
        }),
    )
    .await;
    assert_eq!(sub_status(&srv, org).await, "past_due");

    // 5) Subscription deleted → canceled.
    post_event(
        &http,
        &srv,
        &json!({
            "id": format!("evt-{}", Uuid::new_v4()),
            "type": "customer.subscription.deleted",
            "data": { "object": { "id": subscription } }
        }),
    )
    .await;
    assert_eq!(sub_status(&srv, org).await, "canceled");
}

async fn sub_status(srv: &Server, org: Uuid) -> String {
    sqlx::query_scalar("SELECT subscription_status FROM organizations WHERE id = $1")
        .bind(org)
        .fetch_one(&srv.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn billing_endpoint_guards() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner).await;

    // No token → 401.
    let anon = http
        .post(format!(
            "{}/api/business/organizations/{org}/subscription",
            base(&srv)
        ))
        .json(&json!({ "plan": "business", "interval": "month" }))
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);

    // Owner, bad plan → 400 (validation before any Stripe call).
    let bad_plan = http
        .post(format!(
            "{}/api/business/organizations/{org}/subscription",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .json(&json!({ "plan": "gold", "interval": "month" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_plan.status(), 400);

    // Portal before any subscription → 409 (no Stripe customer yet).
    let portal = http
        .post(format!(
            "{}/api/business/organizations/{org}/subscription/portal",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(portal.status(), 409);

    // Purchase with a non-positive amount → 400.
    let bad_amount = http
        .post(format!(
            "{}/api/business/organizations/{org}/credits/purchase",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .json(&json!({ "credits_amount": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_amount.status(), 400);

    // Credits view: owner sees balance + ledger; a non-member is 404.
    let view: Value = http
        .get(format!(
            "{}/api/business/organizations/{org}/credits",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(view["balance"], 0);
    assert!(view["transactions"].is_array());

    let (_outsider, jwt_out) = user(&srv).await;
    let denied = http
        .get(format!(
            "{}/api/business/organizations/{org}/credits",
            base(&srv)
        ))
        .bearer_auth(&jwt_out)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 404);
}

/// `POST /api/admin/org/gift-subscription` — the Directus "Gift subscription" Flow
/// (secret-guarded, server-to-server). Covers the secret guard, validation, the
/// blank-input defaults (Directus sends numeric fields as quoted strings → "" =
/// use the plan package default), the credit grant + ledger row + active period,
/// and the Stripe-managed refusal. `admin_api_secret` is `test-admin-secret` (set
/// by `Config::test_with_billing`).
#[tokio::test]
async fn admin_gift_subscription() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, _) = user(&srv).await;
    let org = make_org(&srv, owner).await;
    let url = format!("{}/api/admin/org/gift-subscription", base(&srv));

    // Wrong secret → 403, nothing changes.
    let bad = http
        .post(&url)
        .header("x-admin-secret", "nope")
        .json(&json!({ "org_id": org, "plan": "business" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 403);
    assert_eq!(org_balance(&srv, org).await, 0);

    // Bad plan → 400; months out of range → 400.
    for body in [
        json!({ "org_id": org, "plan": "gold" }),
        json!({ "org_id": org, "plan": "business", "months": 0 }),
    ] {
        let r = http
            .post(&url)
            .header("x-admin-secret", "test-admin-secret")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "rejected {body}");
    }

    // Unknown org → 404 (no row to gift).
    let missing = http
        .post(&url)
        .header("x-admin-secret", "test-admin-secret")
        .json(&json!({ "org_id": Uuid::new_v4(), "plan": "business" }))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);

    // Default gift: enterprise, blank months/credits sent the way Directus ACTUALLY
    // renders an unfilled confirmation field — the literal string "null", not "".
    // months → 1, credits → enterprise monthly (5000). (Regression: this used to
    // 422 with `months: invalid digit found in string`.)
    let resp = http
        .post(&url)
        .header("x-admin-secret", "test-admin-secret")
        .json(&json!({ "org_id": org, "plan": "enterprise", "months": "null", "credits": "null", "message": "welcome!" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["plan"], "enterprise");
    assert_eq!(body["months"], 1);
    assert_eq!(body["credits_granted"], 5000);
    assert_eq!(body["balance"], 5000);

    // The org row is now an active, ~1-month, non-renewing enterprise gift, and a
    // single 'gift' ledger row was written.
    let (plan, status, future, cancel): (String, String, bool, bool) = sqlx::query_as(
        "SELECT plan, subscription_status, current_period_end > now(), cancel_at_period_end
         FROM organizations WHERE id = $1",
    )
    .bind(org)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(plan, "enterprise");
    assert_eq!(status, "active");
    assert!(future, "period end is in the future");
    assert!(cancel, "a gift never auto-renews");
    let gift_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM organization_credits_transactions WHERE org_id = $1 AND type = 'gift'",
    )
    .bind(org)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(gift_rows, 1);

    // Explicit credits + multi-month: business, 3 months, 250 credits → +250 and the
    // plan flips to business (period extends past the prior gift).
    let resp2 = http
        .post(&url)
        .header("x-admin-secret", "test-admin-secret")
        .json(&json!({ "org_id": org, "plan": "business", "months": "3", "credits": "250" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp2.status(), 200);
    let body2: Value = resp2.json().await.unwrap();
    assert_eq!(body2["months"], 3);
    assert_eq!(body2["credits_granted"], 250);
    assert_eq!(body2["balance"], 5250);
    assert_eq!(org_balance(&srv, org).await, 5250);

    // A live Stripe subscription blocks gifting (409) so a gift can't desync billing.
    sqlx::query("UPDATE organizations SET stripe_subscription_id = 'sub_live' WHERE id = $1")
        .bind(org)
        .execute(&srv.pool)
        .await
        .unwrap();
    let conflict = http
        .post(&url)
        .header("x-admin-secret", "test-admin-secret")
        .json(&json!({ "org_id": org, "plan": "business" }))
        .send()
        .await
        .unwrap();
    assert_eq!(conflict.status(), 409);
    assert_eq!(org_balance(&srv, org).await, 5250, "no grant on conflict");
}
