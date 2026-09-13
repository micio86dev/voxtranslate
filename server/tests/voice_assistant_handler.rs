//! The B2B Voice Assistant WebSocket handler — its gates, in order.
//!
//! The unit-level pieces (session JSON, credit metering, the semaphore) live in
//! `voice_assistant_integration.rs`. What is exercised here is the handler itself:
//! how it resolves auth for a browser socket that cannot send headers, which roles it
//! admits, what it does when it is half-configured, and — the part that matters most —
//! that an ineligible org is told WHY in-band rather than getting a dead socket.
//!
//! DB-gated: skipped without `DATABASE_URL`, like every other handler test here.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use serde_json::Value;
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::business::voice_assistant::retrieve_rag_chunks;
use voxtranslate_server::config::{Config, VoiceAssistantConfig};
use voxtranslate_server::embeddings::OpenAiEmbeddings;
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::{app, db, AppState};

const SECRET: &str = "voice-assistant-handler-secret";

struct Server {
    addr: SocketAddr,
    pool: db::Pool,
}

fn va_cfg() -> VoiceAssistantConfig {
    VoiceAssistantConfig {
        api_key: "test-key".into(),
        model: "gpt-realtime-2.1".into(),
        cost_per_minute: 0.18,
        markup: 0.25,
        max_sessions: 4,
    }
}

/// Boot the app with the voice assistant configured. `with_embeddings = false`
/// reproduces the half-configured deployment: the route exists, RAG cannot run.
async fn setup(with_embeddings: bool) -> Option<Server> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let mut config = Config::test_with_billing(&url, SECRET, 0.0);
    config.voice_assistant = Some(va_cfg());
    let mut state = AppState::new(config);
    state.safety = Some(SafetyService::new(pool.clone()));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);
    if with_embeddings {
        state.embeddings = Some(OpenAiEmbeddings::new(
            "test-key".into(),
            "text-embedding-3-small".into(),
        ));
    } else {
        state.embeddings = None;
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    Some(Server { addr, pool })
}

/// One user, with a JWT signed by the server's secret.
async fn user(srv: &Server, name: &str) -> (Uuid, String) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: name.into(),
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

/// An org owned by a fresh user, with `role` for that user.
async fn org_with(srv: &Server, role: &str) -> (Uuid, Uuid, String) {
    let (user_id, jwt) = user(srv, "VA User").await;
    let org: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id) VALUES ('VA Co', $1, $2) RETURNING id",
    )
    .bind(format!("va-{}", Uuid::new_v4().simple()))
    .bind(user_id)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, $3)")
        .bind(org)
        .bind(user_id)
        .bind(role)
        .execute(&srv.pool)
        .await
        .unwrap();
    (org, user_id, jwt)
}

/// Give the org a live subscription and a funded credit pool.
async fn make_eligible(srv: &Server, org: Uuid) {
    sqlx::query(
        "UPDATE organizations
            SET subscription_status = 'active',
                current_period_end = now() + interval '30 days',
                credits_balance = 5000
          WHERE id = $1",
    )
    .bind(org)
    .execute(&srv.pool)
    .await
    .unwrap();
}

fn ws_url(srv: &Server, org: Uuid, query: &str) -> String {
    format!(
        "ws://{}/api/business/organizations/{org}/voice-assistant{query}",
        srv.addr
    )
}

/// Open the socket and return its first text frame.
async fn first_frame(srv: &Server, org: Uuid, jwt: &str) -> Value {
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url(srv, org, &format!("?token={jwt}")))
        .await
        .expect("the upgrade must succeed — the reason travels in-band");
    loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
            Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t)))) => {
                return serde_json::from_str(t.as_str()).expect("frame is JSON");
            }
            Ok(Some(Ok(_))) => continue,
            other => panic!("no text frame before close: {other:?}"),
        }
    }
}

