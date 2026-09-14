//! The two endpoints that make a preference follow the user between devices:
//! `POST /api/user/language` and `POST /api/user/tts-prefs`.
//!
//! Both are partial writes against a column the client controls, so what matters
//! is what they REFUSE — an unknown engine name, a voice id long enough to be an
//! attack on the column, a language code that is not one.
//!
//! DB-gated: skipped without `DATABASE_URL`.

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::Client;
use serde_json::json;
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::Config;
use voxtranslate_server::db::{self, Pool};
use voxtranslate_server::{app, AppState};

const SECRET: &str = "user-prefs-secret";

struct Server {
    addr: SocketAddr,
    pool: Pool,
}

fn base(srv: &Server) -> String {
    format!("http://{}", srv.addr)
}

async fn setup() -> Option<Server> {
    let url = voxtranslate_server::db::test_database_url()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let config = Config::test_with_billing(&url, SECRET, 0.0);
    let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);
    let mut state = AppState::new(config);
    state.billing = Some(BillingService::new(pool.clone(), min_join));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    Some(Server { addr, pool })
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

async fn user(srv: &Server) -> (Uuid, String) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: "Traveller".into(),
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

async fn set_language(http: &Client, srv: &Server, jwt: &str, v: serde_json::Value) -> u16 {
    http.post(format!("{}/api/user/language", base(srv)))
        .bearer_auth(jwt)
        .json(&json!({ "language": v }))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn set_tts(http: &Client, srv: &Server, jwt: &str, body: serde_json::Value) -> u16 {
    http.post(format!("{}/api/user/tts-prefs", base(srv)))
        .bearer_auth(jwt)
        .json(&body)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn stored_language(srv: &Server, uid: Uuid) -> Option<String> {
    sqlx::query_scalar("SELECT language FROM users WHERE id = $1")
        .bind(uid)
        .fetch_one(&srv.pool)
        .await
        .unwrap()
}

async fn stored_tts(srv: &Server, uid: Uuid) -> (Option<String>, Option<String>) {
    sqlx::query_as("SELECT tts_engine_pref, tts_voice_id FROM users WHERE id = $1")
        .bind(uid)
        .fetch_one(&srv.pool)
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// Language
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_language_choice_follows_the_user_to_their_next_device() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv).await;

    assert_eq!(set_language(&http, &srv, &jwt, json!("pt-BR")).await, 204);
    assert_eq!(stored_language(&srv, uid).await.as_deref(), Some("pt-BR"));
}

#[tokio::test]
async fn null_clears_the_choice_rather_than_storing_the_word_null() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv).await;

    assert_eq!(set_language(&http, &srv, &jwt, json!("it")).await, 204);
    assert_eq!(set_language(&http, &srv, &jwt, json!(null)).await, 204);
    // Back to "let the browser decide", which is what the UI's Clear button means.
    assert_eq!(stored_language(&srv, uid).await, None);
}

#[tokio::test]
async fn whitespace_is_the_same_as_clearing_it() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv).await;

    assert_eq!(set_language(&http, &srv, &jwt, json!("fr")).await, 204);
    // An empty string is a client bug, not a language; storing it would make every
    // later read produce a code that matches nothing.
    assert_eq!(set_language(&http, &srv, &jwt, json!("   ")).await, 400);
    assert_eq!(stored_language(&srv, uid).await.as_deref(), Some("fr"));
}

#[tokio::test]
async fn a_value_too_long_to_be_a_language_code_is_refused() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv).await;

    assert_eq!(
        set_language(&http, &srv, &jwt, json!("x".repeat(64))).await,
        400
    );
    assert_eq!(stored_language(&srv, uid).await, None);
}

#[tokio::test]
async fn a_signed_out_visitor_has_no_preferences_to_save() {
    let srv = srv!();
    let http = Client::new();

    let r = http
        .post(format!("{}/api/user/language", base(&srv)))
        .json(&json!({ "language": "it" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

// ---------------------------------------------------------------------------
// Voice
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_engine_and_the_voice_can_be_saved_together_or_apart() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv).await;

    assert_eq!(
        set_tts(
            &http,
            &srv,
            &jwt,
            json!({ "tts_engine_pref": "vox", "tts_voice_id": "voice-1" })
        )
        .await,
        200
    );
    assert_eq!(
        stored_tts(&srv, uid).await,
        (Some("vox".into()), Some("voice-1".into()))
    );

    // Partial: sending only what changed must not wipe the other field.
    assert_eq!(
        set_tts(&http, &srv, &jwt, json!({ "tts_voice_id": "voice-2" })).await,
        200
    );
    assert_eq!(
        stored_tts(&srv, uid).await,
        (Some("vox".into()), Some("voice-2".into()))
    );
}

#[tokio::test]
async fn only_the_three_engines_that_exist_are_accepted() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv).await;

    for good in ["auto", "browser", "vox"] {
        assert_eq!(
            set_tts(&http, &srv, &jwt, json!({ "tts_engine_pref": good })).await,
            200,
            "{good}"
        );
    }
    for junk in ["elevenlabs", "VOX", "", "auto "] {
        assert_eq!(
            set_tts(&http, &srv, &jwt, json!({ "tts_engine_pref": junk })).await,
            400,
            "{junk} was accepted"
        );
    }
    // The last good value survives every refusal.
    assert_eq!(stored_tts(&srv, uid).await.0.as_deref(), Some("vox"));
}

#[tokio::test]
async fn a_voice_id_long_enough_to_be_an_attack_is_refused() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv).await;

    assert_eq!(
        set_tts(
            &http,
            &srv,
            &jwt,
            json!({ "tts_voice_id": "x".repeat(129) })
        )
        .await,
        400
    );
    assert_eq!(stored_tts(&srv, uid).await.1, None);

    // The boundary itself is allowed — it is a cap on abuse, not on real ids.
    assert_eq!(
        set_tts(
            &http,
            &srv,
            &jwt,
            json!({ "tts_voice_id": "x".repeat(128) })
        )
        .await,
        200
    );
}

#[tokio::test]
async fn an_empty_body_changes_nothing_and_is_not_an_error() {
    let srv = srv!();
    let http = Client::new();
    let (uid, jwt) = user(&srv).await;

    set_tts(
        &http,
        &srv,
        &jwt,
        json!({ "tts_engine_pref": "browser", "tts_voice_id": "keep-me" }),
    )
    .await;
    assert_eq!(set_tts(&http, &srv, &jwt, json!({})).await, 200);

    // Both fields absent means "nothing changed", not "clear everything" — a UI
    // that saves on blur would otherwise wipe the choice it just showed.
    assert_eq!(
        stored_tts(&srv, uid).await,
        (Some("browser".into()), Some("keep-me".into()))
    );
}

#[tokio::test]
async fn a_signed_out_visitor_cannot_save_a_voice() {
    let srv = srv!();
    let http = Client::new();

    let r = http
        .post(format!("{}/api/user/tts-prefs", base(&srv)))
        .json(&json!({ "tts_engine_pref": "vox" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}
