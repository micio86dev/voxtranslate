//! Webinar chat file upload — `POST /api/w/{code}/files`.
//!
//! Supabase Storage is replaced by a local mock that speaks the same two REST
//! endpoints, so the handler runs end to end — multipart parse, the access and
//! chat gates, the type and size limits, the upload, the signed URL — without a
//! network call or a real bucket. The mock can be told to fail either call, which
//! is the only way to reach the two `502` branches.
//!
//! DB-gated: skipped without `DATABASE_URL`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;
use reqwest::multipart::{Form, Part};
use reqwest::Client;
use serde_json::{json, Value};
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::{Config, StorageConfig, WebinarConfig};
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::storage::SupabaseStorage;
use voxtranslate_server::{app, db, AppState};

const SECRET: &str = "webinar-files-secret";

// ---------------------------------------------------------------------------
// A local stand-in for Supabase Storage
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct MockStorage {
    /// Objects that were actually uploaded, as `path` → byte length.
    uploads: Arc<std::sync::Mutex<Vec<(String, usize)>>>,
    upload_fails: Arc<AtomicBool>,
    sign_fails: Arc<AtomicBool>,
    /// Return a 200 whose body carries no `signedURL` at all.
    sign_returns_nothing: Arc<AtomicBool>,
}

async fn mock_upload(
    State(mock): State<MockStorage>,
    Path((_bucket, object)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    if mock.upload_fails.load(Ordering::SeqCst) {
        return (StatusCode::INSUFFICIENT_STORAGE, "bucket full").into_response();
    }
    mock.uploads.lock().unwrap().push((object, body.len()));
    (StatusCode::OK, "{}").into_response()
}

async fn mock_sign(
    State(mock): State<MockStorage>,
    Path((bucket, object)): Path<(String, String)>,
) -> axum::response::Response {
    if mock.sign_fails.load(Ordering::SeqCst) {
        return (StatusCode::NOT_FOUND, "object not found").into_response();
    }
    if mock.sign_returns_nothing.load(Ordering::SeqCst) {
        return axum::Json(json!({})).into_response();
    }
    axum::Json(json!({
        "signedURL": format!("/object/sign/{bucket}/{object}?token=mock-token"),
    }))
    .into_response()
}

/// Boot the mock and return its base URL plus a handle to steer it.
async fn mock_storage() -> (String, MockStorage) {
    let mock = MockStorage::default();
    let router = Router::new()
        .route(
            "/storage/v1/object/sign/{bucket}/{*object}",
            post(mock_sign),
        )
        .route("/storage/v1/object/{bucket}/{*object}", post(mock_upload))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{addr}"), mock)
}

// ---------------------------------------------------------------------------
// Server harness
// ---------------------------------------------------------------------------

struct Server {
    addr: SocketAddr,
    pool: db::Pool,
    storage: MockStorage,
}

fn storage_cfg(base_url: String, max_bytes: usize) -> StorageConfig {
    StorageConfig {
        supabase_url: base_url,
        service_key: "service-key".into(),
        bucket: "chat-files".into(),
        max_bytes,
        signed_ttl_secs: 3600,
    }
}

/// `storage = false` reproduces a deployment with no bucket configured.
async fn setup_with(storage: bool, max_bytes: usize) -> Option<Server> {
    let url = voxtranslate_server::db::test_database_url()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;

    let (base_url, mock) = mock_storage().await;
    let mut config = Config::test_with_billing(&url, SECRET, 0.0);
    config.webinar = Some(WebinarConfig::test_default());
    if storage {
        config.storage = Some(storage_cfg(base_url.clone(), max_bytes));
    }
    let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);
    let has_storage = config.storage.is_some();
    let storage_client = config
        .storage
        .as_ref()
        .map(|c| SupabaseStorage::new(reqwest::Client::new(), c));
    let mut state = AppState::new(config);
    state.billing = Some(BillingService::new(pool.clone(), min_join));
    state.safety = Some(SafetyService::new(pool.clone()));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);
    if has_storage {
        state.storage = storage_client;
    } else {
        state.storage = None;
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    Some(Server {
        addr,
        pool,
        storage: mock,
    })
}

async fn setup() -> Option<Server> {
    setup_with(true, 8 * 1024 * 1024).await
}

fn base(srv: &Server) -> String {
    format!("http://{}", srv.addr)
}

async fn user(srv: &Server) -> (Uuid, String) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: "Host".into(),
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

/// An org with a live subscription, so the webinar routes are open.
async fn org(srv: &Server, owner: Uuid) -> Uuid {
    let org_id: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id, subscription_status, current_period_end)
         VALUES ($1, $2, $3, 'active', now() + interval '30 days')
         RETURNING id",
    )
    .bind("Acme")
    .bind(format!("wf-{}", Uuid::new_v4().simple()))
    .bind(owner)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(org_id)
    .bind(owner)
    .execute(&srv.pool)
    .await
    .unwrap();
    org_id
}

