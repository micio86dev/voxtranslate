//! Stripe integration via raw `reqwest` — no SDK crate.
//!
//! Two pieces: [`create_checkout_session`] (REST call to create a hosted
//! Checkout Session) and [`verify_stripe_signature`] (manual HMAC-SHA256
//! verification of the `Stripe-Signature` webhook header). The webhook handler
//! and crediting live in `api.rs` / `billing.rs`.

use chrono::DateTime;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use uuid::Uuid;

use crate::config::{BillingConfig, CreditPackage, OrgBillingConfig};

type HmacSha256 = Hmac<Sha256>;

/// Base URL for the Stripe API. A constant so tests could point elsewhere.
const STRIPE_API_BASE: &str = "https://api.stripe.com";

/// Params that turn a one-off Checkout Session into an **invoiced** one (spec 0109).
///
/// `mode=payment` produces only a payment *receipt* unless `invoice_creation` is
/// switched on — that omission is why consumer credit purchases had no fiscal
/// document at all. The rest is the identity the invoice must carry: a billing
/// address (mandatory on an EU invoice) and an optional VAT / tax id for business
/// buyers.
///
/// `existing` is the buyer's already-known Stripe customer, when we have one.
/// Stripe rejects `customer` together with `customer_creation`, and — when
/// `tax_id_collection` is enabled and a `customer` is supplied — it *requires*
/// `customer_update[address]`, so the two branches genuinely differ.
fn invoicing_params(existing: Option<&str>, description: &str) -> Vec<(&'static str, String)> {
    let mut p = vec![
        ("invoice_creation[enabled]", "true".to_string()),
        (
            "invoice_creation[invoice_data][description]",
            description.to_string(),
        ),
        ("billing_address_collection", "required".to_string()),
        ("tax_id_collection[enabled]", "true".to_string()),
    ];
    match existing {
        Some(customer) => p.extend([
            ("customer", customer.to_string()),
            // Required by Stripe alongside tax_id_collection, and it keeps the
            // customer's saved address/name in step with what they just typed.
            ("customer_update[address]", "auto".to_string()),
            ("customer_update[name]", "auto".to_string()),
        ]),
        None => p.push(("customer_creation", "always".to_string())),
    }
    p
}

/// Form params for a consumer credit-package Checkout Session. Pure, so the
/// invoicing flags can be pinned by a unit test without a live Stripe call.
pub fn consumer_checkout_params(
    cfg: &BillingConfig,
    pkg: &CreditPackage,
    user_id: &Uuid,
    existing_customer: Option<&str>,
) -> Vec<(&'static str, String)> {
    let uid = user_id.to_string();
    let mut params = vec![
        ("mode", "payment".to_string()),
        ("success_url", cfg.stripe_success_url.clone()),
        ("cancel_url", cfg.stripe_cancel_url.clone()),
        ("client_reference_id", uid.clone()),
        ("line_items[0][price]", pkg.stripe_price_id.clone()),
        ("line_items[0][quantity]", "1".to_string()),
        ("metadata[user_id]", uid.clone()),
        ("metadata[package_id]", pkg.id.clone()),
        ("metadata[credits_usd]", format!("{:.6}", pkg.credits_usd)),
    ];
    params.extend(invoicing_params(existing_customer, "VoxTranslate credits"));
    // Mirrored onto the invoice so a webhook that only sees the invoice can still
    // resolve the buyer.
    params.push(("invoice_creation[invoice_data][metadata][user_id]", uid));
    params
}

/// Create a Stripe Checkout Session for a credit package and return its hosted
/// URL. We pass `client_reference_id` + `metadata` (user id, package, credits)
/// so the webhook can credit the right account on completion, and turn on
/// invoice creation so the purchase yields a real, numbered invoice PDF.
pub async fn create_checkout_session(
    http: &reqwest::Client,
    cfg: &BillingConfig,
    pkg: &CreditPackage,
    user_id: &Uuid,
    existing_customer: Option<&str>,
) -> Result<String, String> {
    let params = consumer_checkout_params(cfg, pkg, user_id, existing_customer);
    post_checkout(http, cfg, &params).await
}

