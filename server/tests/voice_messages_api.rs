//! Project voice notes — `…/projects/{id}/voice-messages` (create, list, audio-url).
//!
//! `voice_messages_scope.rs` asserts the SQL contract these notes have to satisfy;
//! this drives the handlers themselves over HTTP. Supabase Storage is a local mock
//! so the artifact really is uploaded, deleted and signed without a bucket.
//!
//! Transcription is not mocked: `deepgram::transcribe_file` posts to a hard-coded
//! Deepgram URL, so with no key the transcript comes back empty — which is a real,
//! documented path (the note is still saved, just untranslated).
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
use voxtranslate_server::config::{Config, StorageConfig};
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::storage::SupabaseStorage;
use voxtranslate_server::{app, db, AppState};

const SECRET: &str = "voice-messages-secret";

// ---------------------------------------------------------------------------
// Mock Supabase Storage
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct MockStorage {
    uploaded: Arc<std::sync::Mutex<Vec<String>>>,
    deleted: Arc<std::sync::Mutex<Vec<String>>>,
    upload_fails: Arc<AtomicBool>,
    sign_fails: Arc<AtomicBool>,
}

async fn mock_upload(
    State(mock): State<MockStorage>,
    Path((_bucket, object)): Path<(String, String)>,
) -> axum::response::Response {
    if mock.upload_fails.load(Ordering::SeqCst) {
        return (StatusCode::INSUFFICIENT_STORAGE, "bucket full").into_response();
    }
    mock.uploaded.lock().unwrap().push(object);
    (StatusCode::OK, "{}").into_response()
}

async fn mock_delete(
    State(mock): State<MockStorage>,
    Path((_bucket, object)): Path<(String, String)>,
) -> axum::response::Response {
    mock.deleted.lock().unwrap().push(object);
    (StatusCode::OK, "{}").into_response()
}

async fn mock_sign(
    State(mock): State<MockStorage>,
    Path((bucket, object)): Path<(String, String)>,
) -> axum::response::Response {
    if mock.sign_fails.load(Ordering::SeqCst) {
        return (StatusCode::NOT_FOUND, "no such object").into_response();
    }
    axum::Json(json!({ "signedURL": format!("/object/sign/{bucket}/{object}?token=mock") }))
        .into_response()
}

async fn mock_storage() -> (String, MockStorage) {
    let mock = MockStorage::default();
    let router = Router::new()
        .route("/storage/v1/object/sign/{bucket}/{*object}", post(mock_sign))
        .route(
            "/storage/v1/object/{bucket}/{*object}",
            post(mock_upload).delete(mock_delete),
        )
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

async fn setup_with(storage: bool, max_bytes: usize) -> Option<Server> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;

    let (base_url, mock) = mock_storage().await;
    let mut config = Config::test_with_billing(&url, SECRET, 0.0);
    if storage {
        config.storage = Some(StorageConfig {
            supabase_url: base_url,
            service_key: "service-key".into(),
            bucket: "chat-files".into(),
            max_bytes,
            signed_ttl_secs: 3600,
        });
    }
    let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);
    let storage_client = config
        .storage
        .as_ref()
        .map(|c| SupabaseStorage::new(reqwest::Client::new(), c));
    let mut state = AppState::new(config);
    state.billing = Some(BillingService::new(pool.clone(), min_join));
    state.safety = Some(SafetyService::new(pool.clone()));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);
    state.storage = storage_client;

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

/// An org whose subscription is live unless `subscribed` says otherwise.
async fn org(srv: &Server, owner: Uuid, subscribed: bool) -> Uuid {
    let (status, period) = if subscribed {
        ("active", Some(chrono::Utc::now() + chrono::Duration::days(30)))
    } else {
        ("none", None)
    };
    let org_id: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id, subscription_status, current_period_end,
                                    credits_balance)
         VALUES ($1, $2, $3, $4, $5, 10000) RETURNING id",
    )
    .bind("Voice Co")
    .bind(format!("vm-{}", Uuid::new_v4().simple()))
    .bind(owner)
    .bind(status)
    .bind(period)
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

