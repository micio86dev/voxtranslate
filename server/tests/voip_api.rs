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
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::{Config, VoipConfig};
use voxtranslate_server::telephony::mock::MockTelephonyProvider;
use voxtranslate_server::{app, db, AppState};

const SECRET: &str = "voip-api-secret";

struct Server {
    addr: SocketAddr,
    pool: db::Pool,
}

/// Stand up the API with VoIP enabled and the **mock** provider — the whole flow, no telco.
async fn setup_with_voip(voip: Option<VoipConfig>) -> Option<Server> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;

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
    if enabled {
        state.telephony = Some(Arc::new(MockTelephonyProvider::default()));
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    Some(Server { addr, pool })
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
async fn an_org_with_no_settings_row_reads_as_disabled() {
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
    assert_eq!(body["enabled"], json!(false));
    // …and with the conservative consent policy, so a misconfiguration cannot mean
    // "capture silently".
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
async fn an_org_that_has_not_enabled_voip_cannot_dial() {
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
    assert_eq!(res.status(), StatusCode::PAYMENT_REQUIRED);
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
    // R5, end to end: the rate deck is empty in a fresh database, so every destination is
    // unpriced and every call stops. Failing this way is loud, which is the point.
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
        .json(&json!({ "destination": "+393201234567" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::PAYMENT_REQUIRED);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"], json!("rate_unavailable"));
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
