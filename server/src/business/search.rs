//! Semantic transcript search (Business dashboard).
//!
//! `GET /api/business/organizations/{org_id}/search?q=&project_id=&limit=` runs a
//! pgvector cosine KNN over `transcript_embeddings`, scoped to the projects the
//! caller may see:
//!   - **member** → transcripts of projects they CREATED or PARTICIPATED in;
//!   - **admin / owner** → all of the org's transcripts.
//!
//! Authorization is enforced here in the API layer (via [`require_role`]), never
//! client-side; an out-of-scope `project_id` simply yields no rows (no leak).
//!
//! This module also hosts the process-internal `POST /internal/embeddings/backfill`
//! (guarded by `EMBEDDINGS_BACKFILL_SECRET`) that embeds pre-existing transcripts
//! in batches — run once after the feature ships.

use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::FromRow;
use uuid::Uuid;

use crate::business::transcripts::{self, Segment};
use crate::business::{
    audit, bad_request, db_err, require_pool, require_role, role_rank, ADMIN, MEMBER,
};
use crate::middleware::AuthUser;
use crate::AppState;

#[derive(Deserialize, Default)]
pub struct SearchQuery {
    q: Option<String>,
    project_id: Option<Uuid>,
    limit: Option<i64>,
}

#[derive(FromRow)]
struct ResultRow {
    session_id: Uuid,
    project_id: Option<Uuid>,
    project_name: Option<String>,
    room: String,
    started_at: DateTime<Utc>,
    content: String,
    speaker_name: Option<String>,
    start_ms: Option<i64>,
    score: f64,
}

/// `GET /api/business/organizations/{org_id}/search` — semantic transcript search.
/// Requires org membership; the visible project set is resolved from the caller's role.
pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Query(q): Query<SearchQuery>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let role = require_role(pool, org_id, user.user_id, MEMBER).await?;
    let is_admin = role_rank(&role) >= ADMIN;

    let query = q.q.as_deref().map(str::trim).unwrap_or_default();
    if query.is_empty() {
        return Err(bad_request("q is required"));
    }
    let limit = q.limit.unwrap_or(20).clamp(1, 50);

    let embedder = state.embeddings.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "semantic search not configured",
        )
            .into_response()
    })?;

    // Visible project set. Admins/owners search the whole org (no project filter);
    // members search projects they CREATED or PARTICIPATED in (a call where they
    // were a participant). A member with no visible projects can match nothing.
    let scope: Option<Vec<Uuid>> = if is_admin {
        None
    } else {
        let visible: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM projects
             WHERE org_id = $1 AND archived_at IS NULL
               AND (created_by = $2
                    OR id IN (
                        SELECT DISTINCT cs.project_id FROM call_sessions cs
                        JOIN session_participants sp ON sp.session_id = cs.id
                        WHERE cs.org_id = $1 AND sp.user_id = $2 AND cs.project_id IS NOT NULL
                    ))",
        )
        .bind(org_id)
        .bind(user.user_id)
        .fetch_all(pool)
        .await
        .map_err(db_err)?;
        if visible.is_empty() {
            return Ok(Json(json!({ "results": [] })).into_response());
        }
        Some(visible)
    };

    let vector = embedder.embed_one(query).await.map_err(|e| {
        tracing::error!("search query embedding failed: {e}");
        (StatusCode::BAD_GATEWAY, "embedding failed").into_response()
    })?;
    let qvec = pgvector::Vector::from(vector);

    // KNN with the scope filter. `$3::uuid[] IS NULL` is the admin path (no project
    // restriction, including unassigned calls); members pass their visible set, which
    // also excludes any explicit `project_id` they can't see. `$4` optionally narrows
    // to one project.
    let rows: Vec<ResultRow> = sqlx::query_as(
        "SELECT te.session_id, te.project_id, p.name AS project_name,
                cs.room, cs.started_at, te.content, te.speaker_name, te.start_ms,
                1 - (te.embedding <=> $2) AS score
         FROM transcript_embeddings te
         JOIN call_sessions cs ON cs.id = te.session_id
         LEFT JOIN projects p  ON p.id = te.project_id
         WHERE te.org_id = $1
           AND ($3::uuid[] IS NULL OR te.project_id = ANY($3))
           AND ($4::uuid IS NULL OR te.project_id = $4)
         ORDER BY te.embedding <=> $2
         LIMIT $5",
    )
    .bind(org_id)
    .bind(qvec)
    .bind(scope.as_deref())
    .bind(q.project_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;

    // Compliance trail (like `transcript.view`). The query text itself is not logged —
    // only its length — to keep search terms out of the audit store.
    audit::log_audit_event(
        pool,
        org_id,
        user.user_id,
        "transcript.search",
        "transcript",
        org_id,
        json!({ "q_len": query.chars().count(), "project_id": q.project_id, "results": rows.len() }),
    );

    let results: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "session_id": r.session_id,
                "project_id": r.project_id,
                "project_name": r.project_name,
                "room": r.room,
                "started_at": r.started_at,
                "snippet": r.content,
                "speaker_name": r.speaker_name,
                "start_ms": r.start_ms,
                "score": r.score,
            })
        })
        .collect();

    Ok(Json(json!({ "results": results })).into_response())
}

