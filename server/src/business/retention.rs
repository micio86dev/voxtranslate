//! Enterprise data-retention sweep (spec 0106).
//!
//! Enterprise orgs configure a `retention_days` window in `settings`. This
//! background task enforces it: once past the window, a call's cloud recording
//! (the storage object) and its diarized transcript are permanently deleted, the
//! `recording_storage_path` pointer is cleared, and the session is marked
//! `transcript_status = 'expired'`.
//!
//! The feature ships **dormant**: nothing runs unless `RETENTION_SWEEP_ENABLED`
//! is truthy (see [`Config::retention_sweep_enabled`]). It is bounded (at most
//! `batch` sessions per pass) and best-effort — a failure on one session never
//! aborts the pass or the loop, and storage-delete failures leave the row intact
//! so the next pass retries (never an orphaned object with a cleared pointer).
//!
//! [`Config::retention_sweep_enabled`]: crate::config::Config::retention_sweep_enabled

use std::time::Duration;

use uuid::Uuid;

use crate::AppState;

/// Run the retention sweep forever, every `interval`. Spawned from `serve()` only
/// when the feature is enabled and a DB is present.
pub async fn run_sweep(state: AppState, interval: Duration, batch: i64) {
    let mut tick = tokio::time::interval(interval);
    loop {
        tick.tick().await;
        match sweep_once(&state, batch).await {
            Ok(0) => {}
            Ok(n) => tracing::info!("retention sweep purged {n} expired session(s)"),
            Err(e) => tracing::warn!("retention sweep failed (non-fatal): {e}"),
        }
    }
}

/// One bounded pass. Returns how many sessions were purged. No-op (returns 0)
/// without a database. Safe to call repeatedly — already-cleared sessions don't
/// match the query, and a missing storage object counts as deleted.
pub async fn sweep_once(state: &AppState, batch: i64) -> Result<u64, sqlx::Error> {
    let Some(pool) = state.pool.as_ref() else {
        return Ok(0);
    };
    let storage = state.recordings_storage.as_ref();

    // Expired sessions for Enterprise orgs with a positive retention_days. The
    // age is measured from the call's end (falling back to its start). The regex
    // guard keeps the `::int` cast safe and excludes 0 / "keep forever".
    let rows: Vec<(Uuid, Uuid, Option<String>, i32, bool)> = sqlx::query_as(
        "SELECT cs.id,
                cs.org_id,
                cs.recording_storage_path,
                (o.settings->>'retention_days')::int AS retention_days,
                COALESCE((o.settings->>'compliance_mode')::boolean, false) AS compliance
         FROM call_sessions cs
         JOIN organizations o ON o.id = cs.org_id
         WHERE o.plan = 'enterprise'
           AND (o.settings->>'retention_days') ~ '^[1-9][0-9]*$'
           AND (cs.recording_storage_path IS NOT NULL
                OR EXISTS (SELECT 1 FROM transcripts t WHERE t.session_id = cs.id))
           AND COALESCE(cs.ended_at, cs.started_at)
               < now() - make_interval(days => (o.settings->>'retention_days')::int)
         ORDER BY COALESCE(cs.ended_at, cs.started_at) ASC
         LIMIT $1",
    )
    .bind(batch)
    .fetch_all(pool)
    .await?;

    let mut purged = 0u64;
    for (session_id, org_id, rec_path, retention_days, compliance) in rows {
        // 1) Delete the recording object first. If we can't (storage unconfigured
        //    or the API errored), skip the whole session so we never clear a
        //    pointer to an object that still exists — the next pass retries.
        if let Some(path) = rec_path.as_deref() {
            let Some(storage) = storage else {
                tracing::warn!(
                    "retention: session {session_id} recording past retention but \
                     recordings storage is not configured — skipping"
                );
                continue;
            };
            if let Err(e) = storage.delete(path).await {
                tracing::warn!(
                    "retention: storage delete failed for {path} (session {session_id}): {e}"
                );
                continue;
            }
        }

        // 2) Drop the transcript text and clear the recording pointer atomically.
        let mut tx = pool.begin().await?;
        sqlx::query("DELETE FROM transcripts WHERE session_id = $1")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE call_sessions
             SET recording_storage_path = NULL, transcript_status = 'expired'
             WHERE id = $1",
        )
        .bind(session_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        // 3) Compliance audit trail (system actor → NULL). Mirrors audit.rs: only
        //    orgs in compliance mode keep a trail.
        if compliance {
            let meta = serde_json::json!({
                "retention_days": retention_days,
                "deleted_recording": rec_path.is_some(),
            });
            if let Err(e) = sqlx::query(
                "INSERT INTO audit_logs (org_id, actor_id, action, resource_type, resource_id, metadata)
                 VALUES ($1, NULL, 'retention.purge', 'call_session', $2, $3)",
            )
            .bind(org_id)
            .bind(session_id)
            .bind(meta)
            .execute(pool)
            .await
            {
                tracing::error!("retention audit insert failed: {e}");
            }
        }

        purged += 1;
    }
    Ok(purged)
}