/// Create a Stripe Checkout Session in **subscription** mode for an org (spec
/// 0106): auto-renewing, the customer manages it via the Billing Portal. The
/// `org_id`/`plan`/`interval` ride along as metadata so the webhook can link the
/// resulting customer + subscription to the org.
pub async fn create_org_subscription_checkout(
    http: &reqwest::Client,
    cfg: &BillingConfig,
    org_cfg: &OrgBillingConfig,
    org_id: &Uuid,
    price_id: &str,
    plan: &str,
    interval: &str,
) -> Result<String, String> {
    let oid = org_id.to_string();
    let params = [
        ("mode", "subscription".to_string()),
        ("success_url", org_cfg.success_url.clone()),
        ("cancel_url", org_cfg.cancel_url.clone()),
        ("client_reference_id", oid.clone()),
        ("line_items[0][price]", price_id.to_string()),
        ("line_items[0][quantity]", "1".to_string()),
        ("metadata[org_id]", oid.clone()),
        ("metadata[plan]", plan.to_string()),
        ("metadata[interval]", interval.to_string()),
        ("subscription_data[metadata][org_id]", oid),
        // Subscriptions already invoice themselves — `invoice_creation` is a
        // payment-mode-only flag and Stripe rejects it here. What they still need
        // is the buyer's legal identity on those invoices.
        ("billing_address_collection", "required".to_string()),
        ("tax_id_collection[enabled]", "true".to_string()),
    ];
    post_checkout(http, cfg, &params).await
}

/// Form params for an org credit top-up. Pure, for the same reason as
/// [`consumer_checkout_params`].
pub fn org_purchase_params(
    org_cfg: &OrgBillingConfig,
    org_id: &Uuid,
    credits: i32,
    existing_customer: Option<&str>,
) -> Vec<(&'static str, String)> {
    let oid = org_id.to_string();
    let mut params = vec![
        ("mode", "payment".to_string()),
        ("success_url", org_cfg.success_url.clone()),
        ("cancel_url", org_cfg.cancel_url.clone()),
        ("client_reference_id", oid.clone()),
        ("line_items[0][price_data][currency]", "usd".to_string()),
        (
            "line_items[0][price_data][product_data][name]",
            "VoxTranslate organization credits".to_string(),
        ),
        (
            "line_items[0][price_data][unit_amount]",
            org_cfg.credit_unit_amount_cents.to_string(),
        ),
        ("line_items[0][quantity]", credits.to_string()),
        ("metadata[org_id]", oid.clone()),
        ("metadata[credits]", credits.to_string()),
    ];
    params.extend(invoicing_params(
        existing_customer,
        "VoxTranslate organization credits",
    ));
    params.push(("invoice_creation[invoice_data][metadata][org_id]", oid));
    params
}

/// Create a one-off Checkout Session that tops up an org's credit pool by
/// `credits` (priced inline at `credit_unit_amount_cents` each). Invoiced, so the
/// top-up produces a document the org can book — same as a subscription renewal.
pub async fn create_org_purchase_checkout(
    http: &reqwest::Client,
    cfg: &BillingConfig,
    org_cfg: &OrgBillingConfig,
    org_id: &Uuid,
    credits: i32,
    existing_customer: Option<&str>,
) -> Result<String, String> {
    let params = org_purchase_params(org_cfg, org_id, credits, existing_customer);
    post_checkout(http, cfg, &params).await
}

/// Create a Stripe Billing **Customer Portal** session so an org admin can cancel
/// auto-renew, switch plan, or update their card — Stripe-hosted, no custom UI.
pub async fn create_portal_session(
    http: &reqwest::Client,
    cfg: &BillingConfig,
    org_cfg: &OrgBillingConfig,
    customer_id: &str,
) -> Result<String, String> {
    let params = [
        ("customer", customer_id.to_string()),
        ("return_url", org_cfg.portal_return_url.clone()),
    ];
    let resp = http
        .post(format!("{STRIPE_API_BASE}/v1/billing_portal/sessions"))
        .bearer_auth(&cfg.stripe_secret_key)
        .form(&params)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("stripe returned {}", resp.status()));
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    body["url"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "portal session had no url".to_string())
}

