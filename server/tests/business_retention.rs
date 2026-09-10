//! Integration test for the Enterprise data-retention sweep (spec 0106).
//!
//! DB-gated like the other business tests: no-ops without `DATABASE_URL`. Drives
//! `business::retention::sweep_once` directly (no HTTP server needed) against a
//! transcript-only session, so no recordings storage is required.

use rust_decimal::Decimal;
use uuid::Uuid;
use voxtranslate_server::auth::{upsert_google_user, GoogleIdentity};
use voxtranslate_server::business::retention::{sweep_once, sweep_voip_recordings_once};
use voxtranslate_server::config::Config;
use voxtranslate_server::telephony::mock::MockTelephonyProvider;
use voxtranslate_server::telephony::ProviderError;
use voxtranslate_server::{db, AppState};

const SECRET: &str = "retention-secret";

async fn pool_or_skip() -> Option<db::Pool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    Some(pool)
}

async fn make_user(pool: &db::Pool) -> Uuid {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: "Retention Owner".into(),
        avatar_url: None,
    };
    upsert_google_user(pool, &identity, Decimal::ZERO, None, None)
        .await
        .unwrap()
        .0
        .id
}

/// Insert an Enterprise org with the given retention window + compliance flag.
async fn make_enterprise_org(pool: &db::Pool, owner: Uuid, retention_days: i32) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, plan, owner_id, settings)
         VALUES ($1, 'Acme', $2, 'enterprise', $3,
                 jsonb_build_object('retention_days', $4::int, 'compliance_mode', true))",
    )
    .bind(id)
    .bind(format!("acme-{id}"))
    .bind(owner)
    .bind(retention_days)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// A finished call `age_days` old with a transcript (no recording).
async fn make_session_with_transcript(pool: &db::Pool, org: Uuid, age_days: i64) -> Uuid {
    let sid = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO call_sessions (id, room, started_at, ended_at, org_id, transcript_status)
         VALUES ($1, 'room', now() - make_interval(days => $2::int),
                 now() - make_interval(days => $2::int), $3, 'ready')",
    )
    .bind(sid)
    .bind(age_days as i32)
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO transcripts (session_id, org_id, source_language, segments)
         VALUES ($1, $2, 'en', '[{\"text\":\"hello\"}]'::jsonb)",
    )
    .bind(sid)
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
    sid
}

async fn transcript_count(pool: &db::Pool, sid: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM transcripts WHERE session_id = $1")
        .bind(sid)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn status_of(pool: &db::Pool, sid: Uuid) -> String {
    sqlx::query_scalar("SELECT transcript_status FROM call_sessions WHERE id = $1")
        .bind(sid)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn retention_sweep_purges_only_expired_enterprise_transcripts() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: no DATABASE_URL");
        return;
    };

    let owner = make_user(&pool).await;
    let org = make_enterprise_org(&pool, owner, 30).await;
    // One call well past the 30-day window, one comfortably inside it.
    let expired = make_session_with_transcript(&pool, org, 60).await;
    let fresh = make_session_with_transcript(&pool, org, 5).await;

    // No recordings storage configured → transcript-only purge still works.
    let mut state = AppState::new(Config::test_with_billing(
        &std::env::var("DATABASE_URL").unwrap(),
        SECRET,
        0.0,
    ));
    state.pool = Some(pool.clone());

    let purged = sweep_once(&state, 200).await.unwrap();
    assert!(purged >= 1, "expected at least the expired session purged");

    // Expired: transcript gone, session marked expired.
    assert_eq!(transcript_count(&pool, expired).await, 0);
    assert_eq!(status_of(&pool, expired).await, "expired");

    // Fresh: untouched.
    assert_eq!(transcript_count(&pool, fresh).await, 1);
    assert_eq!(status_of(&pool, fresh).await, "ready");

    // Compliance-mode org → an audit row was written for the purge.
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_logs
         WHERE org_id = $1 AND action = 'retention.purge' AND resource_id = $2",
    )
    .bind(org)
    .bind(expired)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audited, 1, "expected one retention.purge audit row");

    // Idempotent: a second pass finds nothing new to purge for these sessions.
    let again = sweep_once(&state, 200).await.unwrap();
    assert_eq!(transcript_count(&pool, expired).await, 0);
    assert_eq!(status_of(&pool, fresh).await, "ready");
    let _ = again; // other tests' data may also be swept; only our rows are asserted
}

