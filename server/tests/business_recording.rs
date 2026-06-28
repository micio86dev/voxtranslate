//! Integration tests for business cloud-recording endpoints (spec 0106): the signed
//! upload-URL issuance, `complete` (path validation, org charge, insufficient-credits
//! revert), and the signed playback URL — with a MOCKED Supabase Storage (a raw TCP
//! server returning canned signed-URL JSON) injected into `recordings_storage`, so the
//! happy paths run without real Supabase. DB-gated; skipped without `DATABASE_URL`.

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::Client;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::business::credits::add_org_credits;
use voxtranslate_server::config::{Config, StorageConfig};
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::storage::SupabaseStorage;
use voxtranslate_server::transcripts::TranscriptService;
use voxtranslate_server::{app, db, AppState};

const SECRET: &str = "biz-recording-secret";

struct Server {
    addr: SocketAddr,
    pool: db::Pool,
}

fn base(srv: &Server) -> String {
    format!("http://{}", srv.addr)
}

/// A raw mock HTTP server that replies with `response` to every request, then closes
/// (connection-close delimits the body). Returns the base URL.
async fn spawn_mock(response: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.flush().await;
            });
        }
    });
    format!("http://{addr}")
}

const SIGN_UPLOAD_OK: &str =
    "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{\"url\":\"/object/upload/sign/recordings/p?token=up123\"}";
const SIGN_DOWNLOAD_OK: &str =
    "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{\"signedURL\":\"/object/sign/recordings/p?token=dl123\"}";
const ANY_OK: &str = "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n";

async fn setup(mock_base: &str) -> Option<Server> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let config = Config::test_with_billing(&url, SECRET, 0.0);
    let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);
    let mut state = AppState::new(config);
    state.billing = Some(BillingService::new(pool.clone(), min_join));
    state.safety = Some(SafetyService::new(pool.clone()));
    state.transcripts = Some(TranscriptService::new(pool.clone()));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);
    let scfg = StorageConfig {
        supabase_url: mock_base.to_string(),
        service_key: "svc".into(),
        bucket: "recordings".into(),
        max_bytes: 25 * 1024 * 1024,
        signed_ttl_secs: 3600,
    };
    state.recordings_storage = Some(SupabaseStorage::for_bucket(
        Client::new(),
        &scfg,
        "recordings".into(),
        3600,
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    Some(Server { addr, pool })
}

async fn user(srv: &Server) -> (Uuid, String) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: "Recorder".into(),
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
    (u.id, jwt)
}

async fn make_org(srv: &Server, owner: Uuid) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id) VALUES ('Rec Co', $1, $2) RETURNING id",
    )
    .bind(format!("rec-{}", Uuid::new_v4().simple()))
    .bind(owner)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(id)
    .bind(owner)
    .execute(&srv.pool)
    .await
    .unwrap();
    id
}

async fn make_call(srv: &Server, org: Uuid, recording_enabled: bool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO call_sessions (id, room, org_id, transcript_status, cloud_recording_enabled)
         VALUES ($1, $2, $3, 'none', $4)",
    )
    .bind(id)
    .bind(format!("room-{}", Uuid::new_v4().simple()))
    .bind(org)
    .bind(recording_enabled)
    .execute(&srv.pool)
    .await
    .unwrap();
    id
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

