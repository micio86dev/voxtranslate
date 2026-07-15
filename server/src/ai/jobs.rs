//! Async job tracking for the heavy, paid AI features (report, transcript
//! correction, email draft). Each fans out into many Groq calls; on a
//! multi-hour transcript the work runs longer than the edge proxy's request
//! ceiling, so the POST endpoints return immediately with a job handle and run
//! generation in a background task. The client polls
//! `GET /api/sessions/{id}/ai-job/{job_id}` until the job is `done` (result
//! attached) or `failed` (reason attached) — no single request is held open
//! long enough to time out. See migration 014.

use std::time::Duration;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::db::Pool;

/// A pending job whose process died is reclaimable after this many minutes. The
/// running task heartbeats [`touch`] every 60s, so a *live* job — however long —
/// is never seen as stale and never double-run; only an abandoned one ages out.
const STALE_MINUTES: i32 = 3;

/// How often a running job bumps `updated_at` so it isn't mistaken for stale.
const HEARTBEAT: Duration = Duration::from_secs(60);

/// Terminal jobs are pruned once older than this (best-effort, on each claim).
const PRUNE_AFTER: &str = "1 hour";

/// A job row as the poll endpoint serves it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AiJob {
    pub id: Uuid,
    pub status: String,
    pub error: Option<String>,
    pub result_json: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

/// Outcome of claiming the job slot for `(session, feature, params_key)`.
pub enum Claim {
    /// We own a fresh (or reclaimed-stale/terminal) slot — spawn the task.
    Owned(Uuid),
    /// A recent pending job already covers this exact request — just report it,
    /// don't start a second generation (the dedup that prevents double charges).
    AlreadyRunning(Uuid),
}

/// A background job's failure: a short machine reason the client can branch on
/// (`groq`, `insufficient_credits`, `db`, `unavailable`) plus an optional JSON
/// payload (e.g. the standard 402 body for `insufficient_credits`).
pub struct JobFailure {
    pub reason: String,
    pub payload: Option<serde_json::Value>,
}

impl JobFailure {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            payload: None,
        }
    }

    pub fn with_payload(reason: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            reason: reason.into(),
            payload: Some(payload),
        }
    }
}

/// Claim the slot for this request. Atomic via `INSERT … ON CONFLICT DO UPDATE`
/// guarded by a `WHERE`: the conflict row is reclaimed only when it is terminal
/// (`done`/`failed`) or a *stale* pending (heartbeat lapsed). A live pending row
/// fails the `WHERE`, so the upsert touches nothing and returns no row — that is
/// [`Claim::AlreadyRunning`]. The row lock taken on conflict serializes racing
/// claims, so exactly one caller ever owns a slot.
pub async fn claim(
    pool: &Pool,
    session_id: Uuid,
    user_id: Uuid,
    feature: &str,
    params_key: &str,
) -> Result<Claim, sqlx::Error> {
    // Best-effort housekeeping; never blocks a claim on its outcome.
    let _ = sqlx::query(&format!(
        "DELETE FROM ai_jobs WHERE status <> 'pending' AND updated_at < now() - interval '{PRUNE_AFTER}'",
    ))
    .execute(pool)
    .await;

    let owned: Option<(Uuid,)> = sqlx::query_as(
        "INSERT INTO ai_jobs (session_id, user_id, feature, params_key, status)
         VALUES ($1, $2, $3, $4, 'pending')
         ON CONFLICT (session_id, feature, params_key) DO UPDATE
           SET status = 'pending', error = NULL, result_json = NULL,
               user_id = EXCLUDED.user_id, updated_at = now()
           WHERE ai_jobs.status <> 'pending'
              OR ai_jobs.updated_at < now() - make_interval(mins => $5)
         RETURNING id",
    )
    .bind(session_id)
    .bind(user_id)
    .bind(feature)
    .bind(params_key)
    .bind(STALE_MINUTES)
    .fetch_optional(pool)
    .await?;

    if let Some((id,)) = owned {
        return Ok(Claim::Owned(id));
    }

    // The conflict row was a live pending one (no upsert) — return its id.
    let existing: (Uuid,) = sqlx::query_as(
        "SELECT id FROM ai_jobs WHERE session_id = $1 AND feature = $2 AND params_key = $3",
    )
    .bind(session_id)
    .bind(feature)
    .bind(params_key)
    .fetch_one(pool)
    .await?;
    Ok(Claim::AlreadyRunning(existing.0))
}