#[tokio::test]
async fn retention_sweep_is_noop_without_database() {
    // Guest mode (no pool) must never error or do work.
    let mut cfg = Config::test_with_billing("postgres://unused", SECRET, 0.0);
    cfg.billing = None;
    let state = AppState::new(cfg);
    assert_eq!(sweep_once(&state, 50).await.unwrap(), 0);
}

/// A finished phone call `age_days` old whose recording sits on the carrier's storage.
async fn make_phone_call_with_recording(
    pool: &db::Pool,
    org: Uuid,
    user: Uuid,
    age_days: i64,
    recording_id: &str,
) -> Uuid {
    let sid = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO call_sessions (id, room, started_at, ended_at, org_id, kind)
         VALUES ($1, $2, now() - make_interval(days => $3::int),
                 now() - make_interval(days => $3::int), $4, 'phone')",
    )
    .bind(sid)
    .bind(format!("ph-{sid}"))
    .bind(age_days as i32)
    .bind(org)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO voip_calls
            (session_id, org_id, user_id, provider, provider_region, direction,
             caller_e164, recipient_e164, recipient_pseudonym, recipient_country,
             source_language, target_language, engine_id, status,
             started_at, ended_at, recording_status,
             provider_recording_id, provider_recording_url)
         VALUES ($1, $2, $3, 'mock', 'test', 'outbound',
                 '+390211111111', '+393209999999', 'pseudo', 'IT',
                 'it', 'en', 'standard', 'completed',
                 now() - make_interval(days => $4::int),
                 now() - make_interval(days => $4::int), 'saved', $5, 'https://carrier/rec.mp3')
         RETURNING id",
    )
    .bind(sid)
    .bind(org)
    .bind(user)
    .bind(age_days as i32)
    .bind(recording_id)
    .fetch_one(pool)
    .await
    .map(|r: sqlx::postgres::PgRow| {
        use sqlx::Row;
        r.get::<Uuid, _>("id")
    })
    .unwrap()
}

async fn voip_settings(pool: &db::Pool, org: Uuid, retention_days: Option<i32>) {
    sqlx::query(
        "INSERT INTO voip_org_settings (org_id, enabled, recording_enabled, recording_retention_days)
         VALUES ($1, TRUE, TRUE, $2)
         ON CONFLICT (org_id) DO UPDATE SET recording_retention_days = EXCLUDED.recording_retention_days",
    )
    .bind(org)
    .bind(retention_days)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn expired_phone_recordings_are_deleted_at_the_carrier_and_forgotten_here() {
    // R21. A phone recording is NOT in our object storage — it is on the carrier's, and
    // `provider_recording_id` is the only durable handle on it. Two things must both be
    // true: the bytes are deleted at the provider, and only then is the handle cleared.
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: no DATABASE_URL");
        return;
    };
    let owner = make_user(&pool).await;
    let org = make_enterprise_org(&pool, owner, 30).await;
    voip_settings(&pool, org, Some(30)).await;

    let rec_expired = format!("rec-old-{}", Uuid::new_v4());
    let rec_fresh = format!("rec-new-{}", Uuid::new_v4());
    let expired = make_phone_call_with_recording(&pool, org, owner, 60, &rec_expired).await;
    let fresh = make_phone_call_with_recording(&pool, org, owner, 5, &rec_fresh).await;

    let provider = std::sync::Arc::new(MockTelephonyProvider::default());
    let mut state = AppState::new(Config::test_with_billing(
        &std::env::var("DATABASE_URL").unwrap(),
        SECRET,
        0.0,
    ));
    state.pool = Some(pool.clone());
    state.telephony = Some(provider.clone());

    let purged = sweep_voip_recordings_once(&state, 200).await.unwrap();
    assert!(purged >= 1, "expected the expired recording deleted");

    assert!(
        provider.recording_deleted(&rec_expired),
        "the carrier still holds a recording that is past its retention window"
    );
    assert!(
        !provider.recording_deleted(&rec_fresh),
        "a recording inside its window must not be touched"
    );

    let (status, handle): (String, Option<String>) = sqlx::query_as(
        "SELECT recording_status, provider_recording_id FROM voip_calls WHERE id = $1",
    )
    .bind(expired)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "deleted");
    assert_eq!(
        handle, None,
        "the pointer is cleared only after the bytes are gone"
    );

    let (fresh_status, fresh_handle): (String, Option<String>) = sqlx::query_as(
        "SELECT recording_status, provider_recording_id FROM voip_calls WHERE id = $1",
    )
    .bind(fresh)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(fresh_status, "saved");
    assert_eq!(fresh_handle.as_deref(), Some(rec_fresh.as_str()));
}

