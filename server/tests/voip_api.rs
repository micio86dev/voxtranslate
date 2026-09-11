//! Integration tests for the VoIP HTTP surface (spec 0111, R1/R24/R28).
//!
//! What these pin, and why each one is a security property rather than a nicety:
//!
//! - The feature switch is a **route registration**, so "off" means 404 and not "a handler
//!   that has to remember to check a flag".
//! - Tenancy: a member of org A cannot read, hang up or reconfigure anything belonging to
//!   org B. Asserted per route, because tenancy leaks route by route.
//! - Role: reading settings is a member action, changing them is an admin one.
//! - The webhook endpoint is unauthenticated in the session sense and authenticated
//!   cryptographically — an unsigned body must change nothing.
//!
//! DB-gated; the database needs **pgvector**. Run:
//! `DATABASE_URL=postgres://…/voxtest cargo test --test voip_api`.

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::{Config, VoipConfig};
use voxtranslate_server::telephony::mock::{MockCommand, MockTelephonyProvider, MockWebhookBody};
use voxtranslate_server::telephony::{LegId, MediaTrack, PlayRequest, ProviderError};
use voxtranslate_server::{app, db, AppState};

const SECRET: &str = "voip-api-secret";

struct Server {
    addr: SocketAddr,
    pool: db::Pool,
    /// The same provider the server is using, so a test can sign a webhook the way the
    /// carrier would and then ask the provider what it was actually told to do.
    provider: Option<Arc<MockTelephonyProvider>>,
}

/// Stand up the API with VoIP enabled and the **mock** provider — the whole flow, no telco.
async fn setup_with_voip(voip: Option<VoipConfig>) -> Option<Server> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;

    // The global concurrency cap counts EVERY live call in the database, so rows left
    // behind by earlier runs would make unrelated tests fail with `concurrency_limit`.
    // Production has the stall reaper for this (webhook::fail_stalled_calls); a shared
    // test database needs the same tidy-up done up front.
    sqlx::query(
        "UPDATE voip_calls SET status = 'failed',
             failure_reason = COALESCE(failure_reason, 'provider_unavailable'),
             ended_at = COALESCE(ended_at, now())
         WHERE status IN ('created','dialing','ringing','answered','bridged','ending')
           AND started_at < now() - interval '1 minute'",
    )
    .execute(&pool)
    .await
    .ok();

    let mut config = Config::test_with_billing(&url, SECRET, 0.0);
    let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);
    let enabled = voip.is_some();
    config.voip = voip;

    let mut state = AppState::new(config);
    state.billing = Some(BillingService::new(pool.clone(), min_join));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);
    // `AppState::new` builds the provider from config; the test config path does not go
    // through `from_env`, so wire it here to match what a real deployment gets.
    let provider = enabled.then(|| Arc::new(MockTelephonyProvider::default()));
    if let Some(p) = provider.clone() {
        state.telephony = Some(p);
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    Some(Server {
        addr,
        pool,
        provider,
    })
}

async fn setup() -> Option<Server> {
    setup_with_voip(Some(VoipConfig::test_default())).await
}

fn base(srv: &Server) -> String {
    format!("http://{}", srv.addr)
}

async fn user(srv: &Server) -> (Uuid, String) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: "Caller".into(),
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
        "INSERT INTO organizations (name, slug, owner_id, credits_balance,
                                    subscription_status, current_period_end)
         VALUES ('VoIP Co', $1, $2, 5000, 'active', now() + interval '30 days')
         RETURNING id",
    )
    .bind(format!("voip-{}", Uuid::new_v4().simple()))
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

/// An organization whose subscription has lapsed. Everything else matches [`make_org`],
/// including a healthy credit balance — the refusal must come from the subscription, not
/// from an empty wallet, or the test would prove the wrong thing.
async fn make_org_without_subscription(srv: &Server, owner: Uuid) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id, credits_balance,
                                    subscription_status, current_period_end)
         VALUES ('Lapsed Co', $1, $2, 5000, 'canceled', now() - interval '1 day')
         RETURNING id",
    )
    .bind(format!("voip-{}", Uuid::new_v4().simple()))
    .bind(owner)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(owner)
        .bind("owner")
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

async fn make_call(srv: &Server, org: Uuid, owner: Uuid) -> Uuid {
    let session = Uuid::new_v4();
    sqlx::query("INSERT INTO call_sessions (id, room, org_id, kind) VALUES ($1, $2, $3, 'phone')")
        .bind(session)
        .bind(format!("ph-{}", Uuid::new_v4().simple()))
        .bind(org)
        .execute(&srv.pool)
        .await
        .unwrap();
    sqlx::query_scalar(
        "INSERT INTO voip_calls
            (session_id, org_id, user_id, provider, direction, recipient_e164,
             recipient_pseudonym, recipient_country, source_language, target_language,
             engine_id, status)
         VALUES ($1, $2, $3, 'mock', 'outbound', '+8613800138000', 'pseudo', 'CN', 'it',
                 'zh', 'standard', 'completed')
         RETURNING id",
    )
    .bind(session)
    .bind(org)
    .bind(owner)
    .fetch_one(&srv.pool)
    .await
    .unwrap()
}

fn client() -> Client {
    Client::new()
}

macro_rules! srv {
    () => {
        match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skipping — no DATABASE_URL");
                return;
            }
        }
    };
}

// ---- the feature switch ---------------------------------------------------

#[tokio::test]
async fn with_voip_disabled_the_routes_do_not_exist() {
    // Not "a handler that returns 503": the routes are absent. That is what makes
    // VOIP_ENABLED=false a real kill switch rather than a flag somebody can forget.
    let Some(srv) = setup_with_voip(None).await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;

    for (method, path) in [
        (
            "POST",
            format!("/api/business/organizations/{org}/voip/quote"),
        ),
        (
            "GET",
            format!("/api/business/organizations/{org}/voip/calls"),
        ),
        (
            "GET",
            format!("/api/business/organizations/{org}/voip/settings"),
        ),
        ("POST", "/api/voip/webhooks/mock".to_string()),
    ] {
        let url = format!("{}{}", base(&srv), path);
        let req = match method {
            "POST" => client().post(&url).json(&json!({})),
            _ => client().get(&url),
        };
        let res = req.bearer_auth(&jwt).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "{method} {path}");
    }
}

// ---- tenancy (R28) --------------------------------------------------------

#[tokio::test]
async fn a_non_member_cannot_touch_an_orgs_voip_at_all() {
    let srv = srv!();
    let (owner, _) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    let call = make_call(&srv, org, owner).await;
    let (_outsider, outsider_jwt) = user(&srv).await;

    let b = base(&srv);
    let cases: Vec<(&str, String)> = vec![
        (
            "GET",
            format!("{b}/api/business/organizations/{org}/voip/calls"),
        ),
        (
            "GET",
            format!("{b}/api/business/organizations/{org}/voip/calls/{call}"),
        ),
        (
            "GET",
            format!("{b}/api/business/organizations/{org}/voip/settings"),
        ),
        (
            "POST",
            format!("{b}/api/business/organizations/{org}/voip/quote"),
        ),
        (
            "POST",
            format!("{b}/api/business/organizations/{org}/voip/calls/{call}/hangup"),
        ),
    ];
    for (method, url) in cases {
        let req = match method {
            "POST" => client()
                .post(&url)
                .json(&json!({"destination": "+393201234567"})),
            _ => client().get(&url),
        };
        let res = req.bearer_auth(&outsider_jwt).send().await.unwrap();
        assert!(
            res.status() == StatusCode::FORBIDDEN || res.status() == StatusCode::NOT_FOUND,
            "{method} {url} leaked to a non-member with {}",
            res.status()
        );
    }
}