#[tokio::test]
async fn upload_url_happy_and_disabled() {
    let mock = spawn_mock(SIGN_UPLOAD_OK).await;
    let srv = skip_without_db!(setup(&mock).await);
    let (uid, jwt) = user(&srv).await;
    let org = make_org(&srv, uid).await;

    // Enabled call → a signed upload URL scoped to {org}/{session}/.
    let call = make_call(&srv, org, true).await;
    let r = Client::new()
        .post(format!(
            "{}/api/business/rooms/{}/recording/upload-url",
            base(&srv),
            call
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert!(v["upload_url"].as_str().unwrap().contains("token=up123"));
    assert!(v["object_path"]
        .as_str()
        .unwrap()
        .starts_with(&format!("{org}/{call}/")));

    // Recording not enabled → forbidden.
    let call2 = make_call(&srv, org, false).await;
    let r = Client::new()
        .post(format!(
            "{}/api/business/rooms/{}/recording/upload-url",
            base(&srv),
            call2
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
}

#[tokio::test]
async fn playback_url_happy_and_missing() {
    let mock = spawn_mock(SIGN_DOWNLOAD_OK).await;
    let srv = skip_without_db!(setup(&mock).await);
    let (uid, jwt) = user(&srv).await;
    let org = make_org(&srv, uid).await;
    let call = make_call(&srv, org, true).await;

    // No recording stored yet → 404.
    let r = Client::new()
        .get(format!(
            "{}/api/business/rooms/{}/recording/url",
            base(&srv),
            call
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    // Store a path → signed playback URL.
    sqlx::query("UPDATE call_sessions SET recording_storage_path = $2 WHERE id = $1")
        .bind(call)
        .bind(format!("{org}/{call}/rec.webm"))
        .execute(&srv.pool)
        .await
        .unwrap();
    let r = Client::new()
        .get(format!(
            "{}/api/business/rooms/{}/recording/url",
            base(&srv),
            call
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert!(v["url"].as_str().unwrap().contains("token=dl123"));
    assert_eq!(v["expires_in"], 3600);
}

#[tokio::test]
async fn complete_charges_validates_path_and_handles_insufficient() {
    let mock = spawn_mock(ANY_OK).await;
    let srv = skip_without_db!(setup(&mock).await);
    let (uid, jwt) = user(&srv).await;
    let org = make_org(&srv, uid).await;
    let call = make_call(&srv, org, true).await;

    // A path outside the {org}/{session}/ namespace is rejected.
    let r = Client::new()
        .post(format!(
            "{}/api/business/rooms/{}/recording/complete",
            base(&srv),
            call
        ))
        .bearer_auth(&jwt)
        .json(&json!({ "object_path": "someone-else/evil.webm", "duration_seconds": 60 }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    let good_path = format!("{org}/{call}/rec.webm");

    // No org credits → 402 and the status is reverted to 'none'.
    let r = Client::new()
        .post(format!(
            "{}/api/business/rooms/{}/recording/complete",
            base(&srv),
            call
        ))
        .bearer_auth(&jwt)
        .json(&json!({ "object_path": good_path, "duration_seconds": 600 }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 402);
    let status: String =
        sqlx::query_scalar("SELECT transcript_status FROM call_sessions WHERE id = $1")
            .bind(call)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(status, "none");

    // Fund the org → complete succeeds, path stored, transcription enqueued.
    add_org_credits(&srv.pool, org, 10_000, "grant", "test top-up", None)
        .await
        .unwrap();
    let r = Client::new()
        .post(format!(
            "{}/api/business/rooms/{}/recording/complete",
            base(&srv),
            call
        ))
        .bearer_auth(&jwt)
        .json(&json!({ "object_path": good_path, "duration_seconds": 600 }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["transcript_status"], "processing");
    let stored: Option<String> =
        sqlx::query_scalar("SELECT recording_storage_path FROM call_sessions WHERE id = $1")
            .bind(call)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(stored.as_deref(), Some(good_path.as_str()));
}

#[tokio::test]
async fn recording_endpoints_require_membership() {
    let mock = spawn_mock(SIGN_DOWNLOAD_OK).await;
    let srv = skip_without_db!(setup(&mock).await);
    let (owner, _) = user(&srv).await;
    let org = make_org(&srv, owner).await;
    let call = make_call(&srv, org, true).await;
    // A different, non-member user is forbidden (require_call_role).
    let (_stranger, other_jwt) = user(&srv).await;
    let r = Client::new()
        .get(format!(
            "{}/api/business/rooms/{}/recording/url",
            base(&srv),
            call
        ))
        .bearer_auth(&other_jwt)
        .send()
        .await
        .unwrap();
    assert!(r.status() == 403 || r.status() == 404, "got {}", r.status());
}