#[tokio::test]
async fn a_failed_carrier_delete_leaves_the_handle_for_the_next_pass() {
    // The ordering rule, asserted from the failing side. Clearing the handle after a failed
    // delete would strand the audio on the carrier's disk with nothing able to name it —
    // the exact permanent leak migration 053 was written to prevent for uploads.
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: no DATABASE_URL");
        return;
    };
    let owner = make_user(&pool).await;
    let org = make_enterprise_org(&pool, owner, 30).await;
    voip_settings(&pool, org, Some(30)).await;

    let rec = format!("rec-stuck-{}", Uuid::new_v4());
    let call = make_phone_call_with_recording(&pool, org, owner, 60, &rec).await;

    let provider = std::sync::Arc::new(MockTelephonyProvider::default());
    provider.fail_recording_deletes(ProviderError::Unavailable {
        detail: "carrier is down".into(),
    });

    let mut state = AppState::new(Config::test_with_billing(
        &std::env::var("DATABASE_URL").unwrap(),
        SECRET,
        0.0,
    ));
    state.pool = Some(pool.clone());
    state.telephony = Some(provider.clone());

    let purged = sweep_voip_recordings_once(&state, 200).await.unwrap();
    assert_eq!(purged, 0, "nothing was purged, because nothing was deleted");

    let (status, handle): (String, Option<String>) = sqlx::query_as(
        "SELECT recording_status, provider_recording_id FROM voip_calls WHERE id = $1",
    )
    .bind(call)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "saved", "the row must still say a recording exists");
    assert_eq!(
        handle.as_deref(),
        Some(rec.as_str()),
        "the handle must survive so the next pass can retry"
    );
}

#[tokio::test]
async fn without_a_configured_provider_nothing_is_touched() {
    // No provider means no way to reach the bytes. Doing nothing keeps the handles intact
    // for a deployment that has one, instead of marking recordings deleted that are not.
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: no DATABASE_URL");
        return;
    };
    let owner = make_user(&pool).await;
    let org = make_enterprise_org(&pool, owner, 30).await;
    voip_settings(&pool, org, Some(30)).await;
    let rec = format!("rec-orphan-{}", Uuid::new_v4());
    let call = make_phone_call_with_recording(&pool, org, owner, 60, &rec).await;

    let mut state = AppState::new(Config::test_with_billing(
        &std::env::var("DATABASE_URL").unwrap(),
        SECRET,
        0.0,
    ));
    state.pool = Some(pool.clone());
    state.telephony = None;

    assert_eq!(sweep_voip_recordings_once(&state, 200).await.unwrap(), 0);

    let handle: Option<String> =
        sqlx::query_scalar("SELECT provider_recording_id FROM voip_calls WHERE id = $1")
            .bind(call)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(handle.as_deref(), Some(rec.as_str()));
}

#[tokio::test]
async fn an_org_with_no_voip_retention_window_keeps_its_recordings() {
    // NULL means "follow no VoIP-specific rule", and the safe reading of that is keep.
    // Inventing a default here would delete a customer's recordings on a schedule they
    // never set.
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: no DATABASE_URL");
        return;
    };
    let owner = make_user(&pool).await;
    let org = make_enterprise_org(&pool, owner, 30).await;
    voip_settings(&pool, org, None).await;
    let rec = format!("rec-keep-{}", Uuid::new_v4());
    make_phone_call_with_recording(&pool, org, owner, 3650, &rec).await;

    let provider = std::sync::Arc::new(MockTelephonyProvider::default());
    let mut state = AppState::new(Config::test_with_billing(
        &std::env::var("DATABASE_URL").unwrap(),
        SECRET,
        0.0,
    ));
    state.pool = Some(pool.clone());
    state.telephony = Some(provider.clone());

    // Not asserted on the return count: the sweep is global and a shared test database
    // carries expired rows from other tests. What matters is that THIS org's recording was
    // not among them.
    sweep_voip_recordings_once(&state, 200).await.unwrap();
    assert!(
        !provider.recording_deleted(&rec),
        "a recording with no configured retention window was deleted anyway"
    );
}
