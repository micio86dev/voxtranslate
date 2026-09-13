//! Every call this server makes TO Stripe, against a stand-in.
//!
//! `stripe_webhook.rs` covers what arrives from Stripe; this is the other
//! direction — checkout, the billing portal, and the three read endpoints the
//! dashboard and the marketing site depend on. `BillingConfig::stripe_base_url`
//! is what makes any of it reachable: nothing here can run against the real API
//! without creating real customers.
//!
//! No database needed: these are plain functions over an HTTP client.

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use uuid::Uuid;
use voxtranslate_server::config::{BillingConfig, Config, CreditPackage, OrgBillingConfig};
use voxtranslate_server::stripe_handler as stripe;

// ---------------------------------------------------------------------------
// A stand-in for Stripe
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Stripe {
    /// Form bodies received, in order.
    posted: Arc<Mutex<Vec<String>>>,
    /// Query strings received on GETs.
    queried: Arc<Mutex<Vec<HashMap<String, String>>>>,
    fail: Arc<AtomicU16>,
}

fn failing(s: &Stripe) -> Option<axum::response::Response> {
    let code = s.fail.load(Ordering::SeqCst);
    (code != 0).then(|| (StatusCode::from_u16(code).unwrap(), "stripe is down").into_response())
}

async fn checkout(State(s): State<Stripe>, body: String) -> axum::response::Response {
    if let Some(r) = failing(&s) {
        return r;
    }
    s.posted.lock().unwrap().push(body);
    Json(json!({ "id": "cs_test_1", "url": "https://checkout.stripe.test/pay/cs_test_1" }))
        .into_response()
}

async fn portal(State(s): State<Stripe>, body: String) -> axum::response::Response {
    if let Some(r) = failing(&s) {
        return r;
    }
    s.posted.lock().unwrap().push(body);
    Json(json!({ "url": "https://billing.stripe.test/p/session/abc" })).into_response()
}

async fn subscription(State(s): State<Stripe>) -> axum::response::Response {
    if let Some(r) = failing(&s) {
        return r;
    }
    // A modern Stripe payload: the period moved onto the item, and the default
    // payment method is expanded.
    Json(json!({
        "status": "active",
        "start_date": 1_700_000_000,
        "cancel_at_period_end": true,
        "cancel_at": 1_800_000_000,
        "canceled_at": 0,
        "items": { "data": [{
            "current_period_start": 1_750_000_000,
            "current_period_end": 1_752_000_000,
            "price": {
                "unit_amount": 4900,
                "currency": "eur",
                "recurring": { "interval": "month", "interval_count": 1 }
            }
        }]},
        "default_payment_method": { "card": {
            "brand": "visa", "last4": "4242", "exp_month": 11, "exp_year": 2030
        }},
    }))
    .into_response()
}

async fn price(State(s): State<Stripe>) -> axum::response::Response {
    if let Some(r) = failing(&s) {
        return r;
    }
    Json(json!({
        "unit_amount": 19900,
        "currency": "eur",
        "active": true,
        "recurring": { "interval": "year", "interval_count": 1 },
    }))
    .into_response()
}

async fn invoice(State(s): State<Stripe>) -> axum::response::Response {
    if let Some(r) = failing(&s) {
        return r;
    }
    Json(json!({ "id": "in_1", "invoice_pdf": "https://files.stripe.test/in_1.pdf" }))
        .into_response()
}

async fn invoices(
    State(s): State<Stripe>,
    Query(q): Query<HashMap<String, String>>,
) -> axum::response::Response {
    if let Some(r) = failing(&s) {
        return r;
    }
    s.queried.lock().unwrap().push(q);
    Json(json!({ "data": [{ "id": "in_2" }, { "id": "in_1" }] })).into_response()
}

