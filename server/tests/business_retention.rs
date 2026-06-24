//! Integration test for the Enterprise data-retention sweep (spec 0106).
//!
//! DB-gated like the other business tests: no-ops without `DATABASE_URL`. Drives
//! `business::retention::sweep_once` directly (no HTTP server needed) against a
//! transcript-only session, so no recordings storage is required.

use rust_decimal::Decimal;
use uuid::Uuid;
use voxtranslate_server::auth::{upsert_google_user, GoogleIdentity};
use voxtranslate_server::business::retention::sweep_once;
use voxtranslate_server::config::Config;
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
    upsert_google_user(pool, &identity, Decimal::ZERO, None)
        .await
        .unwrap()
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