async fn project(srv: &Server, org_id: Uuid, owner: Uuid, languages: &[&str]) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO projects (org_id, name, created_by, default_languages)
         VALUES ($1, 'Nord', $2, $3) RETURNING id",
    )
    .bind(org_id)
    .bind(owner)
    .bind(languages.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    .fetch_one(&srv.pool)
    .await
    .unwrap()
}

fn vm_url(srv: &Server, org_id: Uuid, project_id: Uuid) -> String {
    format!(
        "{}/api/business/organizations/{org_id}/projects/{project_id}/voice-messages",
        base(srv)
    )
}

/// The clip itself, grouped so the call site stays readable (the same reason
/// `voice_messages::PersistArgs` exists).
struct Clip<'a> {
    file_name: &'a str,
    bytes: Vec<u8>,
    duration: Option<i32>,
}

/// A short webm clip with no stated duration — what most of these tests want.
fn clip(file_name: &str) -> Clip<'_> {
    Clip {
        file_name,
        bytes: b"b".to_vec(),
        duration: None,
    }
}

/// POST one voice note.
async fn post_note(
    http: &Client,
    srv: &Server,
    jwt: &str,
    org_id: Uuid,
    project_id: Uuid,
    clip: Clip<'_>,
) -> reqwest::Response {
    let mut form = Form::new().part(
        "file",
        Part::bytes(clip.bytes).file_name(clip.file_name.to_string()),
    );
    if let Some(d) = clip.duration {
        form = form.text("duration_seconds", d.to_string());
    }
    http.post(vm_url(srv, org_id, project_id))
        .bearer_auth(jwt)
        .multipart(form)
        .send()
        .await
        .unwrap()
}

/// The common case: a subscribed org, a project, and a member who can post.
async fn ready(srv: &Server) -> (Uuid, Uuid, String) {
    let (owner, jwt) = user(srv, "Recorder").await;
    let org_id = org(srv, owner, true).await;
    let project_id = project(srv, org_id, owner, &["en", "it"]).await;
    (org_id, project_id, jwt)
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

// ---------------------------------------------------------------------------
// Creating a note
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_note_is_saved_and_answers_with_what_the_client_needs_to_render_it() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, project_id, jwt) = ready(&srv).await;

    let r = post_note(
        &http,
        &srv,
        &jwt,
        org_id,
        project_id,
        Clip { file_name: "note.webm", bytes: b"fake-opus-bytes".to_vec(), duration: Some(12) },
    )
    .await;

    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["id"].as_str().is_some());
    assert!(body["session_id"].as_str().is_some());
    assert_eq!(body["project_id"], project_id.to_string());
    assert_eq!(body["file_name"], "note.webm");
    assert_eq!(body["content_type"], "audio/webm");
    assert_eq!(body["size"], 15);
    assert_eq!(body["duration_seconds"], 12);
    // No Deepgram key here, so the clip is stored untranslated rather than lost.
    assert_eq!(body["translated"], false);
    assert_eq!(body["word_count"], 0);
}

#[tokio::test]
async fn the_audio_is_uploaded_under_the_project_before_anything_is_charged() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, project_id, jwt) = ready(&srv).await;

    post_note(
        &http,
        &srv,
        &jwt,
        org_id,
        project_id,
        Clip { file_name: "note.webm", bytes: b"bytes".to_vec(), duration: None },
    )
    .await;

    let uploaded = srv.storage.uploaded.lock().unwrap().clone();
    assert_eq!(uploaded.len(), 1);
    assert!(uploaded[0].starts_with(&format!("{project_id}/")));
    assert!(uploaded[0].ends_with(".webm"));
}

#[tokio::test]
async fn a_note_materialises_the_rows_search_and_insights_already_consume() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, project_id, jwt) = ready(&srv).await;

    let body: Value = post_note(
        &http,
        &srv,
        &jwt,
        org_id,
        project_id,
        Clip { file_name: "note.m4a", bytes: b"bytes".to_vec(), duration: Some(5) },
    )
    .await
    .json()
    .await
    .unwrap();
    let session_id = Uuid::parse_str(body["session_id"].as_str().unwrap()).unwrap();

    // Tagged so the COLD analytics aggregations exclude it from call KPIs …
    let kind: String = sqlx::query_scalar("SELECT kind FROM call_sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert_eq!(kind, "voice_message");

    // … while the uploader's participant row is what grants them search scope.
    let participants: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM session_participants WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(participants, 1);

    // Zero minutes even if a `kind` filter is ever missed.
    let zero_length: bool = sqlx::query_scalar(
        "SELECT started_at = ended_at FROM call_sessions WHERE id = $1",
    )
    .bind(session_id)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert!(zero_length);
}

