//! Erasing the content of a Live Connect conversation (spec 0121, S17).
//!
//! A conversation is two things that must not be confused. Its **content** — what was said, who
//! said it, the guest's name — is personal data, kept only as long as there is a reason. Its
//! **billing facts** — the session row, the metered ticks, the ledger entries — are what the
//! payer's history and the platform's books rest on, and outlive the content.
//!
//! The content lives on `call_sessions` rows, one per room the conversation lived through (see
//! [`super::finalize::room_session_ids`]). Deleting those rows cascades to everything that hangs
//! off them: participants, speech events, transcripts and their embeddings. The link back from
//! `live_sessions` is `ON DELETE SET NULL`, so the billing row survives. What does NOT cascade
//! is the guest's name on `live_participants`; that is scrubbed in the same transaction.
//!
//! One function, [`erase_content`], is used by every reason to erase: an admin or an owner
//! deleting a conversation, the retention pass, and account deletion. Before it existed, deleting
//! an owner's account left the guest's name and words behind, because the foreign key points from
//! `live_sessions` to `call_sessions` and not the other way (defect D4).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::delete;
use axum::{Json, Router};
use serde_json::json;
use uuid::Uuid;

use super::finalize::room_sessions_sql;
use super::routes::api_err;
use crate::business::{audit, db_err, require_pool, require_role, ADMIN};
use crate::db::Pool;
use crate::middleware::AuthUser;
use crate::AppState;

/// What a scrubbed participant's name is replaced with.
pub const ERASED_NAME: &str = "Erased";

/// Conversations erased per retention pass; the next pass takes the next batch.
const RETENTION_BATCH: i64 = 100;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Erased {
    /// Meeting-side rows deleted (each takes its participants, events, transcript and
    /// embeddings with it).
    pub call_sessions: u64,
    /// Live participants whose name and token hash were scrubbed.
    pub participants_scrubbed: u64,
}

/// Delete the content of one conversation, in one transaction, and keep its billing facts.
///
/// Every row deleted is named by the conversation itself, never by user or organisation: the
/// `call_sessions` of ITS rooms only (see `room_sessions_sql`). Safe to run again: a second run
/// finds nothing to delete and nothing left to scrub. It does not check that the conversation
/// has ended; callers that must not touch an open one (the endpoints, retention) check first.
pub async fn erase_content(pool: &Pool, live_session_id: Uuid) -> Result<Erased, sqlx::Error> {
    let mut tx = pool.begin().await?;
    // Serialises with another eraser; a finaliser holding a room row simply finishes first.
    let exists: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM live_sessions WHERE id = $1 FOR UPDATE")
            .bind(live_session_id)
            .fetch_optional(&mut *tx)
            .await?;
    if exists.is_none() {
        return Ok(Erased::default());
    }

    let delete = format!(
        "DELETE FROM call_sessions WHERE id IN ({}) RETURNING id",
        room_sessions_sql("$1")
    );
    let deleted: Vec<Uuid> = sqlx::query_scalar(&delete)
        .bind(live_session_id)
        .fetch_all(&mut *tx)
        .await?;

    // The guest's name is personal data that the cascade does not reach; the token hash is
    // a credential fingerprint with no use once the conversation's content is gone. User ids
    // stay: which member answered is part of who did what, and goes with the member's account.
    let scrubbed = sqlx::query(
        "UPDATE live_participants SET display_name = $2, token_hash = NULL
         WHERE session_id = $1 AND (display_name <> $2 OR token_hash IS NOT NULL)",
    )
    .bind(live_session_id)
    .bind(ERASED_NAME)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    if !deleted.is_empty() || scrubbed > 0 {
        sqlx::query(
            "INSERT INTO live_session_events (session_id, kind, detail) VALUES ($1, 'content_erased', $2)",
        )
        .bind(live_session_id)
        .bind(json!({ "call_sessions": deleted.len() }))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(Erased {
        call_sessions: deleted.len() as u64,
        participants_scrubbed: scrubbed,
    })
}

/// Erase the content of every conversation `user_id` owns (a consumer session's owner is its
/// host). Called by account deletion BEFORE the user row goes: afterwards the cascade removes
/// the `live_sessions` rows and with them the only way to find the meeting-side rows. Returns
/// how many conversations were processed.
pub async fn erase_owned_by(pool: &Pool, user_id: Uuid) -> Result<u64, sqlx::Error> {
    let owned: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM live_sessions WHERE owner_user_id = $1")
            .bind(user_id)
            .fetch_all(pool)
            .await?;
    for id in &owned {
        erase_content(pool, *id).await?;
    }
    Ok(owned.len() as u64)
}

/// Erase the content of ended conversations that have outlived their retention. Returns how
/// many were erased. `only` scopes a pass to given sessions (tests share one database).
///
/// * A **business** conversation is kept for its organisation's `settings.retention_days`;
///   `0` there means the organisation keeps everything. An organisation that set nothing (or
///   something that is not a whole number of days) gets `LIVE_RETENTION_DEFAULT_DAYS`, where
///   `0` means no default retention.
/// * A **consumer** conversation is only ever stored when its owner opted in, and is kept for
///   `LIVE_B2C_RETENTION_DAYS` (`0` keeps it until the owner deletes it).
///
/// Age is measured from when the conversation ended. Open conversations are never touched. A
/// conversation whose content is already gone is not selected again.
pub async fn retention_pass(state: &AppState, only: Option<&[Uuid]>) -> Result<u64, sqlx::Error> {
    let Some(pool) = state.pool.as_ref() else {
        return Ok(0);
    };
    let cfg = &state.live.config;
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT ls.id
         FROM live_sessions ls
         LEFT JOIN organizations o ON o.id = ls.org_id
         CROSS JOIN LATERAL (
             SELECT CASE
                 WHEN ls.org_id IS NOT NULL THEN
                     CASE WHEN (o.settings->>'retention_days') IS NULL THEN $2::int
                          WHEN (o.settings->>'retention_days') ~ '^[0-9]{1,4}$'
                          THEN (o.settings->>'retention_days')::int
                          -- Set, but not a whole number of days we understand: the organisation
                          -- said SOMETHING, so do not erase on a default it never chose.
                          ELSE 0 END
                 WHEN ls.save_transcript THEN $3::int
                 ELSE 0
             END AS days
         ) r
         WHERE ls.status IN ('ended', 'expired')
           AND r.days > 0
           AND ($1::uuid[] IS NULL OR ls.id = ANY($1))
           AND COALESCE(ls.ended_at, ls.created_at) < now() - make_interval(days => r.days)
           AND (ls.call_session_id IS NOT NULL
                OR EXISTS (SELECT 1 FROM live_participants p
                           WHERE p.session_id = ls.id
                             AND (p.display_name <> $5 OR p.token_hash IS NOT NULL)))
         ORDER BY COALESCE(ls.ended_at, ls.created_at)
         LIMIT $4",
    )
    .bind(only)
    .bind(cfg.retention_default_days as i32)
    .bind(cfg.b2c_retention_days as i32)
    .bind(RETENTION_BATCH)
    .bind(ERASED_NAME)
    .fetch_all(pool)
    .await?;

    let mut erased = 0;
    for id in ids {
        match erase_content(pool, id).await {
            Ok(_) => erased += 1,
            // One conversation failing must not stop the others; the next pass retries it.
            Err(e) => tracing::warn!(session = %id, error = %e, "live: retention erase failed"),
        }
    }
    Ok(erased)
}

