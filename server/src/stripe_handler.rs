//! Stripe integration via raw `reqwest` — no SDK crate.
//!
//! Two pieces: [`create_checkout_session`] (REST call to create a hosted
//! Checkout Session) and [`verify_stripe_signature`] (manual HMAC-SHA256
//! verification of the `Stripe-Signature` webhook header). The webhook handler
//! and crediting live in `api.rs` / `billing.rs`.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use uuid::Uuid;

use crate::config::{BillingConfig, CreditPackage, OrgBillingConfig};

type HmacSha256 = Hmac<Sha256>;

/// Base URL for the Stripe API. A constant so tests could point elsewhere.
const STRIPE_API_BASE: &str = "https://api.stripe.com";

/// Create a Stripe Checkout Session for a credit package and return its hosted
/// URL. We pass `client_reference_id` + `metadata` (user id, package, credits)
/// so the webhook can credit the right account on completion.
pub async fn create_checkout_session(
    http: &reqwest::Client,
    cfg: &BillingConfig,
    pkg: &CreditPackage,
    user_id: &Uuid,
) -> Result<String, String> {
    let uid = user_id.to_string();
    let params = [
        ("mode", "payment".to_string()),
        ("success_url", cfg.stripe_success_url.clone()),
        ("cancel_url", cfg.stripe_cancel_url.clone()),
        ("client_reference_id", uid.clone()),
        ("line_items[0][price]", pkg.stripe_price_id.clone()),
        ("line_items[0][quantity]", "1".to_string()),
        ("metadata[user_id]", uid),
        ("metadata[package_id]", pkg.id.clone()),
        ("metadata[credits_usd]", format!("{:.6}", pkg.credits_usd)),
    ];

    let resp = http
        .post(format!("{STRIPE_API_BASE}/v1/checkout/sessions"))
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
        .ok_or_else(|| "checkout session had no url".to_string())
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
    ];
    post_checkout(http, cfg, &params).await
}

/// Create a one-off Checkout Session that tops up an org's credit pool by
/// `credits` (priced inline at `credit_unit_amount_cents` each).
pub async fn create_org_purchase_checkout(
    http: &reqwest::Client,
    cfg: &BillingConfig,
    org_cfg: &OrgBillingConfig,
    org_id: &Uuid,
    credits: i32,
) -> Result<String, String> {
    let oid = org_id.to_string();
    let params = [
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
        ("metadata[org_id]", oid),
        ("metadata[credits]", credits.to_string()),
    ];
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
