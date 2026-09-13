//! The Chrome extension's PKCE handoff and its session-upgrade gates.
//!
//! `POST /api/extension/code` is called by the web app for a signed-in user and
//! returns a code only the holder of the matching verifier can redeem;
//! `POST /api/extension/token` redeems it. Together they are what stops a
//! credential ever travelling in a URL — so what is asserted here is mostly the
//! refusals.
//!
//! DB-gated: skipped without `DATABASE_URL`.

use std::net::SocketAddr;
use std::sync::Arc;

use base64::Engine as _;
use reqwest::Client;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::Config;
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::{app, db, AppState};

const SECRET: &str = "extension-auth-secret";

/// An RFC 7636 verifier: 43–128 chars of unreserved ASCII.
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";

struct Server {
    addr: SocketAddr,
    pool: db::Pool,
}

async fn setup() -> Option<Server> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let config = Config::test_with_billing(&url, SECRET, 0.0);
    let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);
    let mut state = AppState::new(config);
    state.billing = Some(BillingService::new(pool.clone(), min_join));
    state.safety = Some(SafetyService::new(pool.clone()));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
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
        name: "Ext User".into(),
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

/// base64url(SHA-256(verifier)) — what the challenge has to equal.
fn challenge_for(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

async fn issue_code(http: &Client, srv: &Server, jwt: &str, challenge: &str) -> reqwest::Response {
    http.post(format!("{}/api/extension/code", base(srv)))
        .bearer_auth(jwt)
        .json(&json!({ "code_challenge": challenge }))
        .send()
        .await
        .unwrap()
}

async fn exchange(http: &Client, srv: &Server, code: &str, verifier: &str) -> reqwest::Response {
    http.post(format!("{}/api/extension/token", base(srv)))
        .json(&json!({ "code": code, "code_verifier": verifier }))
        .send()
        .await
        .unwrap()
}

/// Issue a code for a fresh user and return `(user_id, code)`.
async fn code_for_new_user(http: &Client, srv: &Server) -> (Uuid, String) {
    let (user_id, jwt) = user(srv).await;
    let r = issue_code(http, srv, &jwt, &challenge_for(VERIFIER)).await;
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    (user_id, body["code"].as_str().unwrap().to_string())
}

macro_rules! skip_without_db {
    () => {
        match setup().await {
            Some(srv) => srv,
            None => {
                eprintln!("skipping — no DATABASE_URL");
                return;
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Issuing a code
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_signed_in_user_gets_a_short_lived_code() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (_, jwt) = user(&srv).await;

    let r = issue_code(&http, &srv, &jwt, &challenge_for(VERIFIER)).await;

    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(!body["code"].as_str().unwrap().is_empty());
    let ttl = body["expires_in"].as_i64().unwrap();
    assert!(
        (0..=300).contains(&ttl),
        "a bearer credential in a URL handoff must expire in minutes, got {ttl}s"
    );
}

#[tokio::test]
async fn issuing_a_code_needs_a_signed_in_user() {
    let srv = skip_without_db!();
    let http = Client::new();

    let r = http
        .post(format!("{}/api/extension/code", base(&srv)))
        .json(&json!({ "code_challenge": challenge_for(VERIFIER) }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn plain_pkce_is_refused_because_it_defeats_the_purpose() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (_, jwt) = user(&srv).await;

    let r = http
        .post(format!("{}/api/extension/code", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({
            "code_challenge": challenge_for(VERIFIER),
            "code_challenge_method": "plain",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn the_method_defaults_to_s256_when_omitted() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (_, jwt) = user(&srv).await;

    assert_eq!(
        issue_code(&http, &srv, &jwt, &challenge_for(VERIFIER))
            .await
            .status(),
        200
    );
}

#[tokio::test]
async fn a_challenge_that_is_not_a_sha256_digest_is_refused() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (_, jwt) = user(&srv).await;

    // Exactly 43 base64url chars is the only shape a SHA-256 digest can take.
    // Bounding it stops a hostile caller stuffing the signed token with data.
    for challenge in ["", "too-short", &"x".repeat(44), &"x".repeat(42)] {
        assert_eq!(
            issue_code(&http, &srv, &jwt, challenge).await.status(),
            400,
            "challenge {challenge:?} should be refused"
        );
    }
}

#[tokio::test]
async fn a_challenge_with_characters_outside_base64url_is_refused() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (_, jwt) = user(&srv).await;

    let mut bad = challenge_for(VERIFIER);
    bad.replace_range(0..1, "+");
    assert_eq!(issue_code(&http, &srv, &jwt, &bad).await.status(), 400);
}

#[tokio::test]
async fn surrounding_whitespace_in_a_challenge_is_trimmed_not_rejected() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (_, jwt) = user(&srv).await;

    let padded = format!("  {}  ", challenge_for(VERIFIER));
    let r = issue_code(&http, &srv, &jwt, &padded).await;
    assert_eq!(r.status(), 200);

    // And the trimmed value is what the verifier is checked against.
    let code = r.json::<Value>().await.unwrap()["code"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(exchange(&http, &srv, &code, VERIFIER).await.status(), 200);
}

// ---------------------------------------------------------------------------
// Redeeming a code
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_holder_of_the_verifier_gets_a_session_token_and_their_profile() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (user_id, code) = code_for_new_user(&http, &srv).await;

    let r = exchange(&http, &srv, &code, VERIFIER).await;

    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(!body["token"].as_str().unwrap().is_empty());
    assert_eq!(body["user"]["id"], user_id.to_string());
    // The token is what the extension stores; the response body is the only place
    // it ever appears.
    assert!(body["user"]["email"].as_str().is_some());
}

#[tokio::test]
async fn a_verifier_that_does_not_match_the_challenge_is_refused() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (_, code) = code_for_new_user(&http, &srv).await;

    let other = "another-verifier-that-is-long-enough-to-pass-1234";
    assert_eq!(exchange(&http, &srv, &code, other).await.status(), 401);
}

#[tokio::test]
async fn a_verifier_outside_the_rfc_length_bounds_is_refused_before_any_work() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (_, code) = code_for_new_user(&http, &srv).await;

    for verifier in ["short", &"x".repeat(42), &"x".repeat(129)] {
        assert_eq!(
            exchange(&http, &srv, &code, verifier).await.status(),
            400,
            "verifier of length {} should be refused",
            verifier.len()
        );
    }
}

#[tokio::test]
async fn a_code_this_server_did_not_sign_is_refused() {
    let srv = skip_without_db!();
    let http = Client::new();

    let forged = issue_jwt("another-secret", &Uuid::new_v4(), "e@x.com", "E", 1).unwrap();
    assert_eq!(exchange(&http, &srv, &forged, VERIFIER).await.status(), 401);
}

#[tokio::test]
async fn a_session_token_cannot_be_presented_as_a_code() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (_, jwt) = user(&srv).await;

    // Signed by us, but not a code: the `kind` claim is what tells them apart.
    assert_eq!(exchange(&http, &srv, &jwt, VERIFIER).await.status(), 401);
}

#[tokio::test]
async fn a_malformed_code_is_refused_rather_than_panicking() {
    let srv = skip_without_db!();
    let http = Client::new();

    for code in ["", "not.a.jwt", "a.b"] {
        assert_eq!(exchange(&http, &srv, code, VERIFIER).await.status(), 401);
    }
}

#[tokio::test]
async fn a_code_for_a_deleted_account_is_refused() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (user_id, code) = code_for_new_user(&http, &srv).await;
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(&srv.pool)
        .await
        .unwrap();

    assert_eq!(exchange(&http, &srv, &code, VERIFIER).await.status(), 401);
}

#[tokio::test]
async fn a_suspended_account_cannot_open_an_extension_session_either() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (user_id, code) = code_for_new_user(&http, &srv).await;
    sqlx::query(
        "UPDATE users SET banned_until = now() + interval '7 days', banned_reason = 'abuse'
          WHERE id = $1",
    )
    .bind(user_id)
    .execute(&srv.pool)
    .await
    .unwrap();

    assert_eq!(exchange(&http, &srv, &code, VERIFIER).await.status(), 403);
}

#[tokio::test]
async fn a_code_is_not_bound_to_one_redemption_but_is_bound_to_its_verifier() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (_, code) = code_for_new_user(&http, &srv).await;

    // The code is a signed, short-lived assertion rather than a stored nonce, so
    // the security property is the verifier, not single use. Stating it here so a
    // change to either half is a deliberate one.
    assert_eq!(exchange(&http, &srv, &code, VERIFIER).await.status(), 200);
    assert_eq!(exchange(&http, &srv, &code, VERIFIER).await.status(), 200);
    assert_eq!(
        exchange(
            &http,
            &srv,
            &code,
            "a-different-verifier-that-is-long-enough-to-pass-1234"
        )
        .await
        .status(),
        401
    );
}

// ---------------------------------------------------------------------------
// The session upgrade
// ---------------------------------------------------------------------------

fn ws_url(srv: &Server, query: &str) -> String {
    format!("ws://{}/ws/extension{query}", srv.addr)
}

#[tokio::test]
async fn a_session_needs_a_usable_target_language() {
    let srv = skip_without_db!();

    for query in [
        "?lang=",                // empty
        "?lang=toolongalang",    // over 8 chars
        "?lang=it_IT",           // underscore is not in the allowed set
        "?lang=it&source=it_IT", // the source is validated too
    ] {
        assert!(
            tokio_tungstenite::connect_async(ws_url(&srv, query))
                .await
                .is_err(),
            "{query} should be refused"
        );
    }
}

#[tokio::test]
async fn auto_is_a_source_language_not_a_target() {
    let srv = skip_without_db!();

    // A target of `auto` makes the fan-out skip this listener and the session
    // produces nothing at all — a silent failure, so it is refused loudly.
    assert!(tokio_tungstenite::connect_async(ws_url(&srv, "?lang=auto"))
        .await
        .is_err());
}

#[tokio::test]
async fn a_session_with_no_language_at_all_is_refused() {
    let srv = skip_without_db!();

    assert!(tokio_tungstenite::connect_async(ws_url(&srv, ""))
        .await
        .is_err());
}
