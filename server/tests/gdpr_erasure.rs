//! GDPR Art. 17 — erasure must remove the user's Supabase Storage objects, not
//! just their database rows (migration 053).
//!
//! `SafetyService::delete_user` used to be `DELETE FROM users` relying on FK
//! cascade. It never called `storage.delete()`, so every file a user had uploaded
//! survived their account. These tests pin the fixed contract:
//!
//!   1. the objects are gone from storage after erasure;
//!   2. a storage failure aborts the whole erasure — the account is NOT deleted,
//!      so a retry can complete it (storage-first ordering, mirroring the
//!      retention sweep in `business::retention`);
//!   3. one user's erasure never touches another user's objects;
//!   4. org-owned artifacts (call recordings) are deliberately NOT erased.
//!
//! Storage is checked through the real `SupabaseStorage` HTTP client pointed at a
//! local fake of the Supabase Storage REST API, so the actual delete request is
//! exercised — not a stubbed trait. DB-gated: skipped without `DATABASE_URL`.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::delete;
use axum::Router;
use uuid::Uuid;
use voxtranslate_server::config::StorageConfig;
use voxtranslate_server::db;
use voxtranslate_server::safety::{EraseError, SafetyService};
use voxtranslate_server::storage::SupabaseStorage;

const BUCKET: &str = "chat-files";

// ---------------------------------------------------------------------------
// A minimal fake of the Supabase Storage REST API.
//
// Only the one verb erasure uses is implemented: DELETE
// /storage/v1/object/{bucket}/{*path}. `objects` is the ground truth the tests
// assert against — "does the store still hold these bytes?" — which is what the
// real bucket would answer.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct FakeStore {
    objects: Arc<Mutex<HashSet<String>>>,
    fail_deletes: Arc<AtomicBool>,
}

impl FakeStore {
    fn new(initial: &[&str]) -> Self {
        Self {
            objects: Arc::new(Mutex::new(
                initial.iter().map(|s| (*s).to_string()).collect(),
            )),
            fail_deletes: Arc::new(AtomicBool::new(false)),
        }
    }

    fn holds(&self, path: &str) -> bool {
        self.objects.lock().unwrap().contains(path)
    }

    fn len(&self) -> usize {
        self.objects.lock().unwrap().len()
    }
}

async fn handle_delete(
    State(store): State<FakeStore>,
    Path((bucket, object_path)): Path<(String, String)>,
) -> StatusCode {
    if store.fail_deletes.load(Ordering::SeqCst) {
        return StatusCode::INTERNAL_SERVER_ERROR;
    }
    assert_eq!(bucket, BUCKET, "erasure hit an unexpected bucket");
    // Supabase answers 404 for an object that is already gone, and
    // `SupabaseStorage::delete` treats that as success so a retry is idempotent.
    if store.objects.lock().unwrap().remove(&object_path) {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

/// Start the fake on an ephemeral port and return its base URL.
async fn spawn_fake_storage(store: FakeStore) -> String {
    let app = Router::new()
        .route(
            "/storage/v1/object/{bucket}/{*object_path}",
            delete(handle_delete),
        )
        .with_state(store);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn storage_client(base_url: String) -> SupabaseStorage {
    SupabaseStorage::new(
        reqwest::Client::new(),
        &StorageConfig {
            supabase_url: base_url,
            service_key: "test-service-key".into(),
            bucket: BUCKET.into(),
            max_bytes: 1024,
            signed_ttl_secs: 60,
        },
    )
}

// ---------------------------------------------------------------------------
// DB fixtures
// ---------------------------------------------------------------------------

async fn pool_or_skip() -> Option<db::Pool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    Some(pool)
}

async fn mk_user(pool: &db::Pool, tag: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, google_id, email, name) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(format!("g-{id}"))
        .bind(format!("{tag}-{id}@example.test"))
        .bind(tag)
        .execute(pool)
        .await
        .expect("insert user");
    id
}

async fn mk_session(pool: &db::Pool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO call_sessions (id, room) VALUES ($1, $2)")
        .bind(id)
        .bind(format!("room-{id}"))
        .execute(pool)
        .await
        .expect("insert call_session");
    id
}

/// A chat file uploaded by `user`, recorded with the durable object path that
/// migration 053 added.
async fn mk_chat_file(pool: &db::Pool, session: Uuid, user: Uuid, object_path: &str) {
    sqlx::query(
        "INSERT INTO chat_files
            (session_id, room, sender_peer_id, sender_name, file_url, file_name,
             file_type, size_bytes, user_id, object_path)
         VALUES ($1, 'room', 'peer-1', 'Sender', 'https://example.test/signed',
                 'notes.txt', 'text/plain', 12, $2, $3)",
    )
    .bind(session)
    .bind(user)
    .bind(object_path)
    .execute(pool)
    .await
    .expect("insert chat_file");
}

async fn user_exists(pool: &db::Pool, user: Uuid) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM users WHERE id = $1")
        .bind(user)
        .fetch_one(pool)
        .await
        .expect("count users")
        > 0
}