#[tokio::test]
async fn a_call_belonging_to_another_org_is_not_found_not_forbidden() {
    // 404, not 403: a 403 would confirm the call id exists, which is itself a leak across
    // the tenancy boundary.
    let srv = srv!();
    let (owner_a, jwt_a) = user(&srv).await;
    let org_a = make_org(&srv, owner_a, "owner").await;

    let (owner_b, _) = user(&srv).await;
    let org_b = make_org(&srv, owner_b, "owner").await;
    let call_b = make_call(&srv, org_b, owner_b).await;

    // A's own org, B's call id.
    let res = client()
        .get(format!(
            "{}/api/business/organizations/{org_a}/voip/calls/{call_b}",
            base(&srv)
        ))
        .bearer_auth(&jwt_a)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_plain_member_sees_only_the_calls_they_placed() {
    let srv = srv!();
    let (owner, _) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    let owners_call = make_call(&srv, org, owner).await;

    let (member, member_jwt) = user(&srv).await;
    add_member(&srv, org, member, "member").await;
    let members_call = make_call(&srv, org, member).await;

    let res: Value = client()
        .get(format!(
            "{}/api/business/organizations/{org}/voip/calls",
            base(&srv)
        ))
        .bearer_auth(&member_jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let ids: Vec<String> = res["calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_string())
        .collect();
    assert!(ids.contains(&members_call.to_string()));
    assert!(
        !ids.contains(&owners_call.to_string()),
        "a plain member must not see a colleague's calls"
    );

    // …and cannot open one by id either.
    let res = client()
        .get(format!(
            "{}/api/business/organizations/{org}/voip/calls/{owners_call}",
            base(&srv)
        ))
        .bearer_auth(&member_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_history_list_never_carries_the_full_number() {
    // A call history is the most-exported view in the product. The full number lives on
    // the detail endpoint only.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    make_call(&srv, org, owner).await;

    let body: Value = client()
        .get(format!(
            "{}/api/business/organizations/{org}/voip/calls",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let raw = body.to_string();
    assert!(
        !raw.contains("8613800138000"),
        "the list payload leaked the full number: {raw}"
    );
    assert!(raw.contains("••••"), "…and it should carry the masked form");
}

#[tokio::test]
async fn the_detail_endpoint_never_carries_our_cost_or_our_margin() {
    // Spec 0112 R6. `engine/metadata.rs` proves the same property for the engine
    // catalogue with `engine_info_never_leaks_cost_or_markup`; this is its mirror for
    // the call record. What the customer agreed to (`quoted_price_per_min`) and what they
    // paid (`credits_consumed`) are theirs. What the leg cost us, and the margin we made
    // on it, are not — and a member of any role could read both.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    let call = make_call(&srv, org, owner).await;

    // Rate the call, so the numbers are present in the row and their absence from the
    // response cannot be mistaken for "there was nothing to leak".
    sqlx::query(
        "UPDATE voip_calls
            SET actual_provider_cost_usd = 0.0340, gross_margin = 0.274,
                quoted_price_per_min = 0.0468, credits_consumed = 5
          WHERE id = $1",
    )
    .bind(call)
    .execute(&srv.pool)
    .await
    .unwrap();

    let body: Value = client()
        .get(format!(
            "{}/api/business/organizations/{org}/voip/calls/{call}",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let raw = body.to_string();
    assert!(
        !raw.contains("gross_margin"),
        "the detail payload leaked our margin: {raw}"
    );
    assert!(
        !raw.contains("actual_provider_cost"),
        "the detail payload leaked our provider cost: {raw}"
    );
    assert!(
        !raw.contains("0.274"),
        "the margin value leaked under another name: {raw}"
    );
    // What the customer is entitled to is still there.
    assert_eq!(body["credits_consumed"], serde_json::json!(5));
    assert!(raw.contains("quoted_price_per_min"));
    // A rated call reports its reconciliation as settled, without the number.
    assert_eq!(body["cost_status"], serde_json::json!("final"));
}

#[tokio::test]
async fn an_unrated_call_says_pending_rather_than_zero() {
    // Spec 0112 R6. Telnyx rates calls asynchronously (docs/voip-telnyx-setup.md §7), so
    // `actual_provider_cost_usd` is NULL for a while. Reporting that as 0 would read as
    // "this call was free", which is a very different claim from "we do not know yet".
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    let call = make_call(&srv, org, owner).await;

    let body: Value = client()
        .get(format!(
            "{}/api/business/organizations/{org}/voip/calls/{call}",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(body["cost_status"], serde_json::json!("pending"));
}

#[tokio::test]
async fn the_numbers_list_is_scoped_to_the_org_and_carries_its_verification_state() {
    // Spec 0112 R3. The dialer's caller-id select shipped with a single hardcoded
    // "default" option because nothing served the org's own numbers. This lists the
    // inventory — every row, with the state that decides whether it may be presented —
    // and leaves the "which of these are usable" filter to the client, where it is a pure
    // function with a test. `resolve_caller_id` remains the authority.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;

    let (other_owner, _) = user(&srv).await;
    let other_org = make_org(&srv, other_owner, "owner").await;

    // `voip_numbers.e164` is UNIQUE across the install and nothing sweeps this table, so
    // hardcoded fixtures pass once and then fail for ever on a shared test database.
    // Same reason `enable_dialing` uses a suffix; the country prefix still carries meaning.
    let milan = format!("+39{}", rand_suffix());
    let spain = format!("+34{}", rand_suffix());
    let fax = format!("+39{}", rand_suffix());
    let theirs = format!("+49{}", rand_suffix());

    for (e164, label, verified, outbound, target) in [
        (milan.as_str(), "Milan Office", "verified", true, org),
        (spain.as_str(), "Sales Spain", "pending", true, org),
        (fax.as_str(), "Fax", "verified", false, org),
        (theirs.as_str(), "Someone Else", "verified", true, other_org),
    ] {
        sqlx::query(
            "INSERT INTO voip_numbers (org_id, provider, e164, country, label,
                                       outbound_enabled, verification_status)
             VALUES ($1, 'mock', $2, 'IT', $3, $4, $5)",
        )
        .bind(target)
        .bind(e164)
        .bind(label)
        .bind(outbound)
        .bind(verified)
        .execute(&srv.pool)
        .await
        .unwrap();
    }

    let body: Value = client()
        .get(format!(
            "{}/api/business/organizations/{org}/voip/numbers",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let numbers = body["numbers"].as_array().unwrap();
    assert_eq!(
        numbers.len(),
        3,
        "expected this org's three numbers: {body}"
    );

    let raw = body.to_string();
    assert!(
        !raw.contains(theirs.trim_start_matches('+')),
        "another org's number crossed the tenancy boundary: {raw}"
    );

    let row = numbers
        .iter()
        .find(|n| n["e164"] == milan.as_str())
        .expect("Milan Office missing");
    assert_eq!(row["label"], "Milan Office");
    assert_eq!(row["verification_status"], "verified");
    assert_eq!(row["outbound_enabled"], serde_json::json!(true));

    // The unusable ones are present with the state that says so, rather than hidden —
    // an admin has to be able to see that "Sales Spain" is still pending.
    let row = numbers
        .iter()
        .find(|n| n["e164"] == spain.as_str())
        .expect("Sales Spain missing");
    assert_eq!(row["verification_status"], "pending");
    // And the one that is verified but not outbound-enabled is listed too; deciding which
    // may be PRESENTED is `resolve_caller_id`'s job, not this endpoint's.
    assert!(numbers.iter().any(|n| n["e164"] == fax.as_str()));
}

#[tokio::test]
async fn a_non_member_cannot_list_an_orgs_numbers() {
    // A company's phone numbers are its own. Tenancy is asserted per route because it
    // leaks per route.
    let srv = srv!();
    let (owner, _) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    let (_outsider, outsider_jwt) = user(&srv).await;

    let res = client()
        .get(format!(
            "{}/api/business/organizations/{org}/voip/numbers",
            base(&srv)
        ))
        .bearer_auth(&outsider_jwt)
        .send()
        .await
        .unwrap();

    assert!(
        res.status() == StatusCode::FORBIDDEN || res.status() == StatusCode::NOT_FOUND,
        "an outsider got {} from the numbers list",
        res.status()
    );
}

// ---- roles ----------------------------------------------------------------

#[tokio::test]
async fn reading_settings_is_a_member_action_and_changing_them_is_an_admin_one() {
    let srv = srv!();
    let (owner, _) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    let (member, member_jwt) = user(&srv).await;
    add_member(&srv, org, member, "member").await;

    let url = format!(
        "{}/api/business/organizations/{org}/voip/settings",
        base(&srv)
    );

    let res = client()
        .get(&url)
        .bearer_auth(&member_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let res = client()
        .put(&url)
        .bearer_auth(&member_jwt)
        .json(&json!({ "enabled": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn an_org_with_no_settings_row_reads_as_ready_to_dial() {
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;

    let body: Value = client()
        .get(format!(
            "{}/api/business/organizations/{org}/voip/settings",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // A customer who is already paying should not have to find a settings row before the
    // feature answers. What guards the spend is the subscription check, not this flag.
    assert_eq!(body["enabled"], json!(true));
    // Capture, however, is still off, and consent still conservative: a default may refuse
    // a call, but it must never mean "record whoever picks up".
    assert_eq!(body["recording_enabled"], json!(false));
    assert_eq!(body["transcription_enabled"], json!(false));
    assert_eq!(body["consent_policy"], json!("press_key"));
}

#[tokio::test]
async fn consent_cannot_be_disabled_while_capture_is_enabled() {
    // Refused at the boundary rather than silently corrected on every call. An admin who
    // saved this combination should be told, not overridden.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;

    let res = client()
        .put(format!(
            "{}/api/business/organizations/{org}/voip/settings",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .json(&json!({
            "enabled": true,
            "consent_policy": "disabled",
            "recording_enabled": true,
            "transcription_enabled": true,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn settings_round_trip_and_normalise_country_codes() {
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    let url = format!(
        "{}/api/business/organizations/{org}/voip/settings",
        base(&srv)
    );

    let body: Value = client()
        .put(&url)
        .bearer_auth(&jwt)
        .json(&json!({
            "enabled": true,
            "home_country": "it",
            "allowed_countries": ["de", "FR"],
            "blocked_countries": ["ru"],
            "consent_policy": "notice_only",
            "transcription_enabled": true,
            "max_call_minutes": 15,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(body["enabled"], json!(true));
    assert_eq!(body["home_country"], json!("IT"));
    assert_eq!(body["allowed_countries"], json!(["DE", "FR"]));
    assert_eq!(body["blocked_countries"], json!(["RU"]));
    assert_eq!(body["consent_policy"], json!("notice_only"));
    assert_eq!(body["max_call_minutes"], json!(15));

    // A country code that is not ISO alpha-2 is refused rather than stored and later
    // silently never matching anything.
    let res = client()
        .put(&url)
        .bearer_auth(&jwt)
        .json(&json!({ "enabled": true, "allowed_countries": ["Germany"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

// ---- dialing gate ---------------------------------------------------------

#[tokio::test]
async fn a_subscribed_org_can_quote_without_anyone_writing_a_settings_row() {
    // The whole point of the default: a paying customer's first call is not gated behind
    // an administrative act nobody told them about.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;

    let res = client()
        .post(format!(
            "{}/api/business/organizations/{org}/voip/quote",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .json(&json!({ "destination": "+393201234567" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn an_org_without_a_live_subscription_still_cannot_dial() {
    // This is now the ONLY thing standing between an arbitrary organization and real
    // Telnyx spend. It used to have `enabled = false` in front of it as a second belt;
    // that belt is gone on purpose, so this gate has to be proven rather than assumed.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org_without_subscription(&srv, owner).await;

    let res = client()
        .post(format!(
            "{}/api/business/organizations/{org}/voip/quote",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .json(&json!({ "destination": "+393201234567" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::PAYMENT_REQUIRED,
        "a lapsed subscription must refuse the call, with credits in the wallet or not"
    );
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"], json!("destination_not_allowed"));
}

#[tokio::test]
async fn a_number_that_is_not_e164_is_refused_before_anything_else() {
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;

    for (raw, code) in [
        ("not a number", "number_non_numeric"),
        ("+39320", "number_too_short"),
        ("", "number_empty"),
        ("+9991234567890", "number_unknown_country"),
    ] {
        let res = client()
            .post(format!(
                "{}/api/business/organizations/{org}/voip/quote",
                base(&srv)
            ))
            .bearer_auth(&jwt)
            .json(&json!({ "destination": raw }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{raw}");
        let body: Value = res.json().await.unwrap();
        assert_eq!(body["error"], json!(code), "{raw}");
    }
}

#[tokio::test]
async fn a_destination_with_no_rate_is_refused_rather_than_priced() {
    // R5, end to end. The destination is deliberately one no other test seeds a rate for —
    // the suite shares a database, and `enable_dialing` seeds prefix 39, so asking about
    // Italy here would pass for a reason that has nothing to do with this assertion.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;

    client()
        .put(format!(
            "{}/api/business/organizations/{org}/voip/settings",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .json(&json!({ "enabled": true, "home_country": "IT" }))
        .send()
        .await
        .unwrap();

    let res = client()
        .post(format!(
            "{}/api/business/organizations/{org}/voip/quote",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .json(&json!({ "destination": "+263771234567" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::PAYMENT_REQUIRED);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"], json!("rate_unavailable"));
}

// ---- concurrency caps (R6) ------------------------------------------------

/// Turn the feature on for an org and give it a working rate deck, so a dial can actually
/// get past the policy gate.
async fn enable_dialing(srv: &Server, org: Uuid, jwt: &str, max_per_user: i64) {
    client()
        .put(format!(
            "{}/api/business/organizations/{org}/voip/settings",
            base(srv)
        ))
        .bearer_auth(jwt)
        .json(&json!({
            "enabled": true,
            "home_country": "IT",
            "max_concurrent_per_user": max_per_user,
            "max_concurrent_per_org": max_per_user,
            "transcription_enabled": false,
        }))
        .send()
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO voip_rates (provider, prefix, description, cost_per_minute, fetched_at)
         VALUES ('mock', '39', 'Italy', 0.010, now())
         ON CONFLICT (provider, prefix) DO UPDATE SET fetched_at = now()",
    )
    .execute(&srv.pool)
    .await
    .unwrap();

    // A verified outbound number, so caller-id resolution succeeds. Without one the dial
    // is refused before the cap is ever consulted.
    sqlx::query(
        "INSERT INTO voip_numbers (org_id, provider, e164, country, outbound_enabled,
                                   is_default, verification_status)
         VALUES ($1, 'mock', $2, 'IT', TRUE, TRUE, 'verified')",
    )
    .bind(org)
    .bind(format!("+39021{:07}", rand_suffix()))
    .execute(&srv.pool)
    .await
    .unwrap();
}

fn rand_suffix() -> u32 {
    // Numbers are UNIQUE across the install; the suffix keeps parallel tests apart.
    (Uuid::new_v4().as_u128() % 9_000_000) as u32 + 1_000_000
}

#[tokio::test]
async fn concurrent_dials_cannot_exceed_the_cap() {
    // R6 says the caps are "enforced atomically". They were not: the count was read in one
    // unlocked statement and the row inserted in another, so two requests that both saw
    // one-below-the-limit both got through. Same shape as the credit race the reservation
    // module closes with FOR UPDATE, and it needed the same treatment.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    enable_dialing(&srv, org, &jwt, 1).await;

    let body = json!({
        "destination": "+393201234567",
        "source_language": "en",
        "target_language": "it",
    });
    let url = format!("{}/api/business/organizations/{org}/voip/calls", base(&srv));

    // Fired together, deliberately: the race only exists in the window between the count
    // and the insert.
    let (a, b) = tokio::join!(
        client().post(&url).bearer_auth(&jwt).json(&body).send(),
        client().post(&url).bearer_auth(&jwt).json(&body).send(),
    );
    let statuses = [a.unwrap().status(), b.unwrap().status()];

    let created = statuses
        .iter()
        .filter(|s| **s == StatusCode::CREATED)
        .count();
    assert_eq!(
        created, 1,
        "exactly one dial may pass a cap of 1, got {statuses:?}"
    );
    assert!(
        statuses.contains(&StatusCode::PAYMENT_REQUIRED),
        "the loser must be refused, got {statuses:?}"
    );

    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM voip_calls WHERE org_id = $1
         AND status IN ('created','dialing','ringing','answered','bridged','ending')",
    )
    .bind(org)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(
        live, 1,
        "the cap must hold in the database, not just in the reply"
    );
}

#[tokio::test]
async fn a_dial_that_gets_through_the_gate_holds_credits_and_appears_in_history() {
    // The happy path, end to end over HTTP against the mock provider: no telco, no
    // charges, but the real policy gate, the real ledger and the real history query.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    enable_dialing(&srv, org, &jwt, 5).await;

    let before: i32 = sqlx::query_scalar("SELECT credits_balance FROM organizations WHERE id = $1")
        .bind(org)
        .fetch_one(&srv.pool)
        .await
        .unwrap();

    let res = client()
        .post(format!(
            "{}/api/business/organizations/{org}/voip/calls",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .json(&json!({
            "destination": "+393201234567",
            "source_language": "en",
            "target_language": "it",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let created: Value = res.json().await.unwrap();
    assert_eq!(created["status"], json!("dialing"));
    let reserved = created["reserved_credits"].as_i64().unwrap();
    assert!(
        reserved > 0,
        "a hold must be taken before the provider is called"
    );

    let after: i32 = sqlx::query_scalar("SELECT credits_balance FROM organizations WHERE id = $1")
        .bind(org)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert_eq!(
        after,
        before - reserved as i32,
        "the hold is a real deduction, which is what makes concurrent dials safe"
    );

    // …and it is visible in history, masked.
    let history: Value = client()
        .get(format!(
            "{}/api/business/organizations/{org}/voip/calls",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let calls = history["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["id"], created["call_id"]);
    assert!(!history.to_string().contains("3201234567"));
}

// ---- webhook (R24) --------------------------------------------------------

#[tokio::test]
async fn an_unsigned_webhook_is_rejected() {
    let srv = srv!();
    let res = client()
        .post(format!("{}/api/voip/webhooks/mock", base(&srv)))
        .json(&json!({ "anything": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"], json!("missing_signature"));
}

#[tokio::test]
async fn a_webhook_for_another_provider_is_not_found() {
    let srv = srv!();
    let res = client()
        .post(format!("{}/api/voip/webhooks/telnyx", base(&srv)))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_badly_signed_webhook_is_rejected_and_never_five_hundreds() {
    // A 5xx would make the provider retry for hours something that will never verify.
    let srv = srv!();
    let res = client()
        .post(format!("{}/api/voip/webhooks/mock", base(&srv)))
        .header("x-signature", "deadbeef")
        .header("x-timestamp", chrono::Utc::now().timestamp().to_string())
        .json(&json!({ "event_id": "x" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_webhook_endpoint_needs_no_session() {
    // The signature is the authentication. Requiring a bearer token as well would mean
    // handing the provider a credential, which is strictly worse.
    let srv = srv!();
    let res = client()
        .post(format!("{}/api/voip/webhooks/mock", base(&srv)))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let body: Value = res.json().await.unwrap();
    // Rejected on the SIGNATURE, not by the auth middleware — the distinction is the
    // whole point: no bearer token was sent and none was wanted.
    assert_eq!(body["error"], json!("missing_signature"));
}

#[tokio::test]
async fn answering_a_call_starts_a_media_stream_on_a_ticketed_url() {
    // The gap this closes. Everything else about a call already worked without it — the
    // dial, the credit hold, the state machine, the settlement, the history row — and it
    // all worked in silence, because nothing ever asked the carrier to send us audio.
    //
    // R14/R16: on answer, and only on answer, the provider is told to open a bidirectional
    // media stream against a URL we built, carrying a single-use ticket.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    enable_dialing(&srv, org, &jwt, 5).await;

    let res = client()
        .post(format!(
            "{}/api/business/organizations/{org}/voip/calls",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .json(&json!({
            "destination": "+393201234567",
            "source_language": "it",
            "target_language": "zh",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let body: Value = res.json().await.unwrap();
    let call_id = Uuid::parse_str(body["call_id"].as_str().unwrap()).unwrap();

    let legs: Vec<String> =
        sqlx::query_scalar("SELECT unnest(provider_leg_ids) FROM voip_calls WHERE id = $1")
            .bind(call_id)
            .fetch_all(&srv.pool)
            .await
            .unwrap();
    let leg = LegId::new(legs.first().expect("the dial recorded a leg").clone());

    let provider = srv.provider.clone().expect("voip is enabled");
    assert!(
        !provider.is_streaming(&leg),
        "nothing may stream while the phone is still ringing — that would carry ringback \
         and start the provider's streaming charge early"
    );

    // Exactly the webhook a carrier sends when the far end picks up.
    let event = MockWebhookBody {
        client_state: Some(call_id.to_string()),
        ..MockWebhookBody::new(
            &format!("ev-{}", Uuid::new_v4()),
            &leg,
            "answered",
            chrono::Utc::now(),
        )
    };
    let raw = serde_json::to_vec(&event).unwrap();
    let h = provider.sign(&raw, chrono::Utc::now());

    let res = client()
        .post(format!("{}/api/voip/webhooks/mock", base(&srv)))
        .header("x-signature", h.signature.unwrap())
        .header("x-timestamp", h.timestamp.unwrap())
        .header("content-type", "application/json")
        .body(raw)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    assert!(
        provider.is_streaming(&leg),
        "the answered call has no audio path: the provider was never told to stream"
    );

    let cfg = provider
        .commands()
        .into_iter()
        .find_map(|c| match c {
            MockCommand::StartMedia(l, cfg) if l == leg => Some(*cfg),
            _ => None,
        })
        .expect("a StartMedia command for this leg");

    assert!(
        cfg.bidirectional,
        "a translated call must be able to send audio back; inbound-only is a monitor, \
         not a conversation"
    );
    assert_eq!(
        cfg.track,
        MediaTrack::Inbound,
        "streaming both tracks would send us the translated audio we just played, and the \
         engine would translate its own output"
    );

    let expected_prefix = format!("{}/voip/media/", VoipConfig::test_default().media_ws_base);
    assert!(
        cfg.url.starts_with(&expected_prefix),
        "the stream URL must be built from VOIP_MEDIA_WS_BASE and nothing else — an \
         attacker-chosen host here makes the carrier a confused deputy. Got: {}",
        cfg.url
    );

    let ticket = cfg.url.strip_prefix(&expected_prefix).unwrap();
    assert!(
        !ticket.is_empty() && ticket.contains('.'),
        "the URL must carry a signed ticket, got {ticket:?}"
    );
    assert!(
        !ticket.contains(&call_id.to_string()),
        "the ticket must not put the call id in the URL in the clear — it is signed, not \
         guessable"
    );
}

/// Dial with transcription on, so the consent gate actually applies.
///
/// `enable_dialing` deliberately turns capture off — most tests are about money and
/// tenancy and would otherwise pay for an announcement they do not assert on. These ones
/// are about the announcement.
async fn enable_dialing_with_capture(srv: &Server, org: Uuid, jwt: &str) {
    enable_dialing(srv, org, jwt, 5).await;
    sqlx::query(
        "UPDATE voip_org_settings
         SET transcription_enabled = TRUE, ai_analysis_enabled = TRUE,
             consent_policy = 'press_key', consent_refused_action = 'continue_unrecorded'
         WHERE org_id = $1",
    )
    .bind(org)
    .execute(&srv.pool)
    .await
    .unwrap();
}

/// Place a call and answer it, returning `(call_id, leg)`.
async fn dial_and_answer(
    srv: &Server,
    org: Uuid,
    jwt: &str,
    target_language: &str,
) -> (Uuid, LegId) {
    let res = client()
        .post(format!(
            "{}/api/business/organizations/{org}/voip/calls",
            base(srv)
        ))
        .bearer_auth(jwt)
        .json(&json!({
            "destination": "+393201234567",
            "source_language": "it",
            "target_language": target_language,
            "transcribe": true,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let body: Value = res.json().await.unwrap();
    let call_id = Uuid::parse_str(body["call_id"].as_str().unwrap()).unwrap();

    let legs: Vec<String> =
        sqlx::query_scalar("SELECT unnest(provider_leg_ids) FROM voip_calls WHERE id = $1")
            .bind(call_id)
            .fetch_all(&srv.pool)
            .await
            .unwrap();
    let leg = LegId::new(legs.first().expect("a leg").clone());
    post_event(srv, call_id, &leg, "answered", None).await;
    (call_id, leg)
}

/// Post one signed provider webhook, exactly as the carrier would.
async fn post_event(srv: &Server, call_id: Uuid, leg: &LegId, kind: &str, digit: Option<char>) {
    let provider = srv.provider.clone().expect("voip is enabled");
    let event = MockWebhookBody {
        client_state: Some(call_id.to_string()),
        digit,
        ..MockWebhookBody::new(
            &format!("ev-{}", Uuid::new_v4()),
            leg,
            kind,
            chrono::Utc::now(),
        )
    };
    let raw = serde_json::to_vec(&event).unwrap();
    let h = provider.sign(&raw, chrono::Utc::now());
    let res = client()
        .post(format!("{}/api/voip/webhooks/mock", base(srv)))
        .header("x-signature", h.signature.unwrap())
        .header("x-timestamp", h.timestamp.unwrap())
        .header("content-type", "application/json")
        .body(raw)
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "webhook {kind} was not accepted"
    );
}

#[tokio::test]
async fn the_recipient_is_told_before_anything_is_captured() {
    // R19/R20. The person on the telephone has no screen to consent on, so the disclosure
    // is the whole of their protection. It must be spoken in THEIR language, before a
    // single word is kept, and the gate must actually be open.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    enable_dialing_with_capture(&srv, org, &jwt).await;

    let (call_id, leg) = dial_and_answer(&srv, org, &jwt, "zh").await;
    let provider = srv.provider.clone().unwrap();

    let spoken = provider
        .commands()
        .into_iter()
        .find_map(|c| match c {
            MockCommand::Play(l, PlayRequest::Speak { text, language, .. }) if l == leg => {
                Some((text, language))
            }
            _ => None,
        })
        .expect("the disclosure must be spoken on answer");
    assert_eq!(
        spoken.1, "zh",
        "the announcement goes in the RECIPIENT's language, not the caller's — they are \
         the one being asked"
    );
    assert!(
        !spoken.0.trim().is_empty(),
        "an empty announcement is not a disclosure"
    );

    assert!(
        provider
            .commands()
            .iter()
            .any(|c| matches!(c, MockCommand::Gather(l, _) if *l == leg)),
        "press-key policy without a gather is a notice pretending to be consent"
    );

    let row = sqlx::query(
        "SELECT consent_status, disclosure_language, disclosure_played_at,
                transcription_started_at
         FROM voip_calls WHERE id = $1",
    )
    .bind(call_id)
    .fetch_one(&srv.pool)
    .await
    .unwrap();

    assert_eq!(
        row.get::<String, _>("consent_status"),
        "pending",
        "the gate is open; nobody has answered yet"
    );
    assert_eq!(
        row.get::<Option<String>, _>("disclosure_language"),
        Some("zh".into())
    );
    assert!(
        row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("disclosure_played_at")
            .is_some(),
        "the stamp is the audit evidence; without it there is no proof anyone was told"
    );
    assert!(
        row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("transcription_started_at")
            .is_none(),
        "NOTHING may be captured while consent is still pending"
    );

    // And the engine session must not exist yet either. The transcript service is handed
    // to the engine at socket-open time and cannot be attached later, so a session opened
    // while the gate is still open would be a session that can never transcribe — which
    // would turn a GRANTED consent into no transcript at all.
    assert!(
        !provider.is_streaming(&leg),
        "the audio path must not be armed while the recipient is still being asked"
    );
}

#[tokio::test]
async fn the_audio_path_opens_once_the_gate_is_answered() {
    // The other half of the ordering: a gated call is silent for a few seconds and then
    // works. If this regressed, every press-key call would stay silent forever.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    enable_dialing_with_capture(&srv, org, &jwt).await;
    let (call_id, leg) = dial_and_answer(&srv, org, &jwt, "it").await;
    let provider = srv.provider.clone().unwrap();

    assert!(!provider.is_streaming(&leg));
    post_event(&srv, call_id, &leg, "dtmf", Some('1')).await;
    assert!(
        provider.is_streaming(&leg),
        "consent was granted and the call still has no audio path"
    );
}

#[tokio::test]
async fn a_refused_gate_still_opens_the_audio_path() {
    // Refusing a transcript is not refusing the call. The two people are still on the
    // telephone and are still being charged for a translation they must actually get.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    enable_dialing_with_capture(&srv, org, &jwt).await;
    let (call_id, leg) = dial_and_answer(&srv, org, &jwt, "it").await;
    let provider = srv.provider.clone().unwrap();

    post_event(&srv, call_id, &leg, "dtmf", Some('9')).await;
    assert!(
        provider.is_streaming(&leg),
        "a denied transcript must not cost the customer the translation they paid for"
    );
}

#[tokio::test]
async fn a_transcript_is_never_kept_without_a_stamp_that_says_it_may_be() {
    // The single gate between a telephone conversation and `transcript_events`. The engine
    // persists a segment whenever it holds a transcript service and checks nothing else,
    // so this predicate is the only thing standing between an un-consenting recipient and
    // a stored record of their words.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    enable_dialing_with_capture(&srv, org, &jwt).await;
    let (call_id, leg) = dial_and_answer(&srv, org, &jwt, "zh").await;

    let mut state = AppState::new(Config::test_with_billing(
        &std::env::var("DATABASE_URL").unwrap(),
        SECRET,
        0.0,
    ));
    state.pool = Some(srv.pool.clone());

    assert!(
        !voxtranslate_server::voip::session::transcription_permitted(&state, call_id).await,
        "a call whose gate is still open must not be transcribed"
    );

    post_event(&srv, call_id, &leg, "dtmf", Some('1')).await;
    assert!(
        voxtranslate_server::voip::session::transcription_permitted(&state, call_id).await,
        "a granted call must be transcribed, or granting consent achieves nothing"
    );

    // An id nobody has ever heard of, and a deployment with no database: both fail closed.
    assert!(
        !voxtranslate_server::voip::session::transcription_permitted(&state, Uuid::new_v4()).await
    );
    state.pool = None;
    assert!(!voxtranslate_server::voip::session::transcription_permitted(&state, call_id).await);
}

#[tokio::test]
async fn a_recipient_who_could_not_be_told_is_never_captured() {
    // The compliance branch. If the announcement cannot be delivered — the carrier refuses
    // the `speak`, or cannot speak at all — the call carries on and capture does NOT start.
    // Recording someone who was never told is the incident this whole module exists to
    // prevent, so this path has to fail towards silence on our side, not theirs.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    enable_dialing_with_capture(&srv, org, &jwt).await;

    let provider = srv.provider.clone().unwrap();
    provider.fail_plays(ProviderError::Unavailable {
        detail: "carrier refused the announcement".into(),
    });

    let (call_id, leg) = dial_and_answer(&srv, org, &jwt, "it").await;

    let row = sqlx::query(
        "SELECT consent_status, transcription_status, disclosure_played_at,
                transcription_started_at
         FROM voip_calls WHERE id = $1",
    )
    .bind(call_id)
    .fetch_one(&srv.pool)
    .await
    .unwrap();

    assert!(
        row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("disclosure_played_at")
            .is_none(),
        "nothing was spoken, so nothing may be stamped as spoken"
    );
    assert_eq!(
        row.get::<String, _>("transcription_status"),
        "none",
        "capture must be switched OFF, not left wanting"
    );
    assert!(row
        .get::<Option<chrono::DateTime<chrono::Utc>>, _>("transcription_started_at")
        .is_none());
    assert_eq!(
        row.get::<String, _>("consent_status"),
        "not_required",
        "and the gate must settle: `pending` here is unreachable by the timeout sweep, \
         which only looks at rows that have a disclosure stamp"
    );

    // The two people are still on the telephone and still paying for a translation.
    assert!(
        provider.is_streaming(&leg),
        "a failed announcement must not also cost them the call"
    );
}

#[tokio::test]
async fn a_gate_nobody_answers_times_out_and_keeps_nothing() {
    // The carrier's own gather timeout fires on some routes and not others, and the task
    // that opened the gate may be gone. So the deadline is enforced from the row — and a
    // timeout is NOT consent.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    enable_dialing_with_capture(&srv, org, &jwt).await;
    let (call_id, leg) = dial_and_answer(&srv, org, &jwt, "it").await;
    let provider = srv.provider.clone().unwrap();

    assert!(!provider.is_streaming(&leg), "the gate is open");

    // Age the disclosure past the grace window rather than waiting for it.
    sqlx::query(
        "UPDATE voip_calls SET disclosure_played_at = now() - interval '5 minutes',
                               status = 'answered'
         WHERE id = $1",
    )
    .bind(call_id)
    .execute(&srv.pool)
    .await
    .unwrap();

    let mut state = AppState::new(Config::test_with_billing(
        &std::env::var("DATABASE_URL").unwrap(),
        SECRET,
        0.0,
    ));
    state.pool = Some(srv.pool.clone());
    state.telephony = Some(provider.clone());

    voxtranslate_server::voip::disclosure::time_out_pending_consent(
        &state,
        &srv.pool,
        provider.as_ref(),
        20,
        100,
    )
    .await
    .unwrap();

    let row = sqlx::query(
        "SELECT consent_status, transcription_status, transcription_started_at, status
         FROM voip_calls WHERE id = $1",
    )
    .bind(call_id)
    .fetch_one(&srv.pool)
    .await
    .unwrap();

    assert_eq!(
        row.get::<String, _>("consent_status"),
        "timeout",
        "silence is recorded as silence, never as agreement"
    );
    assert_eq!(row.get::<String, _>("transcription_status"), "none");
    assert!(row
        .get::<Option<chrono::DateTime<chrono::Utc>>, _>("transcription_started_at")
        .is_none());
    assert_eq!(
        row.get::<String, _>("status"),
        "answered",
        "the org's policy here is continue_unrecorded: the call goes on"
    );
}

#[tokio::test]
async fn two_digits_arriving_together_cannot_both_decide() {
    // Sent concurrently, not in sequence: the compare-and-swap in `on_dtmf` exists for the
    // window between reading `pending` and writing the answer, and a sequential test never
    // opens that window. Same shape as `concurrent_dials_cannot_exceed_the_cap`.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    enable_dialing_with_capture(&srv, org, &jwt).await;
    let (call_id, leg) = dial_and_answer(&srv, org, &jwt, "it").await;

    tokio::join!(
        post_event(&srv, call_id, &leg, "dtmf", Some('1')),
        post_event(&srv, call_id, &leg, "dtmf", Some('4')),
    );

    let (status, received): (String, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT consent_status, consent_received_at FROM voip_calls WHERE id = $1")
            .bind(call_id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();

    // Either digit may win — that is a race between two keypresses and not ours to
    // arbitrate. What must not happen is both winning, or neither.
    assert!(
        status == "granted" || status == "denied",
        "the gate must settle on exactly one answer, got {status}"
    );
    assert!(received.is_some(), "and it must record when it settled");
}

#[tokio::test]
async fn an_analysis_the_caller_asked_for_is_queued_once_and_only_once() {
    // R23. The tick at dial time is the consent to charge, so the analysis must actually
    // happen — and must happen exactly once, because every enqueue spends credits and the
    // sweep runs every minute for ever.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    enable_dialing_with_capture(&srv, org, &jwt).await;

    let res = client()
        .post(format!(
            "{}/api/business/organizations/{org}/voip/calls",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .json(&json!({
            "destination": "+393201234567",
            "source_language": "it",
            "target_language": "en",
            "transcribe": true,
            "ai_analysis": true,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let body: Value = res.json().await.unwrap();
    let call_id = Uuid::parse_str(body["call_id"].as_str().unwrap()).unwrap();
    let session_id = Uuid::parse_str(body["session_id"].as_str().unwrap()).unwrap();

    let requested: bool =
        sqlx::query_scalar("SELECT ai_analysis_requested FROM voip_calls WHERE id = $1")
            .bind(call_id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert!(
        requested,
        "the request was collected from the dialer and then dropped — nothing could act \
         on it once the call ended, which is the only moment it can happen"
    );

    let mut state = AppState::new(Config::test_with_billing(
        &std::env::var("DATABASE_URL").unwrap(),
        SECRET,
        0.0,
    ));
    state.pool = Some(srv.pool.clone());
    // The sweep reads the finished transcript, so it needs the service that owns it.
    state.transcripts = Some(voxtranslate_server::transcripts::TranscriptService::new(
        srv.pool.clone(),
    ));

    // Not finished, and no transcript: nothing may be queued yet.
    voxtranslate_server::voip::webhook::enqueue_ai_analysis(&state, 50)
        .await
        .unwrap();
    let stamp: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT ai_analysis_enqueued_at FROM voip_calls WHERE id = $1")
            .bind(call_id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert!(
        stamp.is_none(),
        "a call that is still running has no finished transcript to summarise"
    );

    // Now end it, close the session, and give it something to say.
    sqlx::query("UPDATE voip_calls SET status = 'completed', ended_at = now() WHERE id = $1")
        .bind(call_id)
        .execute(&srv.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE call_sessions SET ended_at = now() WHERE id = $1")
        .bind(session_id)
        .execute(&srv.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO transcript_events (session_id, event_type, speaker_peer_id, speaker_name,
                                        original_text, original_lang, translations, ts)
         VALUES ($1, 'speech', 'p1', 'Caller', 'ciao', 'it', '{}'::jsonb, now())",
    )
    .bind(session_id)
    .execute(&srv.pool)
    .await
    .unwrap();

    voxtranslate_server::voip::webhook::enqueue_ai_analysis(&state, 50)
        .await
        .unwrap();

    let stamp: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT ai_analysis_enqueued_at FROM voip_calls WHERE id = $1")
            .bind(call_id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    let first = stamp.expect("a finished call with a transcript must be queued");

    // The sweep runs every minute for ever. A second pass must find nothing to do.
    voxtranslate_server::voip::webhook::enqueue_ai_analysis(&state, 50)
        .await
        .unwrap();
    let stamp: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT ai_analysis_enqueued_at FROM voip_calls WHERE id = $1")
            .bind(call_id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(
        stamp,
        Some(first),
        "the call was reconsidered — every extra pass is another charge to the customer"
    );
}

#[tokio::test]
async fn a_call_nobody_asked_to_analyse_is_left_alone() {
    // The default. Charging for a report the caller did not tick is the failure this
    // column exists to make impossible.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    enable_dialing_with_capture(&srv, org, &jwt).await;
    let (call_id, _leg) = dial_and_answer(&srv, org, &jwt, "en").await;

    let requested: bool =
        sqlx::query_scalar("SELECT ai_analysis_requested FROM voip_calls WHERE id = $1")
            .bind(call_id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert!(!requested);
}

/// A server with video switched on. The default is off, so most tests never see it.
async fn setup_with_video() -> Option<Server> {
    let mut voip = VoipConfig::test_default();
    voip.video_enabled = true;
    setup_with_voip(Some(voip)).await
}

#[tokio::test]
async fn a_video_invite_never_carries_the_room_in_the_clear() {
    // D9. The recipient is on a telephone, so the upgrade is a link into the room the call
    // is already happening in. The link must be a signed, short-lived ticket rather than
    // the room code, because an invitation gets forwarded and a room code has no expiry.
    let Some(srv) = setup_with_video().await else {
        eprintln!("skipping: no DATABASE_URL");
        return;
    };
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    enable_dialing(&srv, org, &jwt, 5).await;

    let res = client()
        .post(format!(
            "{}/api/business/organizations/{org}/voip/calls",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .json(&json!({
            "destination": "+393201234567",
            "source_language": "it",
            "target_language": "en",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let body: Value = res.json().await.unwrap();
    let call_id = body["call_id"].as_str().unwrap().to_string();
    let room = body["room"].as_str().unwrap().to_string();

    let res = client()
        .post(format!(
            "{}/api/business/organizations/{org}/voip/calls/{call_id}/video-invite",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let invite: Value = res.json().await.unwrap();
    let url = invite["url"].as_str().expect("an invitation url");

    assert!(
        !url.contains(&room),
        "the room leaked into a link meant to be forwarded: {url}"
    );
    assert!(url.contains("/api/voip/video/"));
    assert!(
        invite["expires_at"].is_string(),
        "an invitation with no stated expiry is one nobody can reason about"
    );

    // Redeeming it sends the recipient to the room — and only then. Redirects are not
    // followed here: the point of the assertion is WHERE it sends them, and a client that
    // follows the hop reports only that the app answered.
    let ticket = url.rsplit('/').next().unwrap();
    let res = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .get(format!("{}/api/voip/video/{ticket}", base(&srv)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let location = res
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        location.ends_with(&format!("/?room={room}")),
        "expected the ordinary room deep link, got {location}"
    );

    // Audited: someone was invited into a conversation.
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_logs
         WHERE org_id = $1 AND action = 'voip.video_invite'",
    )
    .bind(org)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert!(audited >= 1);
}

#[tokio::test]
async fn a_forged_or_finished_invite_yields_nothing() {
    let Some(srv) = setup_with_video().await else {
        eprintln!("skipping: no DATABASE_URL");
        return;
    };
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    enable_dialing(&srv, org, &jwt, 5).await;

    // Garbage: refused, and with a 404 rather than anything that confirms a call exists.
    for ticket in ["nonsense", "a.b", "..."] {
        let res = client()
            .get(format!("{}/api/voip/video/{ticket}", base(&srv)))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "accepted {ticket:?}");
    }

    // A genuine invitation for a call that has since ended.
    let res = client()
        .post(format!(
            "{}/api/business/organizations/{org}/voip/calls",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .json(&json!({
            "destination": "+393201234567",
            "source_language": "it",
            "target_language": "en",
        }))
        .send()
        .await
        .unwrap();
    let body: Value = res.json().await.unwrap();
    let call_id = body["call_id"].as_str().unwrap().to_string();

    let invite: Value = client()
        .post(format!(
            "{}/api/business/organizations/{org}/voip/calls/{call_id}/video-invite",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ticket = invite["url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_string();

    sqlx::query("UPDATE voip_calls SET status = 'completed', ended_at = now() WHERE id = $1")
        .bind(Uuid::parse_str(&call_id).unwrap())
        .execute(&srv.pool)
        .await
        .unwrap();

    // The signature is still perfectly good. What has changed is that there is nothing
    // worth joining — an invitation into a finished call puts someone alone in a room.
    let res = client()
        .get(format!("{}/api/voip/video/{ticket}", base(&srv)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn video_stays_off_unless_it_is_switched_on() {
    // `VOIP_VIDEO_ENABLED` defaults to false, and off must mean 404 rather than a refusal
    // that confirms which calls exist.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    enable_dialing(&srv, org, &jwt, 5).await;
    let (call_id, _leg) = dial_and_answer(&srv, org, &jwt, "en").await;

    let res = client()
        .post(format!(
            "{}/api/business/organizations/{org}/voip/calls/{call_id}/video-invite",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn another_orgs_call_cannot_be_invited_into() {
    // Tenancy, asserted on this route like every other: it leaks route by route.
    let Some(srv) = setup_with_video().await else {
        eprintln!("skipping: no DATABASE_URL");
        return;
    };
    let (owner_a, jwt_a) = user(&srv).await;
    let org_a = make_org(&srv, owner_a, "owner").await;
    enable_dialing(&srv, org_a, &jwt_a, 5).await;
    let body: Value = client()
        .post(format!(
            "{}/api/business/organizations/{org_a}/voip/calls",
            base(&srv)
        ))
        .bearer_auth(&jwt_a)
        .json(&json!({
            "destination": "+393201234567",
            "source_language": "it",
            "target_language": "en",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let call_id = body["call_id"].as_str().unwrap().to_string();

    let (owner_b, jwt_b) = user(&srv).await;
    let org_b = make_org(&srv, owner_b, "owner").await;

    let res = client()
        .post(format!(
            "{}/api/business/organizations/{org_b}/voip/calls/{call_id}/video-invite",
            base(&srv)
        ))
        .bearer_auth(&jwt_b)
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::NOT_FOUND,
        "a call from another org must be invisible, not merely forbidden"
    );
}

#[tokio::test]
async fn a_call_that_never_answers_gives_its_room_and_leg_back() {
    // The provider accepts the dial and then says nothing — the exact case the stall
    // reaper exists for. Marking the row failed is only half of it: the phone peer holds
    // the room's channel open, so a leg left parked costs a leg, a peer AND a room for the
    // life of the process.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    enable_dialing(&srv, org, &jwt, 5).await;

    let res = client()
        .post(format!(
            "{}/api/business/organizations/{org}/voip/calls",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .json(&json!({
            "destination": "+393201234567",
            "source_language": "it",
            "target_language": "en",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let body: Value = res.json().await.unwrap();
    let call_id = Uuid::parse_str(body["call_id"].as_str().unwrap()).unwrap();
    let room = body["room"]
        .as_str()
        .expect("the dial must name the room to join");

    // Without this the caller's browser has nowhere to go, and the engine translates the
    // telephone into a room with no other languages in it — silence, both directions.
    assert!(room.starts_with("ph-"), "unexpected room name {room}");

    // Age the row past the stall window, then run the reaper the way the sweep does.
    sqlx::query("UPDATE voip_calls SET started_at = now() - interval '20 minutes' WHERE id = $1")
        .bind(call_id)
        .execute(&srv.pool)
        .await
        .unwrap();

    // Run it until this call is reached. The reaper is deployment-wide and takes a bounded
    // batch, so on a shared test database another test's stalled rows can fill one pass —
    // which is the reaper working as designed, not a failure.
    for _ in 0..5 {
        let failed = voxtranslate_server::voip::webhook::fail_stalled_calls(&srv.pool, 100)
            .await
            .unwrap();
        if failed.contains(&call_id) {
            break;
        }
        if failed.is_empty() {
            break;
        }
    }

    let status: String = sqlx::query_scalar("SELECT status FROM voip_calls WHERE id = $1")
        .bind(call_id)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert_eq!(status, "failed");
}

#[tokio::test]
async fn pressing_one_grants_and_only_then_does_capture_start() {
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    enable_dialing_with_capture(&srv, org, &jwt).await;
    let (call_id, leg) = dial_and_answer(&srv, org, &jwt, "it").await;

    post_event(&srv, call_id, &leg, "dtmf", Some('1')).await;

    let row = sqlx::query(
        "SELECT consent_status, consent_received_at, disclosure_played_at,
                transcription_started_at
         FROM voip_calls WHERE id = $1",
    )
    .bind(call_id)
    .fetch_one(&srv.pool)
    .await
    .unwrap();

    assert_eq!(row.get::<String, _>("consent_status"), "granted");
    assert!(row
        .get::<Option<chrono::DateTime<chrono::Utc>>, _>("consent_received_at")
        .is_some());

    let played: chrono::DateTime<chrono::Utc> = row.get("disclosure_played_at");
    let started: chrono::DateTime<chrono::Utc> = row
        .get::<Option<_>, _>("transcription_started_at")
        .expect("capture must start once consent is granted");
    assert!(
        started >= played,
        "capture started BEFORE the disclosure was played — this is the one ordering that \
         cannot be wrong, because it is what an auditor reads off the row"
    );
}

#[tokio::test]
async fn any_other_key_denies_and_the_call_continues_unrecorded() {
    // A wrong key is not an ambiguous signal to be resolved in our favour. And denial ends
    // the CAPTURE, not the call — the two people were talking, and cutting them off
    // because they did not want a transcript would be its own kind of rude.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    enable_dialing_with_capture(&srv, org, &jwt).await;
    let (call_id, leg) = dial_and_answer(&srv, org, &jwt, "it").await;

    post_event(&srv, call_id, &leg, "dtmf", Some('7')).await;

    let row = sqlx::query(
        "SELECT consent_status, transcription_status, transcription_started_at, status
         FROM voip_calls WHERE id = $1",
    )
    .bind(call_id)
    .fetch_one(&srv.pool)
    .await
    .unwrap();

    assert_eq!(row.get::<String, _>("consent_status"), "denied");
    assert_eq!(
        row.get::<String, _>("transcription_status"),
        "none",
        "a denied gate must leave nothing marked as wanted, or the sweep reconsiders it"
    );
    assert!(row
        .get::<Option<chrono::DateTime<chrono::Utc>>, _>("transcription_started_at")
        .is_none());
    assert_eq!(
        row.get::<String, _>("status"),
        "answered",
        "the CALL continues; only the recorder stopped"
    );

    let provider = srv.provider.clone().unwrap();
    assert!(
        !provider
            .commands()
            .iter()
            .any(|c| matches!(c, MockCommand::Hangup(l) if *l == leg)),
        "continue_unrecorded must not hang up on the recipient"
    );
}

#[tokio::test]
async fn a_second_digit_cannot_overturn_a_decision() {
    // Webhooks are redelivered, and a keypad keeps working after the gate closes. Neither
    // may turn a denial into a grant.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    enable_dialing_with_capture(&srv, org, &jwt).await;
    let (call_id, leg) = dial_and_answer(&srv, org, &jwt, "it").await;

    post_event(&srv, call_id, &leg, "dtmf", Some('7')).await;
    post_event(&srv, call_id, &leg, "dtmf", Some('1')).await;

    let status: String = sqlx::query_scalar("SELECT consent_status FROM voip_calls WHERE id = $1")
        .bind(call_id)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert_eq!(
        status, "denied",
        "the first answer stands; a later key must not grant what was refused"
    );
}

// ---- the 0111 findings, closed ---------------------------------------------

#[tokio::test]
async fn a_partial_settings_write_cannot_switch_capture_on() {
    // An admin who sends `{"enabled": true}` has said nothing about transcription. The
    // canonical default is written into `OrgSettings::default_for_new_org` in the team's
    // own words: recording, transcription and AI analysis stay OFF, because "a default
    // that captures someone nobody asked is a different kind of mistake from a default
    // that refuses a call".
    //
    // Three places disagreed about it. The read path returned `false` (and
    // `an_org_with_no_settings_row_reads_as_ready_to_dial` asserts it), while
    // `#[serde(default = "yes")]` and the column default both said `TRUE`. The passing
    // test made the property look held when the write path contradicted it.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;

    let res = client()
        .put(format!(
            "{}/api/business/organizations/{org}/voip/settings",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .json(&json!({ "enabled": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.unwrap();

    assert_eq!(body["transcription_enabled"], json!(false), "{body}");
    assert_eq!(body["recording_enabled"], json!(false), "{body}");
    assert_eq!(body["ai_analysis_enabled"], json!(false), "{body}");

    // The column default must not contradict the API either. A row created by any other
    // path — a support script, a backfill — must not arrive with capture switched on.
    let (other, _) = user(&srv).await;
    let other_org = make_org(&srv, other, "owner").await;
    sqlx::query("INSERT INTO voip_org_settings (org_id) VALUES ($1)")
        .bind(other_org)
        .execute(&srv.pool)
        .await
        .unwrap();
    let stored: bool =
        sqlx::query_scalar("SELECT transcription_enabled FROM voip_org_settings WHERE org_id = $1")
            .bind(other_org)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert!(!stored, "the column default still switches capture on");
}

#[tokio::test]
async fn a_caller_id_the_org_cannot_prove_it_owns_is_refused() {
    // The highest-stakes branch in this file, and it had no test. `resolve_caller_id`
    // says why in the code: presenting a number you cannot prove you own is illegal in
    // most of our markets. Tenancy is asserted per route because it leaks per route —
    // this is the same argument with a regulator behind it.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    enable_dialing(&srv, org, &jwt, 5).await;

    let (other_owner, other_jwt) = user(&srv).await;
    let other_org = make_org(&srv, other_owner, "owner").await;
    enable_dialing(&srv, other_org, &other_jwt, 5).await;

    // A number that belongs to the OTHER organisation, verified and outbound-enabled
    // there. Owning it somewhere is not owning it here.
    let theirs: String =
        sqlx::query_scalar("SELECT e164 FROM voip_numbers WHERE org_id = $1 LIMIT 1")
            .bind(other_org)
            .fetch_one(&srv.pool)
            .await
            .unwrap();

    // One of ours, but still awaiting verification.
    let pending = format!("+39022{:07}", rand_suffix());
    sqlx::query(
        "INSERT INTO voip_numbers (org_id, provider, e164, country, outbound_enabled,
                                   verification_status)
         VALUES ($1, 'mock', $2, 'IT', TRUE, 'pending')",
    )
    .bind(org)
    .bind(&pending)
    .execute(&srv.pool)
    .await
    .unwrap();

    for (caller_id, why) in [
        (theirs.as_str(), "another org's verified number"),
        (pending.as_str(), "our own number, still pending"),
    ] {
        let res = client()
            .post(format!(
                "{}/api/business/organizations/{org}/voip/calls",
                base(&srv)
            ))
            .bearer_auth(&jwt)
            .json(&json!({
                "destination": "+390212345678",
                "source_language": "it",
                "target_language": "en",
                "caller_id": caller_id,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::FORBIDDEN,
            "presenting {why} should be refused"
        );
        let body: Value = res.json().await.unwrap();
        assert_eq!(body["error"], json!("caller_id_unverified"), "{body}");
    }
}

#[tokio::test]
async fn a_quote_refuses_what_a_dial_would_refuse() {
    // `quote`'s doc comment promises "the same gate as dial, so the dialer cannot show a
    // price for a call that would then be refused". Two of dial's checks were missing
    // here, so the exact failure the comment rules out was reachable on two paths: an org
    // with `require_project`, and an org with no verified number to present.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    enable_dialing(&srv, org, &jwt, 5).await;

    let quote = |body: Value| {
        let jwt = jwt.clone();
        let url = format!("{}/api/business/organizations/{org}/voip/quote", base(&srv));
        async move {
            client()
                .post(url)
                .bearer_auth(&jwt)
                .json(&body)
                .send()
                .await
                .unwrap()
        }
    };

    // Baseline: with a project not required and a verified number seeded, it prices.
    let res = quote(json!({ "destination": "+390212345678" })).await;
    assert_eq!(res.status(), StatusCode::OK, "baseline quote should price");

    // (1) The org now requires a project, and none was named.
    sqlx::query("UPDATE voip_org_settings SET require_project = TRUE WHERE org_id = $1")
        .bind(org)
        .execute(&srv.pool)
        .await
        .unwrap();
    let res = quote(json!({ "destination": "+390212345678" })).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"], json!("project_required"), "{body}");

    sqlx::query("UPDATE voip_org_settings SET require_project = FALSE WHERE org_id = $1")
        .bind(org)
        .execute(&srv.pool)
        .await
        .unwrap();

    // (2) A caller id the org cannot prove it owns — refused at quote time, not after the
    // customer has read a price and pressed Call.
    let res = quote(json!({
        "destination": "+390212345678",
        "caller_id": "+390299999999",
    }))
    .await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"], json!("caller_id_unverified"), "{body}");
}

#[tokio::test]
async fn a_policy_refusal_carries_a_code_the_client_can_translate() {
    // `err_response` already states the rule: only the stable machine-readable code
    // crosses the boundary, because a refusal's prose would be untranslatable. These
    // paths shipped raw English as a `text/plain` body instead — which the dashboard
    // cannot even parse, so every one of them surfaced as the generic message.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;

    let res = client()
        .put(format!(
            "{}/api/business/organizations/{org}/voip/settings",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .json(&json!({ "enabled": true, "allowed_countries": ["ITALY"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"], json!("invalid_country_code"), "{body}");

    let res = client()
        .put(format!(
            "{}/api/business/organizations/{org}/voip/settings",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .json(&json!({
            "enabled": true,
            "consent_policy": "disabled",
            "transcription_enabled": true,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let body: Value = res.json().await.unwrap();
    assert_eq!(
        body["error"],
        json!("consent_required_for_capture"),
        "{body}"
    );
}

// ---- contacts (spec 0114) --------------------------------------------------

/// Create a contact through the API and return its id.
async fn make_contact(srv: &Server, org: Uuid, jwt: &str, body: Value) -> (StatusCode, Value) {
    let res = client()
        .post(format!(
            "{}/api/business/organizations/{org}/voip/contacts",
            base(srv)
        ))
        .bearer_auth(jwt)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = res.status();
    (status, res.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn a_contact_carries_its_numbers_and_each_number_its_own_language() {
    // The language lives on the NUMBER. A colleague who takes work calls in English on the
    // office line and Catalan on their mobile is one person, not two.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;

    let office = format!("+3493{:07}", rand_suffix());
    let mobile = format!("+3462{:07}", rand_suffix());
    let (status, body) = make_contact(
        &srv,
        org,
        &jwt,
        json!({
            "name": "Marta Roig",
            "company": "Roig Import",
            "tags": ["supplier"],
            "numbers": [
                { "e164": office, "label": "Office", "language": "en", "is_primary": true },
                { "e164": mobile, "label": "Mobile", "language": "ca" },
            ],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let id = body["id"].as_str().expect("id").to_string();
    let detail: Value = client()
        .get(format!(
            "{}/api/business/organizations/{org}/voip/contacts/{id}",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let numbers = detail["numbers"].as_array().unwrap();
    assert_eq!(numbers.len(), 2, "{detail}");
    let by_label = |l: &str| {
        numbers
            .iter()
            .find(|n| n["label"] == l)
            .unwrap_or_else(|| panic!("{l} missing: {detail}"))
    };
    assert_eq!(by_label("Office")["language"], json!("en"));
    assert_eq!(by_label("Mobile")["language"], json!("ca"));
    assert_eq!(by_label("Office")["is_primary"], json!(true));
    assert_eq!(by_label("Mobile")["is_primary"], json!(false));
}

#[tokio::test]
async fn one_number_belongs_to_one_person_per_organisation() {
    // Inbound must never have to CHOOSE whose call this is (spec 0116), so the constraint
    // lives in the database rather than in a handler that could forget.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    let shared = format!("+3902{:07}", rand_suffix());

    let (first, _) = make_contact(
        &srv,
        org,
        &jwt,
        json!({ "name": "First", "numbers": [{ "e164": shared }] }),
    )
    .await;
    assert_eq!(first, StatusCode::CREATED);

    let (second, body) = make_contact(
        &srv,
        org,
        &jwt,
        json!({ "name": "Second", "numbers": [{ "e164": shared }] }),
    )
    .await;
    assert_eq!(second, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], json!("number_already_known"), "{body}");

    // Another organisation may legitimately know the same supplier.
    let (other_owner, other_jwt) = user(&srv).await;
    let other_org = make_org(&srv, other_owner, "owner").await;
    let (elsewhere, body) = make_contact(
        &srv,
        other_org,
        &other_jwt,
        json!({ "name": "Same supplier", "numbers": [{ "e164": shared }] }),
    )
    .await;
    assert_eq!(elsewhere, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn a_contact_reaches_many_projects_and_appears_once_in_each() {
    // The first many-to-many involving `projects` in this schema.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;

    let mut projects = Vec::new();
    for name in ["Alpha", "Beta"] {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO projects (org_id, name, created_by) VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(org)
        .bind(name)
        .bind(owner)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
        projects.push(id);
    }

    let (status, body) = make_contact(
        &srv,
        org,
        &jwt,
        json!({
            "name": "Shared",
            "numbers": [{ "e164": format!("+3902{:07}", rand_suffix()) }],
            // Linked twice on purpose: linking is idempotent (R3).
            "project_ids": [projects[0], projects[1], projects[0]],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_string();

    for project in &projects {
        let list: Value = client()
            .get(format!(
                "{}/api/business/organizations/{org}/voip/contacts?project_id={project}",
                base(&srv)
            ))
            .bearer_auth(&jwt)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let rows = list["contacts"].as_array().unwrap();
        assert_eq!(
            rows.len(),
            1,
            "one row per project, not one per link: {list}"
        );
        assert_eq!(rows[0]["id"], json!(id));
    }

    // Deleting a project leaves the person, with one link fewer.
    sqlx::query("DELETE FROM projects WHERE id = $1")
        .bind(projects[0])
        .execute(&srv.pool)
        .await
        .unwrap();
    let detail: Value = client()
        .get(format!(
            "{}/api/business/organizations/{org}/voip/contacts/{id}",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail["projects"].as_array().unwrap().len(), 1, "{detail}");
}

#[tokio::test]
async fn contacts_are_searchable_by_the_things_people_remember() {
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    let number = format!("+8613{:07}", rand_suffix());

    make_contact(
        &srv,
        org,
        &jwt,
        json!({
            "name": "Wei Zhang",
            "company": "Shenzhen Optics",
            "tags": ["supplier", "hardware"],
            "numbers": [{ "e164": number, "language": "zh" }],
        }),
    )
    .await;
    make_contact(
        &srv,
        org,
        &jwt,
        json!({ "name": "Someone Else", "numbers": [{ "e164": format!("+3902{:07}", rand_suffix()) }] }),
    )
    .await;

    let find = |query: String| {
        let jwt = jwt.clone();
        let url = format!(
            "{}/api/business/organizations/{org}/voip/contacts?{query}",
            base(&srv)
        );
        async move {
            let body: Value = client()
                .get(url)
                .bearer_auth(&jwt)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            body["contacts"].as_array().unwrap().len()
        }
    };

    assert_eq!(
        find("q=zhang".into()).await,
        1,
        "by name, case-insensitively"
    );
    assert_eq!(find("q=shenzhen".into()).await, 1, "by company");
    assert_eq!(
        find(format!("q={}", &number[4..10])).await,
        1,
        "by part of a number"
    );
    assert_eq!(find("tag=supplier".into()).await, 1, "by tag");
    assert_eq!(
        find("language=zh".into()).await,
        1,
        "by the language they speak"
    );
    assert_eq!(
        find("q=nobody".into()).await,
        0,
        "and nothing when nothing matches"
    );
}

#[tokio::test]
async fn a_number_can_be_looked_up_by_whoever_is_calling() {
    // The reverse index, made addressable. Spec 0116 answers "who is ringing?" with it,
    // and the dashboard uses it to decide whether to offer to save a number.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    let known = format!("+3902{:07}", rand_suffix());
    make_contact(
        &srv,
        org,
        &jwt,
        json!({ "name": "Known", "numbers": [{ "e164": known, "language": "it" }] }),
    )
    .await;

    let lookup = |e164: String| {
        let jwt = jwt.clone();
        let url = format!(
            "{}/api/business/organizations/{org}/voip/contacts/lookup?e164={e164}",
            base(&srv)
        );
        async move { client().get(url).bearer_auth(&jwt).send().await.unwrap() }
    };

    let res = lookup(known.clone()).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["name"], json!("Known"));
    assert_eq!(
        body["language"],
        json!("it"),
        "the language of THAT number: {body}"
    );

    let res = lookup(format!("+3902{:07}", rand_suffix())).await;
    assert_eq!(
        res.status(),
        StatusCode::NOT_FOUND,
        "an unknown number is not an error"
    );
}

#[tokio::test]
async fn deleting_a_contact_leaves_the_calls_that_were_made_to_them() {
    // A call that happened cannot un-happen, and the financial record must outlive the
    // convenience data that described it — the rule the credits ledger already follows.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    let (_, body) = make_contact(
        &srv,
        org,
        &jwt,
        json!({ "name": "Gone", "numbers": [{ "e164": format!("+3902{:07}", rand_suffix()) }] }),
    )
    .await;
    let contact = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();

    let call = make_call(&srv, org, owner).await;
    sqlx::query("UPDATE voip_calls SET contact_id = $1 WHERE id = $2")
        .bind(contact)
        .bind(call)
        .execute(&srv.pool)
        .await
        .unwrap();

    let res = client()
        .delete(format!(
            "{}/api/business/organizations/{org}/voip/contacts/{contact}",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);

    let still_there: Option<Option<Uuid>> =
        sqlx::query_scalar("SELECT contact_id FROM voip_calls WHERE id = $1")
            .bind(call)
            .fetch_optional(&srv.pool)
            .await
            .unwrap();
    let contact_id = still_there.expect("the call was deleted along with the contact");
    assert!(
        contact_id.is_none(),
        "the call still names a contact that is gone"
    );
}

#[tokio::test]
async fn a_non_member_cannot_read_or_write_an_orgs_address_book() {
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    make_contact(
        &srv,
        org,
        &jwt,
        json!({ "name": "Private", "numbers": [{ "e164": format!("+3902{:07}", rand_suffix()) }] }),
    )
    .await;

    let (_outsider, outsider_jwt) = user(&srv).await;
    for path in [
        "/voip/contacts",
        "/voip/contacts/lookup?e164=%2B390212345678",
    ] {
        let res = client()
            .get(format!(
                "{}/api/business/organizations/{org}{path}",
                base(&srv)
            ))
            .bearer_auth(&outsider_jwt)
            .send()
            .await
            .unwrap();
        assert!(
            res.status() == StatusCode::FORBIDDEN || res.status() == StatusCode::NOT_FOUND,
            "an outsider got {} from {path}",
            res.status()
        );
    }
}

#[tokio::test]
async fn dialling_a_known_number_files_the_call_against_the_person() {
    // Resolved from the destination rather than asked of the caller: the number is what
    // was dialled, and who it belongs to is a fact about it.
    let srv = srv!();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner, "owner").await;
    enable_dialing(&srv, org, &jwt, 5).await;

    let number = format!("+3902{:07}", rand_suffix());
    let (_, contact) = make_contact(
        &srv,
        org,
        &jwt,
        json!({ "name": "Known Supplier", "numbers": [{ "e164": number, "language": "it" }] }),
    )
    .await;
    let contact_id = contact["id"].as_str().unwrap().to_string();

    let res = client()
        .post(format!(
            "{}/api/business/organizations/{org}/voip/calls",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .json(&json!({
            "destination": number,
            "source_language": "en",
            "target_language": "it",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let created: Value = res.json().await.unwrap();
    let call_id = created["call_id"].as_str().unwrap();

    let detail: Value = client()
        .get(format!(
            "{}/api/business/organizations/{org}/voip/calls/{call_id}",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail["contact_id"], json!(contact_id), "{detail}");
    assert_eq!(detail["contact_name"], json!("Known Supplier"), "{detail}");
}