/// Create a webinar and return its join code.
async fn webinar(http: &Client, srv: &Server, jwt: &str, org_id: Uuid, chat: bool) -> String {
    let r = http
        .post(format!("{}/api/webinars", base(srv)))
        .bearer_auth(jwt)
        .json(&json!({
            "org_id": org_id,
            "title": "Launch",
            "source_language": "en",
            "chat_enabled": chat,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "create webinar");
    let body: Value = r.json().await.unwrap();
    body["code"].as_str().unwrap().to_string()
}

/// POST one file to the webinar's upload endpoint.
async fn upload(
    http: &Client,
    srv: &Server,
    code: &str,
    file_name: &str,
    bytes: Vec<u8>,
) -> reqwest::Response {
    let part = Part::bytes(bytes).file_name(file_name.to_string());
    http.post(format!("{}/api/w/{code}/files", base(srv)))
        .multipart(Form::new().part("file", part))
        .send()
        .await
        .unwrap()
}

macro_rules! skip_without_db {
    ($setup:expr) => {
        match $setup {
            Some(srv) => srv,
            None => {
                eprintln!("skipping — no DATABASE_URL");
                return;
            }
        }
    };
}

/// A chat-enabled webinar, ready to receive an upload.
async fn chat_webinar(srv: &Server, http: &Client) -> String {
    let (owner, jwt) = user(srv).await;
    let org_id = org(srv, owner).await;
    webinar(http, srv, &jwt, org_id, true).await
}

// ---------------------------------------------------------------------------
// The happy path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_accepted_upload_returns_the_metadata_chat_needs() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let code = chat_webinar(&srv, &http).await;

    let r = upload(&http, &srv, &code, "notes.txt", b"hello webinar".to_vec()).await;

    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["name"], "notes.txt");
    assert_eq!(body["content_type"], "text/plain");
    assert_eq!(body["size"], 13);
    assert!(
        body["url"].as_str().unwrap().contains("token=mock-token"),
        "the client gets a signed URL, not a bucket path"
    );
}

#[tokio::test]
async fn the_object_is_namespaced_by_webinar_code_and_given_a_fresh_id() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let code = chat_webinar(&srv, &http).await;

    upload(&http, &srv, &code, "a.txt", b"one".to_vec()).await;
    upload(&http, &srv, &code, "a.txt", b"two".to_vec()).await;

    let uploads = srv.storage.uploads.lock().unwrap().clone();
    assert_eq!(uploads.len(), 2);
    for (path, _) in &uploads {
        assert!(
            path.starts_with(&format!("wv-{code}/")),
            "object should sit under its webinar: {path}"
        );
        assert!(path.ends_with(".txt"));
    }
    assert_ne!(
        uploads[0].0, uploads[1].0,
        "the same filename twice must not overwrite the first upload"
    );
}

#[tokio::test]
async fn the_content_type_comes_from_the_extension_not_from_the_client() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let code = chat_webinar(&srv, &http).await;

    // A client that mislabels its bytes must not get them served back as that type.
    let part = Part::bytes(b"%PDF-1.4".to_vec())
        .file_name("report.pdf".to_string())
        .mime_str("text/html")
        .unwrap();
    let r = http
        .post(format!("{}/api/w/{code}/files", base(&srv)))
        .multipart(Form::new().part("file", part))
        .send()
        .await
        .unwrap();

    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["content_type"], "application/pdf");
}

#[tokio::test]
async fn an_image_and_an_audio_note_are_both_accepted() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let code = chat_webinar(&srv, &http).await;

    for (name, expected) in [("shot.png", "image/png"), ("note.webm", "audio/webm")] {
        let r = upload(&http, &srv, &code, name, b"bytes".to_vec()).await;
        assert_eq!(r.status(), 200, "{name} should be accepted");
        let body: Value = r.json().await.unwrap();
        assert_eq!(body["content_type"], expected);
    }
}

#[tokio::test]
async fn a_case_insensitive_extension_is_accepted() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let code = chat_webinar(&srv, &http).await;

    let r = upload(&http, &srv, &code, "SHOUTING.TXT", b"x".to_vec()).await;
    assert_eq!(r.status(), 200);
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

#[tokio::test]
async fn without_a_bucket_the_endpoint_says_so_rather_than_failing_later() {
    let srv = skip_without_db!(setup_with(false, 0).await);
    let http = Client::new();
    let code = chat_webinar(&srv, &http).await;

    let r = upload(&http, &srv, &code, "notes.txt", b"x".to_vec()).await;
    assert_eq!(r.status(), 503);
}