/// Retrieve a Stripe Subscription and return the fields the dashboard's
/// subscription box surfaces: raw status, period dates, cancel flags, amount +
/// interval, and the default card. Unix timestamps are converted to RFC3339
/// strings so they overlay cleanly on our DB view; the default payment method is
/// expanded so we can show the card brand + last4. Absent values come back null.
pub async fn get_subscription(
    http: &reqwest::Client,
    cfg: &BillingConfig,
    subscription_id: &str,
) -> Result<serde_json::Value, String> {
    let resp = http
        .get(format!(
            "{STRIPE_API_BASE}/v1/subscriptions/{subscription_id}"
        ))
        .query(&[("expand[]", "default_payment_method")])
        .bearer_auth(&cfg.stripe_secret_key)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("stripe returned {}", resp.status()));
    }
    let s: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;

    // First non-empty unix-seconds value among the given JSON pointers → RFC3339.
    // (Newer Stripe API versions moved the period to the subscription item, so we
    // try the item paths as a fallback to the top-level fields.)
    let iso = |paths: &[&str]| -> Option<String> {
        paths.iter().find_map(|p| {
            s.pointer(p)
                .and_then(|v| v.as_i64())
                .filter(|n| *n > 0)
                .and_then(|n| DateTime::from_timestamp(n, 0))
                .map(|d| d.to_rfc3339())
        })
    };
    let price = s.pointer("/items/data/0/price");
    let card = s.pointer("/default_payment_method/card");

    Ok(serde_json::json!({
        "stripe_status": s["status"].as_str(),
        "start_date": iso(&["/start_date"]),
        "current_period_start": iso(&["/current_period_start", "/items/data/0/current_period_start"]),
        "current_period_end": iso(&["/current_period_end", "/items/data/0/current_period_end"]),
        "cancel_at_period_end": s["cancel_at_period_end"].as_bool(),
        "cancel_at": iso(&["/cancel_at"]),
        "canceled_at": iso(&["/canceled_at"]),
        "amount": price.and_then(|p| p.get("unit_amount")).and_then(|v| v.as_i64()),
        "currency": price.and_then(|p| p.get("currency")).and_then(|v| v.as_str()),
        "interval": price.and_then(|p| p.pointer("/recurring/interval")).and_then(|v| v.as_str()),
        "card_brand": card.and_then(|c| c.get("brand")).and_then(|v| v.as_str()),
        "card_last4": card.and_then(|c| c.get("last4")).and_then(|v| v.as_str()),
        "card_exp_month": card.and_then(|c| c.get("exp_month")).and_then(|v| v.as_i64()),
        "card_exp_year": card.and_then(|c| c.get("exp_year")).and_then(|v| v.as_i64()),
    }))
}

/// Retrieve a single Stripe Invoice. Used at download time: the `invoice_pdf`
/// URL Stripe hands out is short-lived, so we never serve the stored copy — we
/// re-resolve it and redirect to whatever is current.
pub async fn get_invoice(
    http: &reqwest::Client,
    cfg: &BillingConfig,
    invoice_id: &str,
) -> Result<serde_json::Value, String> {
    let resp = http
        .get(format!("{STRIPE_API_BASE}/v1/invoices/{invoice_id}"))
        .bearer_auth(&cfg.stripe_secret_key)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("stripe returned {}", resp.status()));
    }
    resp.json().await.map_err(|e| e.to_string())
}

/// List a customer's invoices, newest first (Stripe's default order). Used to
/// backfill an owner whose invoices predate the local index — the webhook is the
/// normal path, this is the repair path.
pub async fn list_invoices(
    http: &reqwest::Client,
    cfg: &BillingConfig,
    customer_id: &str,
    limit: u8,
) -> Result<Vec<serde_json::Value>, String> {
    let resp = http
        .get(format!("{STRIPE_API_BASE}/v1/invoices"))
        .query(&[("customer", customer_id), ("limit", &limit.to_string())])
        .bearer_auth(&cfg.stripe_secret_key)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("stripe returned {}", resp.status()));
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(body["data"].as_array().cloned().unwrap_or_default())
}

/// Shared POST to Checkout Sessions returning the hosted `url`.
async fn post_checkout(
    http: &reqwest::Client,
    cfg: &BillingConfig,
    params: &[(&str, String)],
) -> Result<String, String> {
    let resp = http
        .post(format!("{STRIPE_API_BASE}/v1/checkout/sessions"))
        .bearer_auth(&cfg.stripe_secret_key)
        .form(params)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("stripe returned {}", resp.status()));
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    body["url"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "checkout session had no url".to_string())
}