// ---- Internal backfill -------------------------------------------------------

#[derive(FromRow)]
struct BackfillRow {
    id: Uuid,
    session_id: Uuid,
    org_id: Uuid,
    project_id: Option<Uuid>,
    segments: Value,
}

#[derive(Deserialize, Default)]
pub struct BackfillQuery {
    /// Transcripts to process this call (default 50, max 500). Call repeatedly until
    /// `remaining` is 0.
    limit: Option<i64>,
}

/// Bearer-secret extractor for the internal backfill endpoint, mirroring
/// [`crate::bench::BenchAuth`]: `404` when `EMBEDDINGS_BACKFILL_SECRET` is unset
/// (the endpoint should look absent), `401` on a missing/wrong token.
pub struct BackfillAuth;

impl FromRequestParts<AppState> for BackfillAuth {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let Some(secret) = state.config.embeddings_backfill_secret.as_deref() else {
            return Err(StatusCode::NOT_FOUND.into_response());
        };
        let bearer = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if constant_eq(bearer, secret) {
            Ok(BackfillAuth)
        } else {
            Err(StatusCode::UNAUTHORIZED.into_response())
        }
    }
}

/// Length-checked, branch-light comparison so a wrong secret doesn't leak its
/// length via timing (same approach as `bench::constant_eq`).
fn constant_eq(given: Option<&str>, secret: &str) -> bool {
    let Some(given) = given else { return false };
    if given.len() != secret.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in given.bytes().zip(secret.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// `POST /internal/embeddings/backfill?limit=` — embed transcripts that have no
/// `transcript_embeddings` rows yet, oldest first, in a bounded batch. Idempotent
/// and resumable: re-run until the response reports `remaining: 0`.
pub async fn backfill(
    auth: Result<BackfillAuth, Response>,
    State(state): State<AppState>,
    Query(q): Query<BackfillQuery>,
) -> Result<Response, Response> {
    auth?;
    let pool = require_pool(&state)?;
    if state.embeddings.is_none() {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "embeddings not configured").into_response());
    }
    let limit = q.limit.unwrap_or(50).clamp(1, 500);

    let rows: Vec<BackfillRow> = sqlx::query_as(
        "SELECT t.id, t.session_id, t.org_id, cs.project_id, t.segments
         FROM transcripts t
         JOIN call_sessions cs ON cs.id = t.session_id
         WHERE NOT EXISTS (
             SELECT 1 FROM transcript_embeddings te WHERE te.transcript_id = t.id
         )
         ORDER BY t.created_at ASC
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;

    let mut embedded = 0usize;
    let mut failed = 0usize;
    for r in &rows {
        let segments: Vec<Segment> = serde_json::from_value(r.segments.clone()).unwrap_or_default();
        match transcripts::embed_and_store(
            &state,
            r.id,
            r.session_id,
            r.org_id,
            r.project_id,
            &segments,
        )
        .await
        {
            Ok(n) if n > 0 => embedded += 1,
            Ok(_) => {} // empty transcript — nothing to embed
            Err(e) => {
                failed += 1;
                tracing::warn!("backfill embed failed ({}): {e}", r.session_id);
            }
        }
    }

    let remaining: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM transcripts t
         WHERE NOT EXISTS (SELECT 1 FROM transcript_embeddings te WHERE te.transcript_id = t.id)",
    )
    .fetch_one(pool)
    .await
    .map_err(db_err)?;

    Ok(Json(json!({
        "processed": rows.len(),
        "embedded": embedded,
        "failed": failed,
        "remaining": remaining,
    }))
    .into_response())
}