#[tokio::test]
async fn an_unknown_webinar_is_a_404() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();

    let r = upload(&http, &srv, "NOPE12345", "notes.txt", b"x".to_vec()).await;
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn a_webinar_with_chat_off_refuses_the_upload() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner).await;
    let code = webinar(&http, &srv, &jwt, org_id, false).await;

    // Uploading attaches content to a chat that does not exist.
    let r = upload(&http, &srv, &code, "notes.txt", b"x".to_vec()).await;
    assert_eq!(r.status(), 403);
}

#[tokio::test]
async fn a_members_only_webinar_refuses_an_anonymous_upload() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let code = chat_webinar(&srv, &http).await;
    sqlx::query("UPDATE webinars SET members_only = true WHERE code = $1")
        .bind(&code)
        .execute(&srv.pool)
        .await
        .unwrap();

    // Uploading consumes the webinar's storage — participation, so it needs a sign-in.
    let r = upload(&http, &srv, &code, "notes.txt", b"x".to_vec()).await;
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn an_empty_file_is_refused() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let code = chat_webinar(&srv, &http).await;

    let r = upload(&http, &srv, &code, "notes.txt", Vec::new()).await;
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn a_body_with_no_file_field_is_refused() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let code = chat_webinar(&srv, &http).await;

    let r = http
        .post(format!("{}/api/w/{code}/files", base(&srv)))
        .multipart(Form::new().text("caption", "hello"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn a_file_over_the_configured_limit_is_refused() {
    let srv = skip_without_db!(setup_with(true, 64).await);
    let http = Client::new();
    let code = chat_webinar(&srv, &http).await;

    let r = upload(&http, &srv, &code, "big.txt", vec![b'x'; 65]).await;
    assert_eq!(r.status(), 413);
    assert!(
        srv.storage.uploads.lock().unwrap().is_empty(),
        "an oversized file must never reach the bucket"
    );
}

#[tokio::test]
async fn a_file_exactly_at_the_limit_is_accepted() {
    let srv = skip_without_db!(setup_with(true, 64).await);
    let http = Client::new();
    let code = chat_webinar(&srv, &http).await;

    let r = upload(&http, &srv, &code, "exact.txt", vec![b'x'; 64]).await;
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn an_executable_is_refused_by_type() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let code = chat_webinar(&srv, &http).await;

    for name in ["payload.exe", "script.sh", "lib.so"] {
        let r = upload(&http, &srv, &code, name, b"MZ".to_vec()).await;
        assert_eq!(r.status(), 415, "{name} must not be storable");
    }
    assert!(srv.storage.uploads.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_file_with_no_extension_is_refused() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let code = chat_webinar(&srv, &http).await;

    let r = upload(&http, &srv, &code, "README", b"x".to_vec()).await;
    assert_eq!(r.status(), 415);
}

// ---------------------------------------------------------------------------
// Storage failures
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_failed_upload_is_reported_as_a_bad_gateway() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let code = chat_webinar(&srv, &http).await;
    srv.storage.upload_fails.store(true, Ordering::SeqCst);

    let r = upload(&http, &srv, &code, "notes.txt", b"x".to_vec()).await;
    assert_eq!(r.status(), 502, "the bucket failed, not the caller");
}

#[tokio::test]
async fn a_failed_signature_is_reported_as_a_bad_gateway() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let code = chat_webinar(&srv, &http).await;
    srv.storage.sign_fails.store(true, Ordering::SeqCst);

    let r = upload(&http, &srv, &code, "notes.txt", b"x".to_vec()).await;
    assert_eq!(r.status(), 502);
    assert_eq!(
        srv.storage.uploads.lock().unwrap().len(),
        1,
        "the object was stored; only the link could not be minted"
    );
}

#[tokio::test]
async fn a_signature_response_with_no_url_is_not_passed_off_as_success() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let code = chat_webinar(&srv, &http).await;
    srv.storage
        .sign_returns_nothing
        .store(true, Ordering::SeqCst);

    let r = upload(&http, &srv, &code, "notes.txt", b"x".to_vec()).await;
    assert_eq!(r.status(), 502);
}

// ---------------------------------------------------------------------------
// Rate limiting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_flood_of_uploads_from_one_caller_is_throttled() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let code = chat_webinar(&srv, &http).await;

    let mut throttled = false;
    for i in 0..15 {
        let r = upload(&http, &srv, &code, &format!("f{i}.txt"), b"x".to_vec()).await;
        if r.status() == 429 {
            throttled = true;
            break;
        }
    }
    assert!(
        throttled,
        "an unauthenticated endpoint that writes to a bucket has to have a ceiling"
    );
}
