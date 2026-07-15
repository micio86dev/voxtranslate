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

/// One KNN hit. A row is EITHER a call (`session_id` set, `webinar_id`/`webinar_code`
/// NULL) OR a webinar (`webinar_id`/`webinar_code` set, `session_id` NULL) — the
/// `transcript_embeddings_owner_excl` CHECK (migration 050) guarantees they are
/// never both set. `room`/`started_at` are coalesced across the two sources so the
/// snippet metadata is uniform; `webinar_code` links a webinar hit back to `/w/{code}`.
#[derive(FromRow)]
struct ResultRow {
    session_id: Option<Uuid>,
    webinar_id: Option<Uuid>,
    webinar_code: Option<String>,
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
        // A member with no visible projects can still match webinars they HOST, so
        // only short-circuit when they also host nothing in this org. An empty
        // `visible` set (bound as `'{}'`) makes the project predicate always false,
        // leaving only the host-owned webinar rows to match.
        if visible.is_empty() {
            let hosts_any: Option<Uuid> = sqlx::query_scalar(
                "SELECT id FROM webinars
                 WHERE org_id = $1 AND host_user_id = $2 AND archived_at IS NULL
                 LIMIT 1",
            )
            .bind(org_id)
            .bind(user.user_id)
            .fetch_optional(pool)
            .await
            .map_err(db_err)?;
            if hosts_any.is_none() {
                return Ok(Json(json!({ "results": [] })).into_response());
            }
        }
        Some(visible)
    };

    let vector = embedder.embed_one(query).await.map_err(|e| {
        tracing::error!("search query embedding failed: {e}");
        (StatusCode::BAD_GATEWAY, "embedding failed").into_response()
    })?;
    let qvec = pgvector::Vector::from(vector);

    // KNN over BOTH call and webinar embeddings (Phase C). The two owner kinds are
    // LEFT-JOINed to their source so `room`/`started_at` are coalesced (call → the
    // call_sessions row; webinar → the webinars row); the `owner_excl` CHECK means
    // exactly one join matches per embedding row.
    //
    // Scope: `$3::uuid[] IS NULL` is the admin path (whole org, incl. project-less
    // calls AND project-less webinars). Members pass their visible project set in
    // `$3`; a row passes if its `project_id` is in that set OR — for a webinar — the
    // member is the host (`$6`). This mirrors the meet rule (see own/participated
    // projects) and additionally lets a host find their OWN webinar even when it is
    // not bound to a project. `$4` optionally narrows to one project.
    let rows: Vec<ResultRow> = sqlx::query_as(
        "SELECT te.session_id, te.webinar_id, w.code AS webinar_code,
                te.project_id, p.name AS project_name,
                COALESCE(cs.room, w.title) AS room,
                COALESCE(cs.started_at, w.actual_start, w.created_at) AS started_at,
                te.content, te.speaker_name, te.start_ms,
                1 - (te.embedding <=> $2) AS score
         FROM transcript_embeddings te
         LEFT JOIN call_sessions cs ON cs.id = te.session_id
         LEFT JOIN webinars w       ON w.id = te.webinar_id
         LEFT JOIN projects p       ON p.id = te.project_id
         WHERE te.org_id = $1
           AND (te.session_id IS NOT NULL OR te.webinar_id IS NOT NULL)
           AND ($3::uuid[] IS NULL
                OR te.project_id = ANY($3)
                OR (te.webinar_id IS NOT NULL AND w.host_user_id = $6))
           AND ($4::uuid IS NULL OR te.project_id = $4)
         ORDER BY te.embedding <=> $2
         LIMIT $5",
    )
    .bind(org_id)
    .bind(qvec)
    .bind(scope.as_deref())
    .bind(q.project_id)
    .bind(limit)
    .bind(user.user_id)
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

    // A hit is unambiguously a call OR a webinar. `kind` labels it; call hits carry
    // `session_id`, webinar hits carry `webinar_id` + `webinar_code` (to link to
    // `/w/{code}`). The other id is null for the source that didn't produce the row.
    let results: Vec<Value> = rows.iter().map(result_json).collect();

    Ok(Json(json!({ "results": results })).into_response())
}

/// The source kind of a search hit: `"call"` when it came from a call session,
/// `"webinar"` when it came from a finished webinar. The `owner_excl` CHECK
/// (migration 050) guarantees a row is never both; a webinar owner wins if present.
fn result_kind(r: &ResultRow) -> &'static str {
    if r.webinar_id.is_some() {
        "webinar"
    } else {
        "call"
    }
}

/// Serialize one hit to the API shape. Split out so the call-vs-webinar mapping is
/// unit-testable without a live database.
fn result_json(r: &ResultRow) -> Value {
    json!({
        "kind": result_kind(r),
        "session_id": r.session_id,
        "webinar_id": r.webinar_id,
        "webinar_code": r.webinar_code,
        "project_id": r.project_id,
        "project_name": r.project_name,
        "room": r.room,
        "started_at": r.started_at,
        "snippet": r.content,
        "speaker_name": r.speaker_name,
        "start_ms": r.start_ms,
        "score": r.score,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn call_row() -> ResultRow {
        ResultRow {
            session_id: Some(Uuid::from_u128(1)),
            webinar_id: None,
            webinar_code: None,
            project_id: Some(Uuid::from_u128(9)),
            project_name: Some("Sales".into()),
            room: "Standup".into(),
            started_at: Utc::now(),
            content: "we shipped the release".into(),
            speaker_name: Some("Alice".into()),
            start_ms: Some(12_000),
            score: 0.83,
        }
    }

    fn webinar_row() -> ResultRow {
        ResultRow {
            session_id: None,
            webinar_id: Some(Uuid::from_u128(2)),
            webinar_code: Some("AbC123".into()),
            project_id: None,
            project_name: None,
            room: "Q3 Webinar".into(),
            started_at: Utc::now(),
            content: "welcome everyone".into(),
            speaker_name: Some("Host".into()),
            start_ms: Some(0),
            score: 0.91,
        }
    }

    #[test]
    fn kind_distinguishes_call_from_webinar() {
        assert_eq!(result_kind(&call_row()), "call");
        assert_eq!(result_kind(&webinar_row()), "webinar");
    }

    #[test]
    fn call_hit_carries_session_id_not_webinar_fields() {
        let v = result_json(&call_row());
        assert_eq!(v["kind"], "call");
        assert_eq!(v["session_id"], Uuid::from_u128(1).to_string());
        assert!(v["webinar_id"].is_null());
        assert!(v["webinar_code"].is_null());
        assert_eq!(v["room"], "Standup");
        assert_eq!(v["snippet"], "we shipped the release");
    }

    #[test]
    fn webinar_hit_carries_webinar_id_and_code_not_session() {
        let v = result_json(&webinar_row());
        assert_eq!(v["kind"], "webinar");
        assert!(v["session_id"].is_null());
        assert_eq!(v["webinar_id"], Uuid::from_u128(2).to_string());
        assert_eq!(v["webinar_code"], "AbC123");
        assert_eq!(v["room"], "Q3 Webinar", "webinar title coalesced into room");
        assert_eq!(v["snippet"], "welcome everyone");
    }
}
