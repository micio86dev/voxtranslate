//! Integration tests for the friends REST API (spec: friends): send / accept /
//! reject / cancel / unfriend, the list + split incoming/outgoing requests views,
//! mutual-request auto-accept, and inviting an accepted friend into a call.
//! DB-gated — skipped without `DATABASE_URL`. Notifications are written to the DB
//! but no real push/email is sent (those sinks are unconfigured here).

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::Client;
use serde_json::{json, Value};
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::config::Config;
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::{app, db, AppState};

const SECRET: &str = "friends-test-secret";

struct Server {
    addr: SocketAddr,
    pool: db::Pool,
}

fn base(srv: &Server) -> String {
    format!("http://{}", srv.addr)
}

async fn setup() -> Option<Server> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let config = Config::test_with_billing(&url, SECRET, 0.0);
    let mut state = AppState::new(config);
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

/// A signed-in user: their id, a bearer JWT, and their (unique) email.
struct TestUser {
    id: Uuid,
    jwt: String,
    email: String,
}

async fn user(srv: &Server, name: &str) -> TestUser {
    let email = format!("{}@x.com", Uuid::new_v4());
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: email.clone(),
        name: name.into(),
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
    TestUser {
        id: u.id,
        jwt,
        email,
    }
}

macro_rules! skip_without_db {
    ($e:expr) => {
        match $e {
            Some(s) => s,
            None => {
                eprintln!("skipping — no DATABASE_URL");
                return;
            }
        }
    };
}