/// Stripe's signed-timestamp tolerance: reject events whose `t` is more than this far
/// from now, neutralising webhook replay beyond the window (issue #117). Matches
/// Stripe's own default tolerance.
const STRIPE_TIMESTAMP_TOLERANCE_SECS: i64 = 300;

/// Verify a Stripe webhook signature with the live clock. The `Stripe-Signature` header
/// looks like `t=<ts>,v1=<hex>,v1=<hex>...`; the signed payload is `"{ts}.{body}"` HMAC'd
/// with the webhook secret. Returns `true` only if a `v1` matches **and** the timestamp
/// is within tolerance.
pub fn verify_stripe_signature(secret: &str, payload: &[u8], sig_header: &str) -> bool {
    verify_stripe_signature_at(
        secret,
        payload,
        sig_header,
        crate::now_unix() as i64,
        STRIPE_TIMESTAMP_TOLERANCE_SECS,
    )
}

/// Signature + freshness check with an injected clock + tolerance, so the timestamp
/// window is unit-testable without a real wall clock.
pub fn verify_stripe_signature_at(
    secret: &str,
    payload: &[u8],
    sig_header: &str,
    now: i64,
    tolerance_secs: i64,
) -> bool {
    if secret.is_empty() {
        return false;
    }
    let mut timestamp: Option<&str> = None;
    let mut signatures: Vec<&str> = Vec::new();
    for part in sig_header.split(',') {
        let mut kv = part.splitn(2, '=');
        match (kv.next(), kv.next()) {
            (Some("t"), Some(t)) => timestamp = Some(t.trim()),
            (Some("v1"), Some(s)) => signatures.push(s.trim()),
            _ => {}
        }
    }
    let (Some(ts), false) = (timestamp, signatures.is_empty()) else {
        return false;
    };

    // Reject an unparseable, stale, or future-dated timestamp before the HMAC check.
    let Ok(ts_num) = ts.parse::<i64>() else {
        return false;
    };
    if (now - ts_num).abs() > tolerance_secs {
        return false;
    }

    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(ts.as_bytes());
    mac.update(b".");
    mac.update(payload);
    let expected = hex::encode(mac.finalize().into_bytes());

    signatures
        .iter()
        .any(|s| constant_time_eq(s.as_bytes(), expected.as_bytes()))
}