async fn chat_file_count(pool: &db::Pool, user: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM chat_files WHERE user_id = $1")
        .bind(user)
        .fetch_one(pool)
        .await
        .expect("count chat_files")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The headline contract: after erasure the bucket holds none of the user's
/// objects, and the account and its rows are gone.
#[tokio::test]
async fn erasure_deletes_the_users_storage_objects() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };

    let user = mk_user(&pool, "erasable").await;
    let session = mk_session(&pool).await;
    let a = format!("{session}/{}.txt", Uuid::new_v4());
    let b = format!("{session}/{}.txt", Uuid::new_v4());
    mk_chat_file(&pool, session, user, &a).await;
    mk_chat_file(&pool, session, user, &b).await;

    let store = FakeStore::new(&[&a, &b]);
    let base = spawn_fake_storage(store.clone()).await;
    let svc = SafetyService::new(pool.clone()).with_files_storage(Some(storage_client(base)));

    svc.delete_user(user).await.expect("erasure should succeed");

    assert!(
        !store.holds(&a),
        "object {a} still in storage after erasure"
    );
    assert!(
        !store.holds(&b),
        "object {b} still in storage after erasure"
    );
    assert_eq!(store.len(), 0, "storage should hold nothing for this user");
    assert!(!user_exists(&pool, user).await, "user row survived erasure");
    assert_eq!(chat_file_count(&pool, user).await, 0, "chat_files survived");
}

/// Failure path. If storage cannot be cleared the erasure must fail loudly and
/// leave the account intact — never a half-erased state where the row is gone but
/// the bytes remain, which would also destroy the only handle on those bytes.
#[tokio::test]
async fn storage_failure_aborts_erasure_and_keeps_the_user() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };

    let user = mk_user(&pool, "storage-fail").await;
    let session = mk_session(&pool).await;
    let obj = format!("{session}/{}.txt", Uuid::new_v4());
    mk_chat_file(&pool, session, user, &obj).await;

    let store = FakeStore::new(&[&obj]);
    store.fail_deletes.store(true, Ordering::SeqCst);
    let base = spawn_fake_storage(store.clone()).await;
    let svc = SafetyService::new(pool.clone()).with_files_storage(Some(storage_client(base)));

    let err = svc
        .delete_user(user)
        .await
        .expect_err("erasure must fail when storage delete fails");
    assert!(
        matches!(err, EraseError::Storage(_)),
        "expected a storage error, got {err:?}"
    );

    assert!(store.holds(&obj), "object should be untouched");
    assert!(
        user_exists(&pool, user).await,
        "user must NOT be deleted when storage failed"
    );
    assert_eq!(
        chat_file_count(&pool, user).await,
        1,
        "chat_files row must survive a failed erasure"
    );

    // The retry path: once storage recovers, the same call completes. A 404 on an
    // already-deleted object counts as success, so this is safe to repeat.
    store.fail_deletes.store(false, Ordering::SeqCst);
    svc.delete_user(user).await.expect("retry should succeed");
    assert!(!store.holds(&obj), "retry should have cleared the object");
    assert!(
        !user_exists(&pool, user).await,
        "retry should erase the user"
    );
}

/// Erasing one account must not reach another account's objects.
#[tokio::test]
async fn erasure_does_not_touch_another_users_objects() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };

    let victim = mk_user(&pool, "victim").await;
    let bystander = mk_user(&pool, "bystander").await;
    let session = mk_session(&pool).await;
    let mine = format!("{session}/{}.txt", Uuid::new_v4());
    let theirs = format!("{session}/{}.txt", Uuid::new_v4());
    mk_chat_file(&pool, session, victim, &mine).await;
    mk_chat_file(&pool, session, bystander, &theirs).await;

    let store = FakeStore::new(&[&mine, &theirs]);
    let base = spawn_fake_storage(store.clone()).await;
    let svc = SafetyService::new(pool.clone()).with_files_storage(Some(storage_client(base)));

    svc.delete_user(victim).await.expect("erasure");

    assert!(
        !store.holds(&mine),
        "the erased user's object should be gone"
    );
    assert!(store.holds(&theirs), "a bystander's object was deleted");
    assert!(user_exists(&pool, bystander).await, "bystander was deleted");
    assert_eq!(chat_file_count(&pool, bystander).await, 1);
}