#[tokio::test]
async fn a_note_with_no_stated_duration_is_still_accepted() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, project_id, jwt) = ready(&srv).await;

    let r = post_note(
        &http,
        &srv,
        &jwt,
        org_id,
        project_id,
        Clip { file_name: "note.ogg", bytes: b"bytes".to_vec(), duration: None },
    )
    .await;
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["duration_seconds"].is_null());
}

#[tokio::test]
async fn a_negative_duration_is_discarded_rather_than_stored() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, project_id, jwt) = ready(&srv).await;

    let form = Form::new()
        .part("file", Part::bytes(b"b".to_vec()).file_name("n.webm"))
        .text("duration_seconds", "-30");
    let r = http
        .post(vm_url(&srv, org_id, project_id))
        .bearer_auth(&jwt)
        .multipart(form)
        .send()
        .await
        .unwrap();

    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["duration_seconds"].is_null());
}

#[tokio::test]
async fn an_unparseable_duration_is_discarded_rather_than_failing_the_upload() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, project_id, jwt) = ready(&srv).await;

    let form = Form::new()
        .part("file", Part::bytes(b"b".to_vec()).file_name("n.webm"))
        .text("duration_seconds", "about a minute");
    let r = http
        .post(vm_url(&srv, org_id, project_id))
        .bearer_auth(&jwt)
        .multipart(form)
        .send()
        .await
        .unwrap();

    assert_eq!(r.status(), 200);
    assert!(r.json::<Value>().await.unwrap()["duration_seconds"].is_null());
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_anonymous_caller_is_refused() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, project_id, _) = ready(&srv).await;

    let r = http
        .post(vm_url(&srv, org_id, project_id))
        .multipart(Form::new().part("file", Part::bytes(b"b".to_vec()).file_name("n.webm")))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn someone_outside_the_org_is_refused() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, project_id, _) = ready(&srv).await;
    let (_, outsider) = user(&srv, "Outsider").await;

    // 404, not 403: an outsider is not told the organization exists.
    let r = post_note(&http, &srv, &outsider, org_id, project_id, clip("n.webm"))
    .await;
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn an_org_with_no_live_subscription_is_refused() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Recorder").await;
    let org_id = org(&srv, owner, false).await;
    let project_id = project(&srv, org_id, owner, &["en"]).await;

    // The same gate as cloud recording: a gifted month stops unlocking this the
    // moment it ends.
    let r = post_note(&http, &srv, &jwt, org_id, project_id, clip("n.webm"))
    .await;
    assert_eq!(r.status(), 403);
}

#[tokio::test]
async fn a_project_belonging_to_another_org_is_refused() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, _, jwt) = ready(&srv).await;
    let (other_owner, _) = user(&srv, "Other").await;
    let other_org = org(&srv, other_owner, true).await;
    let other_project = project(&srv, other_org, other_owner, &["en"]).await;

    let r = post_note(&http, &srv, &jwt, org_id, other_project, clip("n.webm"))
    .await;
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn an_archived_project_takes_no_new_notes() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, project_id, jwt) = ready(&srv).await;
    sqlx::query("UPDATE projects SET archived_at = now() WHERE id = $1")
        .bind(project_id)
        .execute(&srv.pool)
        .await
        .unwrap();

    let r = post_note(&http, &srv, &jwt, org_id, project_id, clip("n.webm"))
    .await;
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn without_a_bucket_the_endpoint_says_so() {
    let srv = skip_without_db!(setup_with(false, 0).await);
    let http = Client::new();
    let (org_id, project_id, jwt) = ready(&srv).await;

    let r = post_note(&http, &srv, &jwt, org_id, project_id, clip("n.webm"))
    .await;
    assert_eq!(r.status(), 503);
}