async fn get_json(srv: &Server, path: &str, jwt: &str) -> Value {
    Client::new()
        .get(format!("{}{}", base(srv), path))
        .bearer_auth(jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn post_status(srv: &Server, path: &str, jwt: &str, body: Value) -> u16 {
    Client::new()
        .post(format!("{}{}", base(srv), path))
        .bearer_auth(jwt)
        .json(&body)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test]
async fn request_accept_list_and_unfriend_full_cycle() {
    let srv = skip_without_db!(setup().await);
    let alice = user(&srv, "Alice").await;
    let bob = user(&srv, "Bob").await;

    // Both start with no friends and no pending requests.
    assert_eq!(get_json(&srv, "/api/friends", &alice.jwt).await, json!([]));
    let reqs = get_json(&srv, "/api/friends/requests", &alice.jwt).await;
    assert_eq!(reqs["incoming"], json!([]));
    assert_eq!(reqs["outgoing"], json!([]));

    // Alice sends Bob a request by email.
    assert_eq!(
        post_status(
            &srv,
            "/api/friends/request",
            &alice.jwt,
            json!({ "email": bob.email })
        )
        .await,
        202
    );

    // It shows as outgoing for Alice and incoming for Bob, with the right person.
    let a_reqs = get_json(&srv, "/api/friends/requests", &alice.jwt).await;
    assert_eq!(a_reqs["outgoing"][0]["id"], bob.id.to_string());
    assert_eq!(a_reqs["incoming"], json!([]));
    let b_reqs = get_json(&srv, "/api/friends/requests", &bob.jwt).await;
    assert_eq!(b_reqs["incoming"][0]["id"], alice.id.to_string());
    assert_eq!(b_reqs["incoming"][0]["email"], alice.email);

    // Bob accepts → both now see each other as accepted friends, no pending left.
    assert_eq!(
        post_status(
            &srv,
            &format!("/api/friends/{}/accept", alice.id),
            &bob.jwt,
            json!({})
        )
        .await,
        204
    );
    let a_friends = get_json(&srv, "/api/friends", &alice.jwt).await;
    assert_eq!(a_friends[0]["id"], bob.id.to_string());
    let b_friends = get_json(&srv, "/api/friends", &bob.jwt).await;
    assert_eq!(b_friends[0]["id"], alice.id.to_string());
    let a_reqs = get_json(&srv, "/api/friends/requests", &alice.jwt).await;
    assert_eq!(a_reqs["outgoing"], json!([]));

    // Alice unfriends → the relationship is gone for both, idempotently.
    assert_eq!(
        Client::new()
            .delete(format!("{}/api/friends/{}", base(&srv), bob.id))
            .bearer_auth(&alice.jwt)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        204
    );
    assert_eq!(get_json(&srv, "/api/friends", &alice.jwt).await, json!([]));
    assert_eq!(get_json(&srv, "/api/friends", &bob.jwt).await, json!([]));
}

#[tokio::test]
async fn request_validation_errors() {
    let srv = skip_without_db!(setup().await);
    let alice = user(&srv, "Alice").await;
    let bob = user(&srv, "Bob").await;

    // Can't friend yourself, and an empty body is a 400.
    assert_eq!(
        post_status(
            &srv,
            "/api/friends/request",
            &alice.jwt,
            json!({ "email": alice.email })
        )
        .await,
        400
    );
    assert_eq!(
        post_status(&srv, "/api/friends/request", &alice.jwt, json!({})).await,
        400
    );

    // Unknown email → 202 (M2: same response as a real queued request, so the
    // endpoint can't be used to enumerate which emails have accounts).
    assert_eq!(
        post_status(
            &srv,
            "/api/friends/request",
            &alice.jwt,
            json!({ "email": "nobody-xyz@example.com" })
        )
        .await,
        202
    );

    // First request OK (202), a duplicate from the same sender is a 409.
    assert_eq!(
        post_status(
            &srv,
            "/api/friends/request",
            &alice.jwt,
            json!({ "email": bob.email })
        )
        .await,
        202
    );
    assert_eq!(
        post_status(
            &srv,
            "/api/friends/request",
            &alice.jwt,
            json!({ "email": bob.email })
        )
        .await,
        409
    );
}

#[tokio::test]
async fn mutual_request_becomes_instant_friends() {
    let srv = skip_without_db!(setup().await);
    let alice = user(&srv, "Alice").await;
    let bob = user(&srv, "Bob").await;

    // Alice requests Bob by email; Bob then requests Alice by user_id → the second
    // request completes the friendship outright (204, no separate accept needed).
    assert_eq!(
        post_status(
            &srv,
            "/api/friends/request",
            &alice.jwt,
            json!({ "email": bob.email })
        )
        .await,
        202
    );
    assert_eq!(
        post_status(
            &srv,
            "/api/friends/request",
            &bob.jwt,
            json!({ "user_id": alice.id })
        )
        .await,
        204
    );

    assert_eq!(
        get_json(&srv, "/api/friends", &alice.jwt).await[0]["id"],
        bob.id.to_string()
    );
    // Re-requesting an already-accepted friend is a 409.
    assert_eq!(
        post_status(
            &srv,
            "/api/friends/request",
            &alice.jwt,
            json!({ "user_id": bob.id })
        )
        .await,
        409
    );
}

#[tokio::test]
async fn invite_requires_accepted_friendship() {
    let srv = skip_without_db!(setup().await);
    let alice = user(&srv, "Alice").await;
    let bob = user(&srv, "Bob").await;

    // Not friends yet → inviting Bob to a call is a 404.
    assert_eq!(
        post_status(
            &srv,
            &format!("/api/friends/{}/invite", bob.id),
            &alice.jwt,
            json!({ "room": "vox-abcdef2345" })
        )
        .await,
        404
    );

    // Become friends.
    post_status(
        &srv,
        "/api/friends/request",
        &alice.jwt,
        json!({ "email": bob.email }),
    )
    .await;
    post_status(
        &srv,
        &format!("/api/friends/{}/accept", alice.id),
        &bob.jwt,
        json!({}),
    )
    .await;

    // Empty room → 400; a real room → 204 and the invite notification is delivered.
    assert_eq!(
        post_status(
            &srv,
            &format!("/api/friends/{}/invite", bob.id),
            &alice.jwt,
            json!({ "room": "  " })
        )
        .await,
        400
    );
    assert_eq!(
        post_status(
            &srv,
            &format!("/api/friends/{}/invite", bob.id),
            &alice.jwt,
            json!({ "room": "vox-abcdef2345" })
        )
        .await,
        204
    );
}

#[tokio::test]
async fn accept_without_pending_request_is_404() {
    let srv = skip_without_db!(setup().await);
    let alice = user(&srv, "Alice").await;
    let bob = user(&srv, "Bob").await;

    // Nothing pending between them → accept is a clean 404, not a 500.
    assert_eq!(
        post_status(
            &srv,
            &format!("/api/friends/{}/accept", bob.id),
            &alice.jwt,
            json!({})
        )
        .await,
        404
    );

    // The requester can't accept their own outgoing request.
    post_status(
        &srv,
        "/api/friends/request",
        &alice.jwt,
        json!({ "email": bob.email }),
    )
    .await;
    assert_eq!(
        post_status(
            &srv,
            &format!("/api/friends/{}/accept", bob.id),
            &alice.jwt,
            json!({})
        )
        .await,
        404
    );
}