/// Rows written before migration 053 carry a NULL `object_path`. They are
/// unattributable, must not crash erasure, and must not produce a delete request
/// for an empty path (which would be a request against the bucket root).
#[tokio::test]
async fn erasure_skips_rows_with_no_recorded_object_path() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };

    let user = mk_user(&pool, "legacy").await;
    let session = mk_session(&pool).await;
    sqlx::query(
        "INSERT INTO chat_files
            (session_id, room, sender_peer_id, sender_name, file_url, file_name,
             file_type, size_bytes, user_id, object_path)
         VALUES ($1, 'room', 'peer-1', 'Sender', 'https://example.test/signed',
                 'legacy.txt', 'text/plain', 12, $2, NULL)",
    )
    .bind(session)
    .bind(user)
    .execute(&pool)
    .await
    .expect("insert legacy chat_file");

    let orphan = "pre-053/legacy.txt";
    let store = FakeStore::new(&[orphan]);
    let base = spawn_fake_storage(store.clone()).await;
    let svc = SafetyService::new(pool.clone()).with_files_storage(Some(storage_client(base)));

    svc.delete_user(user).await.expect("erasure");

    assert!(!user_exists(&pool, user).await, "user should be erased");
    assert!(
        store.holds(orphan),
        "a pre-053 object is unattributable and must be left alone, not guessed at"
    );
}

/// With no storage configured and nothing to delete, erasure behaves exactly as
/// it did before this change. Guards the consumer path on a deploy without
/// SUPABASE_* set.
#[tokio::test]
async fn erasure_without_storage_configured_still_works() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };

    let user = mk_user(&pool, "no-storage").await;
    let svc = SafetyService::new(pool.clone());
    svc.delete_user(user).await.expect("erasure");
    assert!(!user_exists(&pool, user).await);
}

/// Refusing to guess is part of the contract: if the user HAS objects but storage
/// is not configured, erasure must fail rather than delete the rows and strand
/// the bytes with no pointer left to find them.
#[tokio::test]
async fn erasure_refuses_when_objects_exist_but_storage_is_unconfigured() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };

    let user = mk_user(&pool, "unconfigured").await;
    let session = mk_session(&pool).await;
    mk_chat_file(&pool, session, user, "some/object.txt").await;

    let svc = SafetyService::new(pool.clone());
    let err = svc
        .delete_user(user)
        .await
        .expect_err("must refuse without a storage client");
    assert!(
        matches!(err, EraseError::StorageUnavailable),
        "expected StorageUnavailable, got {err:?}"
    );
    assert!(
        user_exists(&pool, user).await,
        "user must survive the refusal"
    );
}

/// Deliberate scope boundary (migration 016: "Org-owned: keep the project if the
/// creator's personal account is deleted"). A cloud recording is a multi-party,
/// org-owned artifact: an individual's erasure must NOT destroy it. It stays
/// governed by the Enterprise retention sweep.
#[tokio::test]
async fn erasure_leaves_org_owned_call_recordings_alone() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };

    let user = mk_user(&pool, "participant").await;
    let session = mk_session(&pool).await;
    let recording = format!("org/{session}/recording.webm");
    sqlx::query("UPDATE call_sessions SET recording_storage_path = $2 WHERE id = $1")
        .bind(session)
        .bind(&recording)
        .execute(&pool)
        .await
        .expect("set recording path");
    sqlx::query(
        "INSERT INTO session_participants (session_id, peer_id, user_id, name, lang)
         VALUES ($1, 'peer-1', $2, 'Participant', 'en')",
    )
    .bind(session)
    .bind(user)
    .execute(&pool)
    .await
    .expect("insert participant");

    let store = FakeStore::new(&[&recording]);
    let base = spawn_fake_storage(store.clone()).await;
    let svc = SafetyService::new(pool.clone()).with_files_storage(Some(storage_client(base)));

    svc.delete_user(user).await.expect("erasure");

    assert!(!user_exists(&pool, user).await, "user should be erased");
    assert!(
        store.holds(&recording),
        "a multi-party org-owned recording must survive an individual's erasure"
    );
}