/// Length-checked, branch-free byte comparison (avoids signature timing leaks).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Build a valid `Stripe-Signature` header for `payload` (used by tests and any
/// local signing). Production webhooks are signed by Stripe, not by us.
pub fn sign_payload(secret: &str, timestamp: i64, payload: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(payload);
    let sig = hex::encode(mac.finalize().into_bytes());
    format!("t={timestamp},v1={sig}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Look a param up by key in a builder's output.
    fn get<'a>(params: &'a [(&str, String)], key: &str) -> Option<&'a str> {
        params
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    }

    fn pkg() -> CreditPackage {
        CreditPackage {
            id: "starter".into(),
            name: "Starter".into(),
            price_usd: 5.0,
            credits_usd: 5.0,
            stripe_price_id: "price_starter".into(),
        }
    }

    fn billing_cfg() -> BillingConfig {
        crate::config::Config::test_with_billing("postgres://unused", "secret", 0.0)
            .billing
            .expect("test_with_billing always sets billing")
    }

    fn org_cfg() -> OrgBillingConfig {
        OrgBillingConfig {
            webhook_secret: "whsec_test".into(),
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

    // The single param whose absence WAS the bug: without `invoice_creation`,
    // `mode=payment` yields a receipt and no invoice ever exists. No integration
    // test catches this without live Stripe, so it is pinned here.
    #[test]
    fn consumer_checkout_asks_stripe_for_an_invoice() {
        let cfg = billing_cfg();
        let uid = Uuid::new_v4();
        let p = consumer_checkout_params(&cfg, &pkg(), &uid, None);

        assert_eq!(get(&p, "invoice_creation[enabled]"), Some("true"));
        assert_eq!(get(&p, "billing_address_collection"), Some("required"));
        assert_eq!(get(&p, "tax_id_collection[enabled]"), Some("true"));
        // No known customer yet → Stripe must make one, or the invoice has no owner.
        assert_eq!(get(&p, "customer_creation"), Some("always"));
        assert_eq!(get(&p, "customer"), None);
        // Crediting metadata is untouched by the invoicing change.
        assert_eq!(get(&p, "mode"), Some("payment"));
        assert_eq!(get(&p, "line_items[0][price]"), Some("price_starter"));
        assert_eq!(get(&p, "metadata[user_id]"), Some(uid.to_string().as_str()));
        assert_eq!(get(&p, "metadata[package_id]"), Some("starter"));
        assert_eq!(get(&p, "metadata[credits_usd]"), Some("5.000000"));
    }

    // Stripe rejects `customer` + `customer_creation` together, and demands
    // `customer_update[address]` once `tax_id_collection` is on with a customer.
    #[test]
    fn consumer_checkout_reuses_a_known_customer() {
        let cfg = billing_cfg();
        let p = consumer_checkout_params(&cfg, &pkg(), &Uuid::new_v4(), Some("cus_123"));

        assert_eq!(get(&p, "customer"), Some("cus_123"));
        assert_eq!(get(&p, "customer_creation"), None);
        assert_eq!(get(&p, "customer_update[address]"), Some("auto"));
        assert_eq!(get(&p, "customer_update[name]"), Some("auto"));
    }

    // Org top-ups are `mode=payment` too — they had exactly the same gap.
    #[test]
    fn org_purchase_is_invoiced_and_keeps_credit_metadata() {
        let oid = Uuid::new_v4();
        let p = org_purchase_params(&org_cfg(), &oid, 250, Some("cus_org"));

        assert_eq!(get(&p, "invoice_creation[enabled]"), Some("true"));
        assert_eq!(get(&p, "customer"), Some("cus_org"));
        assert_eq!(get(&p, "customer_creation"), None);
        assert_eq!(get(&p, "metadata[credits]"), Some("250"));
        assert_eq!(get(&p, "metadata[org_id]"), Some(oid.to_string().as_str()));
        assert_eq!(get(&p, "line_items[0][quantity]"), Some("250"));
    }

    // Verify against the signed timestamp (so freshness passes) — the signature logic
    // is what these cases exercise; the freshness window has its own test below.
    fn verify_fresh(secret: &str, payload: &[u8], header: &str, ts: i64) -> bool {
        verify_stripe_signature_at(secret, payload, header, ts, STRIPE_TIMESTAMP_TOLERANCE_SECS)
    }

    #[test]
    fn signature_round_trip_and_rejections() {
        let secret = "whsec_test";
        let payload = br#"{"id":"evt_1","type":"checkout.session.completed"}"#;
        let ts = 1_700_000_000;
        let header = sign_payload(secret, ts, payload);

        assert!(verify_fresh(secret, payload, &header, ts));
        // Tampered payload is rejected.
        assert!(!verify_fresh(secret, b"{}", &header, ts));
        // Wrong secret is rejected.
        assert!(!verify_fresh("whsec_other", payload, &header, ts));
        // Malformed / empty headers are rejected.
        assert!(!verify_fresh(secret, payload, "garbage", ts));
        assert!(!verify_fresh(secret, payload, "t=1", ts));
        assert!(!verify_fresh("", payload, &header, ts));
    }

    #[test]
    fn signature_accepts_among_multiple_v1() {
        let secret = "whsec_test";
        let payload = b"hello";
        let ts = 123;
        let valid = sign_payload(secret, ts, payload);
        // Splice an extra (bogus) v1 in — a real v1 must still match.
        let with_extra = format!("{valid},v1=deadbeef");
        assert!(verify_fresh(secret, payload, &with_extra, ts));
    }

    #[test]
    fn rejects_stale_or_future_timestamp() {
        let secret = "whsec_test";
        let payload = b"replay";
        let ts = 1_700_000_000;
        let header = sign_payload(secret, ts, payload);
        // Within tolerance (either side) → accepted.
        assert!(verify_stripe_signature_at(
            secret,
            payload,
            &header,
            ts + 299,
            300
        ));
        assert!(verify_stripe_signature_at(
            secret,
            payload,
            &header,
            ts - 299,
            300
        ));
        // A valid signature replayed outside the window → rejected.
        assert!(!verify_stripe_signature_at(
            secret,
            payload,
            &header,
            ts + 600,
            300
        ));
        assert!(!verify_stripe_signature_at(
            secret,
            payload,
            &header,
            ts - 600,
            300
        ));
    }
}