// ---- endpoints -----------------------------------------------------------------------------

fn not_found() -> Response {
    api_err(
        StatusCode::NOT_FOUND,
        "session_not_found",
        "conversation not found",
    )
}

fn still_open() -> Response {
    api_err(
        StatusCode::CONFLICT,
        "session_active",
        "End the conversation before deleting it.",
    )
}

fn is_open(status: &str) -> bool {
    matches!(status, "waiting" | "active")
}

/// `DELETE /api/business/organizations/{org_id}/live/sessions/{id}` — an org admin deletes the
/// content of one of the organisation's conversations. 404 for a non-member and for a
/// conversation that is not this organisation's; 403 for a plain member; 409 while it is open.
/// The billing record stays. Audited.
pub async fn delete_org_session(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, id)): Path<(Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;
    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM live_sessions WHERE id = $1 AND org_id = $2")
            .bind(id)
            .bind(org_id)
            .fetch_optional(pool)
            .await
            .map_err(db_err)?;
    let Some(status) = status else {
        return Err(not_found());
    };
    if is_open(&status) {
        return Err(still_open());
    }
    let out = erase_content(pool, id).await.map_err(db_err)?;
    audit::log_audit_event(
        pool,
        org_id,
        user.user_id,
        "live_session.delete",
        "live_session",
        id,
        json!({ "call_sessions": out.call_sessions }),
    );
    Ok(Json(json!({ "erased": true, "call_sessions": out.call_sessions })).into_response())
}

/// `DELETE /api/live/sessions/{id}` — the owner of a consumer conversation deletes its content.
/// Account JWT, owner only: anyone else (another account, a business conversation's admin, a
/// participant's live token) gets the same answer as for a conversation that does not exist.
pub async fn delete_own_session(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM live_sessions WHERE id = $1 AND owner_user_id = $2")
            .bind(id)
            .bind(user.user_id)
            .fetch_optional(pool)
            .await
            .map_err(db_err)?;
    let Some(status) = status else {
        return Err(not_found());
    };
    if is_open(&status) {
        return Err(still_open());
    }
    let out = erase_content(pool, id).await.map_err(db_err)?;
    Ok(Json(json!({ "erased": true, "call_sessions": out.call_sessions })).into_response())
}

/// Replace the names a conversation's participants gave, and drop their token hashes. Used for a
/// personal conversation that was never meant to be kept, once it is over.
pub async fn scrub_participants(pool: &Pool, live_session_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE live_participants SET display_name = $2, token_hash = NULL
         WHERE session_id = $1 AND (display_name <> $2 OR token_hash IS NOT NULL)",
    )
    .bind(live_session_id)
    .bind(ERASED_NAME)
    .execute(pool)
    .await?;
    Ok(())
}

/// Erase the name of an account that is being deleted from every conversation it sat in. The
/// participant row itself stays (migration 066 turns the foreign key into SET NULL), so the
/// conversation's billing ticks survive the account.
pub async fn scrub_user(pool: &Pool, user_id: Uuid) -> Result<u64, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE live_participants SET display_name = $2, token_hash = NULL WHERE user_id = $1",
    )
    .bind(user_id)
    .bind(ERASED_NAME)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// The two erasure endpoints. Mounted **regardless** of `LIVE_CONNECT_ENABLED`: turning the
/// feature off must not take away the means to delete what it already stored.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/business/organizations/{org_id}/live/sessions/{id}",
            delete(delete_org_session),
        )
        .route("/api/live/sessions/{id}", delete(delete_own_session))
}
