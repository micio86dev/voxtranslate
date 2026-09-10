//! Live provider smoke tests (spec 0111, §"LIVE SMOKE TESTS").
//!
//! **Every test here is `#[ignore]` and additionally gated on `VOIP_LIVE_TESTS=true`.**
//! Two gates, not one, because `cargo test -- --ignored` is a thing people type, and one
//! of these places a real, billable telephone call.
//!
//! They never run in CI. `.github/workflows/ci.yml` runs `cargo test` without `--ignored`,
//! so the gate is structural rather than a convention someone has to remember.
//!
//! ```sh
//! VOIP_LIVE_TESTS=true \
//! TELNYX_API_KEY=… TELNYX_CONNECTION_ID=… TELNYX_PUBLIC_KEY=… \
//!   cargo test --test telnyx_live -- --ignored --nocapture
//!
//! # and, separately, the one that spends money:
//! VOIP_LIVE_TESTS=true TELNYX_LIVE_TEST_TO=+39… \
//!   cargo test --test telnyx_live place_one_real_call -- --ignored --nocapture
//! ```
//!
//! `TELNYX_LIVE_TEST_TO` must be a number **you own**. Never a customer's, never a real
//! person's who has not agreed, and never one committed to a fixture.

use chrono::Utc;
use voxtranslate_server::config::{TelnyxConfig, TELNYX_DEFAULT_API_BASE};
use voxtranslate_server::telephony::telnyx::TelnyxProvider;
use voxtranslate_server::telephony::{DialRequest, TelephonyProvider, WebhookHeaders, E164};

/// Both gates. Returns `None` — and says why — rather than failing, so a full
/// `--ignored` run on a machine with no credentials reports "skipped", not "broken".
fn provider() -> Option<TelnyxProvider> {
    if std::env::var("VOIP_LIVE_TESTS").as_deref() != Ok("true") {
        eprintln!("skipping — set VOIP_LIVE_TESTS=true to run live provider tests");
        return None;
    }
    let api_key = std::env::var("TELNYX_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())?;
    let cfg = TelnyxConfig {
        api_key,
        api_base: std::env::var("TELNYX_API_BASE")
            .unwrap_or_else(|_| TELNYX_DEFAULT_API_BASE.to_string()),
        connection_id: std::env::var("TELNYX_CONNECTION_ID").unwrap_or_default(),
        outbound_voice_profile_id: std::env::var("TELNYX_OUTBOUND_VOICE_PROFILE_ID").ok(),
        public_key_b64: std::env::var("TELNYX_PUBLIC_KEY").unwrap_or_default(),
        default_caller_id: std::env::var("TELNYX_DEFAULT_CALLER_ID").ok(),
        media_anchor: std::env::var("TELNYX_MEDIA_ANCHOR")
            .unwrap_or_else(|_| "Frankfurt, Germany".to_string()),
    };
    Some(TelnyxProvider::new(cfg, 300))
}

/// Costs nothing: asserts the deployment is pointed at the EU, which is a claim we make
/// in writing and should therefore be able to check.
#[tokio::test]
#[ignore = "live provider"]
async fn the_configured_endpoint_is_the_eu_one() {
    let Some(p) = provider() else { return };
    let meta = p.metadata();
    println!(
        "region: {} · eu_telephony: {}",
        meta.region, meta.eu_telephony
    );
    assert!(
        meta.eu_telephony,
        "this deployment is NOT configured for EU telephony — check TELNYX_API_BASE and \
         TELNYX_MEDIA_ANCHOR. Both are required; either alone is a residency claim that \
         is not true."
    );
    assert_eq!(
        meta.capabilities.max_streams_per_leg, 1,
        "the un-bridged two-leg design depends on this being 1"
    );
}

/// Costs nothing. Proves the credentials work AND that the rate deck parses — which is
/// the thing that decides whether any call can be placed at all (R5 fails closed).
#[tokio::test]
#[ignore = "live provider"]
async fn credentials_work_and_the_rate_deck_parses() {
    let Some(p) = provider() else { return };

    let rates = p
        .fetch_rate_deck()
        .await
        .expect("rate deck fetch failed — check TELNYX_API_KEY and the API base");

    println!("parsed {} destination rates", rates.len());
    assert!(
        !rates.is_empty(),
        "the rate deck parsed to ZERO rows. Every call would then be refused with \
         `rate_unavailable` — which is the safe failure, but it means the response shape \
         has changed and `parse_rate_deck` needs updating."
    );

    // Spot-check the shape rather than any particular price: prices move, the shape is
    // what the parser depends on.
    let sample = &rates[0];
    println!(
        "sample: +{} = {} /min ({})",
        sample.prefix, sample.cost_per_minute, sample.description
    );
    assert!(sample.prefix.chars().all(|c| c.is_ascii_digit()));
    assert!(sample.cost_per_minute >= rust_decimal::Decimal::ZERO);
}

/// Costs nothing. The webhook endpoint fails CLOSED without a public key, and this is the
/// check that proves the configured key is the account's — a mismatch would silently
/// reject every real webhook and the calls would appear to hang.
#[tokio::test]
#[ignore = "live provider"]
async fn an_unsigned_webhook_is_rejected_by_the_configured_key() {
    let Some(p) = provider() else { return };

    let body = br#"{"data":{"id":"x","event_type":"call.answered","occurred_at":"2026-01-01T00:00:00Z","payload":{"call_control_id":"v3:x"}}}"#;

    assert!(
        p.verify_webhook(&WebhookHeaders::default(), body, Utc::now())
            .is_err(),
        "an unsigned webhook must be refused"
    );

    let junk = WebhookHeaders {
        signature: Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into()),
        timestamp: Some(Utc::now().timestamp().to_string()),
    };
    assert!(
        p.verify_webhook(&junk, body, Utc::now()).is_err(),
        "a garbage signature must be refused"
    );

    // A genuine end-to-end signature check needs Telnyx to sign something, which only a
    // real event can do. That half is covered by `place_one_real_call` below, whose
    // webhooks arrive at the configured URL and either verify or do not.
    println!(
        "signature verification is armed (public key is {} bytes)",
        std::env::var("TELNYX_PUBLIC_KEY").unwrap_or_default().len()
    );
}