macro_rules! skip_without_db {
    ($with_embeddings:expr) => {
        match setup($with_embeddings).await {
            Some(srv) => srv,
            None => {
                eprintln!("skipping — no DATABASE_URL");
                return;
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Authentication — a browser socket cannot send headers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_socket_with_no_credential_is_refused() {
    let srv = skip_without_db!(true);
    let (org, _, _) = org_with(&srv, "owner").await;

    assert!(
        tokio_tungstenite::connect_async(ws_url(&srv, org, ""))
            .await
            .is_err(),
        "an unauthenticated socket must not be upgraded"
    );
}

#[tokio::test]
async fn a_forged_token_is_refused() {
    let srv = skip_without_db!(true);
    let (org, _, _) = org_with(&srv, "owner").await;

    let forged = issue_jwt("a-different-secret", &Uuid::new_v4(), "e@x.com", "E", 1).unwrap();
    assert!(
        tokio_tungstenite::connect_async(ws_url(&srv, org, &format!("?token={forged}")))
            .await
            .is_err(),
        "a token this server did not sign must not open a socket"
    );
}

#[tokio::test]
async fn the_authorization_header_is_preferred_when_present() {
    let srv = skip_without_db!(true);
    let (org, _, jwt) = org_with(&srv, "owner").await;
    make_eligible(&srv, org).await;

    // Non-browser callers (curl, tests) send a header; the query param is the
    // fallback, not the only path.
    let request = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
        ws_url(&srv, org, ""),
    )
    .map(|mut r| {
        r.headers_mut()
            .insert("authorization", format!("Bearer {jwt}").parse().unwrap());
        r
    })
    .unwrap();

    assert!(
        tokio_tungstenite::connect_async(request).await.is_ok(),
        "a bearer header must authenticate the upgrade"
    );
}

#[tokio::test]
async fn a_malformed_authorization_header_does_not_fall_back_to_the_query_param() {
    let srv = skip_without_db!(true);
    let (org, _, jwt) = org_with(&srv, "owner").await;
    make_eligible(&srv, org).await;

    // The header is present but unusable. Silently reading the query param here
    // would let a header that failed validation be bypassed by one that did not.
    let request = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
        ws_url(&srv, org, &format!("?token={jwt}")),
    )
    .map(|mut r| {
        r.headers_mut()
            .insert("authorization", "Token not-a-bearer".parse().unwrap());
        r
    })
    .unwrap();

    assert!(
        tokio_tungstenite::connect_async(request).await.is_err(),
        "a malformed Authorization header must refuse, not fall through"
    );
}

// ---------------------------------------------------------------------------
// Authorization — team leads and owners only
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_outsider_is_refused_before_the_upgrade() {
    let srv = skip_without_db!(true);
    let (org, _, _) = org_with(&srv, "owner").await;
    let (_, outsider_jwt) = user(&srv, "Outsider").await;

    assert!(
        tokio_tungstenite::connect_async(ws_url(&srv, org, &format!("?token={outsider_jwt}")))
            .await
            .is_err(),
        "someone outside the org must not get a socket at all"
    );
}

#[tokio::test]
async fn a_plain_member_is_refused() {
    let srv = skip_without_db!(true);
    let (org, _, jwt) = org_with(&srv, "member").await;
    make_eligible(&srv, org).await;

    // The assistant reads transcripts, so it is gated like insights: leads and
    // owners only.
    assert!(
        tokio_tungstenite::connect_async(ws_url(&srv, org, &format!("?token={jwt}")))
            .await
            .is_err(),
        "a member must not be able to query the org's transcripts"
    );
}

#[tokio::test]
async fn an_unknown_org_is_refused() {
    let srv = skip_without_db!(true);
    let (_, jwt) = user(&srv, "Nobody").await;

    assert!(tokio_tungstenite::connect_async(ws_url(
        &srv,
        Uuid::new_v4(),
        &format!("?token={jwt}")
    ))
    .await
    .is_err());
}

#[tokio::test]
async fn an_admin_passes_the_role_gate() {
    let srv = skip_without_db!(true);
    let (org, _, jwt) = org_with(&srv, "admin").await;

    // Not eligible yet, so the frame is a refusal — but it is an IN-BAND one,
    // which proves the role gate let the upgrade through.
    let frame = first_frame(&srv, org, &jwt).await;
    assert_eq!(frame["type"], "error");
    assert_eq!(frame["code"], "subscription_required");
}

// ---------------------------------------------------------------------------
// Half-configured deployments
// ---------------------------------------------------------------------------

#[tokio::test]
async fn without_an_embeddings_provider_the_socket_is_refused_outright() {
    let srv = skip_without_db!(false);
    let (org, _, jwt) = org_with(&srv, "owner").await;
    make_eligible(&srv, org).await;

    // There is no useful assistant without RAG, and nothing the user can do about
    // it — so this one is not reported in-band.
    assert!(
        tokio_tungstenite::connect_async(ws_url(&srv, org, &format!("?token={jwt}")))
            .await
            .is_err()
    );
}

// ---------------------------------------------------------------------------
// Eligibility — reported in-band, never as a dead socket
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_lapsed_subscription_is_explained_in_band() {
    let srv = skip_without_db!(true);
    let (org, _, jwt) = org_with(&srv, "owner").await;

    // The gifted-subscription shape: nothing ever flipped the row to 'canceled',
    // so status still reads 'active' and only the date says otherwise.
    sqlx::query(
        "UPDATE organizations
            SET subscription_status = 'active',
                current_period_end = now() - interval '8 days',
                credits_balance = 5000
          WHERE id = $1",
    )
    .bind(org)
    .execute(&srv.pool)
    .await
    .unwrap();

    let frame = first_frame(&srv, org, &jwt).await;
    assert_eq!(frame["type"], "error");
    assert_eq!(frame["code"], "subscription_required");
    // Without an action the dashboard can only show text; with it, a button.
    assert_eq!(frame["action"], "purchase_subscription");
}

#[tokio::test]
async fn an_empty_credit_pool_is_explained_in_band() {
    let srv = skip_without_db!(true);
    let (org, _, jwt) = org_with(&srv, "owner").await;
    sqlx::query(
        "UPDATE organizations
            SET subscription_status = 'active',
                current_period_end = now() + interval '30 days',
                credits_balance = 2
          WHERE id = $1",
    )
    .bind(org)
    .execute(&srv.pool)
    .await
    .unwrap();

    let frame = first_frame(&srv, org, &jwt).await;
    assert_eq!(frame["type"], "error");
    assert_eq!(frame["code"], "insufficient_credits");
    assert_eq!(frame["action"], "purchase_credits");
    assert_eq!(frame["balance"], 2);
}

// ---------------------------------------------------------------------------
// Scope parameters
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_project_scope_is_accepted_and_resolved() {
    let srv = skip_without_db!(true);
    let (org, user_id, jwt) = org_with(&srv, "owner").await;
    make_eligible(&srv, org).await;

    let project: Uuid = sqlx::query_scalar(
        "INSERT INTO projects (org_id, name, created_by) VALUES ($1, 'Nord', $2) RETURNING id",
    )
    .bind(org)
    .bind(user_id)
    .fetch_one(&srv.pool)
    .await
    .unwrap();

    let url = ws_url(&srv, org, &format!("?token={jwt}&project_id={project}"));
    assert!(
        tokio_tungstenite::connect_async(url).await.is_ok(),
        "a valid project scope must not refuse the upgrade"
    );
}

#[tokio::test]
async fn a_member_scope_is_accepted_and_resolved() {
    let srv = skip_without_db!(true);
    let (org, user_id, jwt) = org_with(&srv, "owner").await;
    make_eligible(&srv, org).await;

    let url = ws_url(&srv, org, &format!("?token={jwt}&member_id={user_id}"));
    assert!(tokio_tungstenite::connect_async(url).await.is_ok());
}

#[tokio::test]
async fn a_scope_naming_something_that_does_not_exist_is_simply_unscoped() {
    let srv = skip_without_db!(true);
    let (org, _, jwt) = org_with(&srv, "owner").await;
    make_eligible(&srv, org).await;

    // Best-effort by design: an unknown project or member narrows nothing rather
    // than failing a session the user could not have caused.
    let url = ws_url(
        &srv,
        org,
        &format!(
            "?token={jwt}&project_id={}&member_id={}",
            Uuid::new_v4(),
            Uuid::new_v4()
        ),
    );
    assert!(tokio_tungstenite::connect_async(url).await.is_ok());
}

#[tokio::test]
async fn an_unparseable_scope_is_rejected_by_the_query_extractor() {
    let srv = skip_without_db!(true);
    let (org, _, jwt) = org_with(&srv, "owner").await;
    make_eligible(&srv, org).await;

    assert!(tokio_tungstenite::connect_async(ws_url(
        &srv,
        org,
        &format!("?token={jwt}&project_id=not-a-uuid")
    ))
    .await
    .is_err());
}

// ---------------------------------------------------------------------------
// RAG retrieval
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rag_retrieval_reports_a_failed_embedding_rather_than_panicking() {
    let srv = skip_without_db!(true);
    let (org, _, _) = org_with(&srv, "owner").await;

    // The key is fake, so the embeddings call fails. The handler treats that as
    // "continue with an empty knowledge base"; the function itself must return a
    // readable error rather than unwrap.
    let embedder = OpenAiEmbeddings::new("test-key".into(), "text-embedding-3-small".into());
    let result = retrieve_rag_chunks(&srv.pool, &embedder, org, None, None, "seed").await;

    let err = result.expect_err("a fake key cannot produce an embedding");
    assert!(
        err.contains("embedding failed"),
        "the error should say which step failed, got: {err}"
    );
}