async fn mock_stripe() -> (String, Stripe) {
    let s = Stripe::default();
    let router = Router::new()
        .route("/v1/checkout/sessions", post(checkout))
        .route("/v1/billing_portal/sessions", post(portal))
        .route("/v1/subscriptions/{id}", get(subscription))
        .route("/v1/prices/{id}", get(price))
        .route("/v1/invoices", get(invoices))
        .route("/v1/invoices/{id}", get(invoice))
        .with_state(s.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{addr}"), s)
}

fn pack() -> CreditPackage {
    CreditPackage {
        id: "pack-20".into(),
        name: "20 dollars".into(),
        price_usd: 20.0,
        credits_usd: 20.0,
        stripe_price_id: "price_pack_20".into(),
    }
}

/// A billing config pointed at the stand-in.
async fn setup() -> (reqwest::Client, BillingConfig, OrgBillingConfig, Stripe) {
    let (base, mock) = mock_stripe().await;
    let mut cfg = Config::test_with_billing("postgres://unused", "secret", 0.0)
        .billing
        .expect("test config carries billing");
    cfg.stripe_base_url = base;
    cfg.stripe_secret_key = "sk_test_dummy".into();
    cfg.stripe_success_url = "https://app.test/ok".into();
    cfg.stripe_cancel_url = "https://app.test/no".into();
    let org = OrgBillingConfig {
        webhook_secret: String::new(),
        success_url: "https://dash.test/ok".into(),
        cancel_url: "https://dash.test/no".into(),
        portal_return_url: "https://dash.test/billing".into(),
        credit_unit_amount_cents: 100,
        business_monthly_price_id: "price_biz_m".into(),
        business_annual_price_id: "price_biz_y".into(),
        enterprise_monthly_price_id: "price_ent_m".into(),
        enterprise_annual_price_id: "price_ent_y".into(),
        business_monthly_credits: 500,
        enterprise_monthly_credits: 2500,
    };
    (reqwest::Client::new(), cfg, org, mock)
}

// ---------------------------------------------------------------------------
// Checkout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_consumer_top_up_becomes_a_hosted_checkout_url() {
    let (http, cfg, _org, mock) = setup().await;
    let user = Uuid::new_v4();

    let url = stripe::create_checkout_session(&http, &cfg, &pack(), &user, None)
        .await
        .expect("checkout");
    assert_eq!(url, "https://checkout.stripe.test/pay/cs_test_1");

    // The user id travels as metadata, and it is the ONLY thing that lets the
    // webhook credit the right account when the payment lands minutes later.
    let body = mock.posted.lock().unwrap()[0].clone();
    assert!(body.contains(&user.to_string()), "no user id in: {body}");
    assert!(body.contains("success_url"), "{body}");
}

#[tokio::test]
async fn an_org_subscription_checkout_names_the_plan_it_is_selling() {
    let (http, cfg, org, mock) = setup().await;
    let org_id = Uuid::new_v4();

    let url = stripe::create_org_subscription_checkout(
        &http,
        &cfg,
        &org,
        &org_id,
        &org.business_annual_price_id,
        "business",
        "annual",
    )
    .await
    .expect("checkout");
    assert!(url.starts_with("https://checkout.stripe.test/"));

    let body = mock.posted.lock().unwrap()[0].clone();
    // The annual price, not the monthly one: picking the wrong id here charges a
    // customer twelve times what they agreed to, or a twelfth.
    assert!(body.contains("price_biz_y"), "{body}");
    assert!(body.contains(&org_id.to_string()), "{body}");
}

#[tokio::test]
async fn a_credit_purchase_carries_the_quantity_and_the_org() {
    let (http, cfg, org, mock) = setup().await;
    let org_id = Uuid::new_v4();

    let url = stripe::create_org_purchase_checkout(&http, &cfg, &org, &org_id, 250, Some("cus_9"))
        .await
        .expect("checkout");
    assert!(url.starts_with("https://checkout.stripe.test/"));

    let body = mock.posted.lock().unwrap()[0].clone();
    assert!(body.contains("250"), "{body}");
    assert!(body.contains(&org_id.to_string()), "{body}");
    // An existing customer is reused rather than duplicated — two customer records
    // for one org split its invoice history in half.
    assert!(body.contains("cus_9"), "{body}");
}

#[tokio::test]
async fn the_billing_portal_returns_the_customer_to_where_they_came_from() {
    let (http, cfg, org, mock) = setup().await;

    let url = stripe::create_portal_session(&http, &cfg, &org, "cus_42")
        .await
        .expect("portal");
    assert_eq!(url, "https://billing.stripe.test/p/session/abc");

    let body = mock.posted.lock().unwrap()[0].clone();
    assert!(body.contains("cus_42"), "{body}");
    assert!(body.contains("billing"), "no return_url in: {body}");
}