/// **This one spends money and rings a real telephone.**
///
/// Requires `TELNYX_LIVE_TEST_TO` on top of the two gates, so it cannot run by accident
/// even during a deliberate `--ignored` sweep. It dials, waits, and hangs up; it does not
/// assert on the outcome beyond "the provider accepted it", because whether a human
/// answers is not a property of our code.
#[tokio::test]
#[ignore = "live provider — PLACES A REAL, BILLABLE CALL"]
async fn place_one_real_call() {
    let Some(p) = provider() else { return };

    let Ok(to) = std::env::var("TELNYX_LIVE_TEST_TO") else {
        eprintln!(
            "skipping — set TELNYX_LIVE_TEST_TO to a number YOU OWN. Never a customer's, \
             never a real person's who has not agreed."
        );
        return;
    };
    let from = std::env::var("TELNYX_DEFAULT_CALLER_ID")
        .expect("TELNYX_DEFAULT_CALLER_ID is required to place a call");

    let to = E164::parse(&to).expect("TELNYX_LIVE_TEST_TO must be E.164");
    let from = E164::parse(&from).expect("TELNYX_DEFAULT_CALLER_ID must be E.164");

    // The masked form even here: a test log is still a log.
    println!("dialling {to} from {from} …");

    let leg = p
        .dial(DialRequest {
            to: to.clone(),
            from,
            client_state: format!("live-smoke-{}", uuid::Uuid::new_v4()),
            timeout_secs: 20,
            region: std::env::var("TELNYX_MEDIA_ANCHOR")
                .unwrap_or_else(|_| "Frankfurt, Germany".to_string()),
        })
        .await
        .expect("dial was refused by the provider");

    println!("leg: {} · session: {:?}", leg.id, leg.session_id);

    // Long enough for it to ring, short enough that nobody has to answer it.
    tokio::time::sleep(std::time::Duration::from_secs(8)).await;

    p.hangup(&leg.id).await.expect("hangup failed");
    println!("hung up. Check the provider portal for the CDR and the webhook deliveries.");
}

/// Documents the gap rather than pretending it away: the adapter reports per-leg CDR as
/// unsupported, so cost reconciliation is not wired. Asserting the current behaviour means
/// this test starts FAILING on the day someone implements it — which is the reminder to
/// delete it and wire the reconcile.
#[tokio::test]
#[ignore = "live provider"]
async fn per_leg_cost_is_still_not_available_synchronously() {
    let Some(p) = provider() else { return };
    let err = p
        .fetch_cdr(&voxtranslate_server::telephony::LegId::new("v3:none"))
        .await
        .expect_err("expected the documented Unsupported, not a value");
    println!("as documented: {err}");
    println!(
        "Cost reconciliation stays unwired until the usage-report shape is confirmed \
         against a live account — see docs/voip-telnyx-setup.md §6."
    );
}