#[tokio::test]
async fn a_body_with_no_file_is_refused() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, project_id, jwt) = ready(&srv).await;

    let r = http
        .post(vm_url(&srv, org_id, project_id))
        .bearer_auth(&jwt)
        .multipart(Form::new().text("duration_seconds", "10"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn an_empty_clip_is_refused() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, project_id, jwt) = ready(&srv).await;

    let r = post_note(
        &http,
        &srv,
        &jwt,
        org_id,
        project_id,
        Clip { file_name: "n.webm", bytes: Vec::new(), duration: None },
    )
    .await;
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn a_clip_over_the_limit_is_refused_before_it_reaches_the_bucket() {
    let srv = skip_without_db!(setup_with(true, 32).await);
    let http = Client::new();
    let (org_id, project_id, jwt) = ready(&srv).await;

    let r = post_note(
        &http,
        &srv,
        &jwt,
        org_id,
        project_id,
        Clip { file_name: "n.webm", bytes: vec![b'x'; 33], duration: None },
    )
    .await;
    assert_eq!(r.status(), 413);
    assert!(srv.storage.uploaded.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_document_is_not_a_voice_message() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, project_id, jwt) = ready(&srv).await;

    for name in ["notes.txt", "deck.pdf", "shot.png"] {
        let r = post_note(
        &http,
        &srv,
        &jwt,
        org_id,
        project_id,
        Clip { file_name: name, bytes: b"bytes".to_vec(), duration: None },
    )
        .await;
        assert_eq!(r.status(), 415, "{name} is not audio");
    }
}

#[tokio::test]
async fn a_storage_failure_is_a_bad_gateway_and_saves_nothing() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, project_id, jwt) = ready(&srv).await;
    srv.storage.upload_fails.store(true, Ordering::SeqCst);

    let r = post_note(&http, &srv, &jwt, org_id, project_id, clip("n.webm"))
    .await;
    assert_eq!(r.status(), 502);

    let saved: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM project_voice_messages WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(saved, 0, "a failed upload must leave no row behind");
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn notes_are_listed_newest_first() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, project_id, jwt) = ready(&srv).await;

    for name in ["first.webm", "second.webm", "third.webm"] {
        post_note(&http, &srv, &jwt, org_id, project_id, clip(name))
        .await;
    }

    let body: Value = http
        .get(vm_url(&srv, org_id, project_id))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let names: Vec<&str> = body["voice_messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["file_name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["third.webm", "second.webm", "first.webm"]);
}

#[tokio::test]
async fn the_list_carries_the_metadata_a_row_renders_but_no_audio_url() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, project_id, jwt) = ready(&srv).await;
    post_note(
        &http,
        &srv,
        &jwt,
        org_id,
        project_id,
        Clip { file_name: "n.webm", bytes: b"bytes".to_vec(), duration: Some(7) },
    )
    .await;

    let body: Value = http
        .get(vm_url(&srv, org_id, project_id))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = &body["voice_messages"][0];

    assert_eq!(row["created_by_name"], "Recorder");
    assert_eq!(row["content_type"], "audio/webm");
    assert_eq!(row["size_bytes"], 5);
    assert_eq!(row["duration_seconds"], 7);
    assert_eq!(row["translated"], false);
    // Signed URLs expire, so they are minted on demand rather than embedded here.
    assert!(row.get("url").is_none());
}