/// Claim a WEBINAR job slot for `(webinar, feature, params_key)`. Same atomic
/// upsert + reclaim semantics as [`claim`], but against `webinar_ai_jobs`
/// (migration 049) — a webinar is not a `call_sessions` row, so it can't ride
/// the `ai_jobs` table (its `session_id` FKs `call_sessions`).
pub async fn claim_webinar(
    pool: &Pool,
    webinar_id: Uuid,
    user_id: Uuid,
    feature: &str,
    params_key: &str,
) -> Result<Claim, sqlx::Error> {
    let _ = sqlx::query(&format!(
        "DELETE FROM webinar_ai_jobs WHERE status <> 'pending' AND updated_at < now() - interval '{PRUNE_AFTER}'",
    ))
    .execute(pool)
    .await;

    let owned: Option<(Uuid,)> = sqlx::query_as(
        "INSERT INTO webinar_ai_jobs (webinar_id, user_id, feature, params_key, status)
         VALUES ($1, $2, $3, $4, 'pending')
         ON CONFLICT (webinar_id, feature, params_key) DO UPDATE
           SET status = 'pending', error = NULL, result_json = NULL,
               user_id = EXCLUDED.user_id, updated_at = now()
           WHERE webinar_ai_jobs.status <> 'pending'
              OR webinar_ai_jobs.updated_at < now() - make_interval(mins => $5)
         RETURNING id",
    )
    .bind(webinar_id)
    .bind(user_id)
    .bind(feature)
    .bind(params_key)
    .bind(STALE_MINUTES)
    .fetch_optional(pool)
    .await?;

    if let Some((id,)) = owned {
        return Ok(Claim::Owned(id));
    }

    let existing: (Uuid,) = sqlx::query_as(
        "SELECT id FROM webinar_ai_jobs WHERE webinar_id = $1 AND feature = $2 AND params_key = $3",
    )
    .bind(webinar_id)
    .bind(feature)
    .bind(params_key)
    .fetch_one(pool)
    .await?;
    Ok(Claim::AlreadyRunning(existing.0))
}

/// Fetch a webinar job, scoped to its webinar (blocks cross-webinar id probing).
pub async fn get_webinar(
    pool: &Pool,
    job_id: Uuid,
    webinar_id: Uuid,
) -> Result<Option<AiJob>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, status, error, result_json, created_at
         FROM webinar_ai_jobs WHERE id = $1 AND webinar_id = $2",
    )
    .bind(job_id)
    .bind(webinar_id)
    .fetch_optional(pool)
    .await
}

/// Fetch a job, scoped to its session (the caller is already session-gated, and
/// scoping blocks cross-session id probing).
pub async fn get(
    pool: &Pool,
    job_id: Uuid,
    session_id: Uuid,
) -> Result<Option<AiJob>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, status, error, result_json, created_at
         FROM ai_jobs WHERE id = $1 AND session_id = $2",
    )
    .bind(job_id)
    .bind(session_id)
    .fetch_optional(pool)
    .await
}

/// Mark a job done and attach the result the client will render. `table` is the
/// job table (`ai_jobs` for meets, `webinar_ai_jobs` for webinars) — both share
/// the same columns, so one implementation serves both.
async fn complete(
    pool: &Pool,
    table: &str,
    job_id: Uuid,
    result: serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(&format!(
        "UPDATE {table} SET status = 'done', result_json = $2, error = NULL, updated_at = now()
         WHERE id = $1",
    ))
    .bind(job_id)
    .bind(result)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark a job failed with a machine-readable reason (+ optional payload).
async fn fail(
    pool: &Pool,
    table: &str,
    job_id: Uuid,
    reason: &str,
    payload: Option<serde_json::Value>,
) -> Result<(), sqlx::Error> {
    sqlx::query(&format!(
        "UPDATE {table} SET status = 'failed', error = $2, result_json = $3, updated_at = now()
         WHERE id = $1",
    ))
    .bind(job_id)
    .bind(reason)
    .bind(payload)
    .execute(pool)
    .await?;
    Ok(())
}

/// Heartbeat a still-pending job's `updated_at`. Returns `false` once the row is
/// no longer pending (or gone), which stops the heartbeat loop.
async fn touch(pool: &Pool, table: &str, job_id: Uuid) -> bool {
    sqlx::query(&format!(
        "UPDATE {table} SET updated_at = now() WHERE id = $1 AND status = 'pending'"
    ))
    .bind(job_id)
    .execute(pool)
    .await
    .map(|r| r.rows_affected() == 1)
    .unwrap_or(false)
}

async fn heartbeat(pool: Pool, table: String, job_id: Uuid) {
    loop {
        tokio::time::sleep(HEARTBEAT).await;
        if !touch(&pool, &table, job_id).await {
            break;
        }
    }
}

/// Drive a claimed `ai_jobs` (meet) job to completion. See [`run_on`].
pub async fn run<F>(pool: Pool, job_id: Uuid, work: F)
where
    F: std::future::Future<Output = Result<serde_json::Value, JobFailure>>,
{
    run_on(pool, "ai_jobs", job_id, work).await
}

/// Drive a claimed WEBINAR (`webinar_ai_jobs`) job to completion. See [`run_on`].
pub async fn run_webinar<F>(pool: Pool, job_id: Uuid, work: F)
where
    F: std::future::Future<Output = Result<serde_json::Value, JobFailure>>,
{
    run_on(pool, "webinar_ai_jobs", job_id, work).await
}

/// Drive a claimed job to completion against `table`: heartbeat it while `work`
/// runs, then record the result or the failure. The spawned future returns the
/// JSON body the synchronous endpoint used to return, or a [`JobFailure`].
async fn run_on<F>(pool: Pool, table: &'static str, job_id: Uuid, work: F)
where
    F: std::future::Future<Output = Result<serde_json::Value, JobFailure>>,
{
    let hb = tokio::spawn(heartbeat(pool.clone(), table.to_string(), job_id));
    let outcome = work.await;
    hb.abort();
    let written = match outcome {
        Ok(v) => complete(&pool, table, job_id, v).await,
        Err(f) => fail(&pool, table, job_id, &f.reason, f.payload).await,
    };
    if let Err(e) = written {
        tracing::error!("ai job {job_id} result write failed: {e}");
    }
}