#[tokio::test]
async fn stripe_refusing_a_checkout_is_an_error_not_a_broken_url() {
    let (http, cfg, org, mock) = setup().await;
    mock.fail.store(402, Ordering::SeqCst);

    assert!(
        stripe::create_checkout_session(&http, &cfg, &pack(), &Uuid::new_v4(), None)
            .await
            .is_err()
    );
    assert!(stripe::create_portal_session(&http, &cfg, &org, "cus_1")
        .await
        .is_err());
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_subscription_is_flattened_into_what_the_dashboard_shows() {
    let (http, cfg, _org, _mock) = setup().await;

    let s = stripe::get_subscription(&http, &cfg, "sub_1")
        .await
        .expect("subscription");

    assert_eq!(s["stripe_status"], "active");
    assert_eq!(s["cancel_at_period_end"], true);
    assert_eq!(s["amount"], 4900);
    assert_eq!(s["currency"], "eur");
    assert_eq!(s["interval"], "month");
    assert_eq!(s["card_brand"], "visa");
    assert_eq!(s["card_last4"], "4242");

    // Newer API versions moved the period onto the item; reading only the
    // top-level fields showed every customer a blank renewal date.
    assert!(
        s["current_period_end"].as_str().is_some(),
        "the period fell off the item"
    );
    // Unix seconds become RFC3339 so they overlay on our own DB view.
    assert!(s["start_date"].as_str().unwrap().contains('T'));
    // A zero timestamp is "never", not 1970.
    assert!(s["canceled_at"].is_null(), "0 was read as a real date");
}

#[tokio::test]
async fn a_price_is_read_from_stripe_rather_than_retyped() {
    let (http, cfg, _org, _mock) = setup().await;

    let p = stripe::get_price(&http, &cfg, "price_biz_y")
        .await
        .expect("price");

    // The currency lives on the Price and nowhere else. The marketing site quoted
    // dollars for months while these objects were in euros — this endpoint exists
    // so the number on the page comes from the thing that charges the card.
    assert_eq!(p["unit_amount"], 19900);
    assert_eq!(p["currency"], "eur");
    assert_eq!(p["interval"], "year");
    assert_eq!(p["interval_count"], 1);
    assert_eq!(p["active"], true);
}

#[tokio::test]
async fn an_invoice_is_re_resolved_rather_than_served_from_our_copy() {
    let (http, cfg, _org, _mock) = setup().await;

    let inv = stripe::get_invoice(&http, &cfg, "in_1")
        .await
        .expect("invoice");
    // The PDF link Stripe hands out is short-lived, so a stored copy is a dead
    // link by the time anyone clicks it.
    assert_eq!(inv["invoice_pdf"], "https://files.stripe.test/in_1.pdf");
}

#[tokio::test]
async fn listing_invoices_is_scoped_to_one_customer_and_bounded() {
    let (http, cfg, _org, mock) = setup().await;

    let list = stripe::list_invoices(&http, &cfg, "cus_7", 20)
        .await
        .expect("invoices");
    assert_eq!(list.len(), 2);

    let q = mock.queried.lock().unwrap()[0].clone();
    assert_eq!(q.get("customer").map(String::as_str), Some("cus_7"));
    // Unbounded, this repair path would walk a large customer's entire history.
    assert_eq!(q.get("limit").map(String::as_str), Some("20"));
}

#[tokio::test]
async fn a_stripe_outage_surfaces_as_an_error_on_every_read() {
    let (http, cfg, _org, mock) = setup().await;
    mock.fail.store(503, Ordering::SeqCst);

    // Returning an empty shape instead would show a customer a subscription box
    // with no plan and no renewal date, which reads as "you have been cancelled".
    assert!(stripe::get_subscription(&http, &cfg, "sub_1")
        .await
        .is_err());
    assert!(stripe::get_price(&http, &cfg, "price_1").await.is_err());
    assert!(stripe::get_invoice(&http, &cfg, "in_1").await.is_err());
    assert!(stripe::list_invoices(&http, &cfg, "cus_1", 5)
        .await
        .is_err());
}

#[tokio::test]
async fn a_host_that_is_not_there_is_an_error_not_a_panic() {
    let (http, mut cfg, _org, _mock) = setup().await;
    // Port 1 on loopback refuses immediately — a transport failure rather than an
    // HTTP one, which takes a different branch in every one of these.
    cfg.stripe_base_url = "http://127.0.0.1:1".into();

    assert!(stripe::get_price(&http, &cfg, "price_1").await.is_err());
    assert!(
        stripe::create_checkout_session(&http, &cfg, &pack(), &Uuid::new_v4(), None)
            .await
            .is_err()
    );
}

// ---------------------------------------------------------------------------
// The parameter builders, without a network at all
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_purchase_is_priced_from_config_not_from_the_client() {
    let (_http, _cfg, org, _mock) = setup().await;
    let org_id = Uuid::new_v4();

    let params = stripe::org_purchase_params(&org, &org_id, 250, None);
    let flat: Value = json!(params
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect::<HashMap<_, _>>());

    // The unit price comes from the server's config; a client that could send it
    // could buy 250 credits for a cent.
    let raw = flat.to_string();
    assert!(
        raw.contains(&org.credit_unit_amount_cents.to_string()),
        "{raw}"
    );
    assert!(raw.contains("250"), "{raw}");
}