#[tokio::test]
async fn the_list_pages_and_clamps_its_page_size() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, project_id, jwt) = ready(&srv).await;
    for i in 0..3 {
        post_note(&http, &srv, &jwt, org_id, project_id, clip(&format!("n{i}.webm")))
        .await;
    }

    let page1: Value = http
        .get(format!("{}?limit=2&page=1", vm_url(&srv, org_id, project_id)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page1["voice_messages"].as_array().unwrap().len(), 2);
    assert_eq!(page1["limit"], 2);

    let page2: Value = http
        .get(format!("{}?limit=2&page=2", vm_url(&srv, org_id, project_id)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page2["voice_messages"].as_array().unwrap().len(), 1);
    assert_eq!(page2["page"], 2);

    // Absurd inputs are clamped rather than trusted.
    let clamped: Value = http
        .get(format!(
            "{}?limit=9999&page=0",
            vm_url(&srv, org_id, project_id)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(clamped["limit"], 100);
    assert_eq!(clamped["page"], 1);
}

#[tokio::test]
async fn another_orgs_notes_are_not_listed() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, project_id, jwt) = ready(&srv).await;
    post_note(&http, &srv, &jwt, org_id, project_id, clip("mine.webm"))
    .await;

    let (other_owner, other_jwt) = user(&srv, "Other").await;
    let other_org = org(&srv, other_owner, true).await;

    let r = http
        .get(format!(
            "{}/api/business/organizations/{other_org}/projects/{project_id}/voice-messages",
            base(&srv)
        ))
        .bearer_auth(&other_jwt)
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    assert!(body["voice_messages"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn listing_needs_membership() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, project_id, _) = ready(&srv).await;
    let (_, outsider) = user(&srv, "Outsider").await;

    let r = http
        .get(vm_url(&srv, org_id, project_id))
        .bearer_auth(&outsider)
        .send()
        .await
        .unwrap();
    // 404 rather than 403 — the same "we do not confirm this org exists" rule.
    assert_eq!(r.status(), 404);
}

// ---------------------------------------------------------------------------
// Playback URL
// ---------------------------------------------------------------------------

fn audio_url(srv: &Server, org_id: Uuid, project_id: Uuid, id: &str) -> String {
    format!("{}/{id}/audio-url", vm_url(srv, org_id, project_id))
}

#[tokio::test]
async fn playback_mints_a_signed_url_on_demand() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, project_id, jwt) = ready(&srv).await;
    let created: Value = post_note(&http, &srv, &jwt, org_id, project_id, clip("n.webm"))
    .await
    .json()
    .await
    .unwrap();
    let id = created["id"].as_str().unwrap();

    let r = http
        .get(audio_url(&srv, org_id, project_id, id))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();

    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["url"].as_str().unwrap().contains("token=mock"));
}

#[tokio::test]
async fn playback_of_an_unknown_note_is_a_404() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, project_id, jwt) = ready(&srv).await;

    let r = http
        .get(audio_url(
            &srv,
            org_id,
            project_id,
            &Uuid::new_v4().to_string(),
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn a_note_cannot_be_played_through_another_projects_path() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, project_id, jwt) = ready(&srv).await;
    let (owner, _) = user(&srv, "Second").await;
    let other_project = project(&srv, org_id, owner, &["en"]).await;
    let created: Value = post_note(&http, &srv, &jwt, org_id, project_id, clip("n.webm"))
    .await
    .json()
    .await
    .unwrap();

    let r = http
        .get(audio_url(
            &srv,
            org_id,
            other_project,
            created["id"].as_str().unwrap(),
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn a_failed_signature_is_a_bad_gateway() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, project_id, jwt) = ready(&srv).await;
    let created: Value = post_note(&http, &srv, &jwt, org_id, project_id, clip("n.webm"))
    .await
    .json()
    .await
    .unwrap();
    srv.storage.sign_fails.store(true, Ordering::SeqCst);

    let r = http
        .get(audio_url(
            &srv,
            org_id,
            project_id,
            created["id"].as_str().unwrap(),
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 502);
}

#[tokio::test]
async fn playback_without_a_bucket_says_so() {
    let srv = skip_without_db!(setup_with(false, 0).await);
    let http = Client::new();
    let (org_id, project_id, jwt) = ready(&srv).await;

    let r = http
        .get(audio_url(
            &srv,
            org_id,
            project_id,
            &Uuid::new_v4().to_string(),
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
}

#[tokio::test]
async fn playback_needs_membership() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (org_id, project_id, _) = ready(&srv).await;
    let (_, outsider) = user(&srv, "Outsider").await;

    let r = http
        .get(audio_url(
            &srv,
            org_id,
            project_id,
            &Uuid::new_v4().to_string(),
        ))
        .bearer_auth(&outsider)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}
