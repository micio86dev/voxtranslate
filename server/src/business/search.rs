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
    /// Narrow to one kind of session: `call`, `voice_message`, `phone` or `live_connect`.
    kind: Option<String>,
    /// Language of the conversation. For a Live Connect conversation (two languages) either
    /// side's language matches; for everything else the transcript's source language.
    lang: Option<String>,
    /// RFC 3339 instants bounding when the session started (both inclusive).
    from: Option<String>,
    to: Option<String>,
    /// One session: either the call-session id a hit carries or a Live Connect session id.
    session_id: Option<Uuid>,
    /// Case-insensitive substring of the speaker's name.
    speaker: Option<String>,
}

/// The optional filters of a search, validated. Every one is `None` by default, and a search
/// without filters is exactly the search that existed before they did.
#[derive(Debug, Default, Clone)]
pub struct Filters {
    pub kind: Option<String>,
    pub lang: Option<String>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub session_id: Option<Uuid>,
    /// Already escaped for `ILIKE`.
    pub speaker_like: Option<String>,
}

/// Kinds of `call_sessions.kind` a search can be narrowed to.
const SESSION_KINDS: [&str; 4] = ["call", "voice_message", "phone", "live_connect"];

impl Filters {
    /// Validate the query's filters. The error is the message for a 400.
    fn parse(q: &SearchQuery) -> Result<Self, &'static str> {
        let kind = match q.kind.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
            None => None,
            Some(k) if SESSION_KINDS.contains(&k) => Some(k.to_string()),
            Some(_) => return Err("kind must be one of call, voice_message, phone, live_connect"),
        };
        let lang = match q.lang.as_deref().map(str::trim).filter(|l| !l.is_empty()) {
            None => None,
            Some(l) => {
                let l = l.to_ascii_lowercase();
                let ok = l.len() <= 8 && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
                if !ok {
                    return Err("lang is not a valid language code");
                }
                Some(l)
            }
        };
        let instant = |raw: &Option<String>, err: &'static str| match raw
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            None => Ok(None),
            Some(v) => DateTime::parse_from_rfc3339(v)
                .map(|d| Some(d.with_timezone(&Utc)))
                .map_err(|_| err),
        };
        let from = instant(&q.from, "from must be an RFC 3339 timestamp")?;
        let to = instant(&q.to, "to must be an RFC 3339 timestamp")?;
        let speaker_like = q
            .speaker
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                // A name is searched as text, never as a pattern: `%` and `_` are escaped.
                s.chars()
                    .take(100)
                    .flat_map(|c| match c {
                        '\\' | '%' | '_' => vec!['\\', c],
                        c => vec![c],
                    })
                    .collect::<String>()
            });
        Ok(Self {
            kind,
            lang,
            from,
            to,
            session_id: q.session_id,
            speaker_like,
        })
    }
}

/// One KNN hit. A row is EITHER a call (`session_id` set, `webinar_id`/`webinar_code`
/// NULL) OR a webinar (`webinar_id`/`webinar_code` set, `session_id` NULL) — the
/// `transcript_embeddings_owner_excl` CHECK (migration 050) guarantees they are
/// never both set. `room`/`started_at` are coalesced across the two sources so the
/// snippet metadata is uniform; `webinar_code` links a webinar hit back to `/w/{code}`.
#[derive(FromRow)]
pub struct ResultRow {
    pub session_id: Option<Uuid>,
    pub webinar_id: Option<Uuid>,
    pub webinar_code: Option<String>,
    pub project_id: Option<Uuid>,
    pub project_name: Option<String>,
    pub room: String,
    pub started_at: DateTime<Utc>,
    pub content: String,
    pub speaker_name: Option<String>,
    pub start_ms: Option<i64>,
    pub score: f64,
    /// `call_sessions.kind` of the hit's session. Only reported for Live Connect.
    pub session_kind: Option<String>,
    /// The Live Connect session the hit belongs to; what a client opens the conversation by.
    pub live_session_id: Option<Uuid>,
}

/// Which projects a caller may search. Resolved from their role, never from the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Visible {
    /// An admin or owner: the whole org, project-less calls and webinars included.
    Everything,
    /// A member: projects they created or took part in (possibly none — they may still host
    /// webinars).
    Projects(Vec<Uuid>),
    /// A member who can see no project and hosts no webinar: nothing can match.
    Nothing,
}

/// Resolve what `user_id` may search in `org_id`.
///
/// Members search projects they CREATED or PARTICIPATED in (a call where they were a
/// participant); admins/owners search the whole org.
pub async fn resolve_visibility(
    pool: &crate::db::Pool,
    org_id: Uuid,
    user_id: Uuid,
    is_admin: bool,
) -> Result<Visible, sqlx::Error> {
    if is_admin {
        return Ok(Visible::Everything);
    }
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
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    // A member with no visible projects can still match webinars they HOST, so only
    // short-circuit when they also host nothing in this org. An empty `visible` set (bound
    // as `'{}'`) makes the project predicate always false, leaving only the host-owned
    // webinar rows to match.
    if visible.is_empty() {
        let hosts_any: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM webinars
             WHERE org_id = $1 AND host_user_id = $2 AND archived_at IS NULL
             LIMIT 1",
        )
        .bind(org_id)
        .bind(user_id)
        .fetch_optional(pool)
        .await?;
        if hosts_any.is_none() {
            return Ok(Visible::Nothing);
        }
    }
    Ok(Visible::Projects(visible))
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
    let filters = Filters::parse(&q).map_err(bad_request)?;
    let limit = q.limit.unwrap_or(20).clamp(1, 50);

    let embedder = state.embeddings.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "semantic search not configured",
        )
            .into_response()
    })?;

    let visible = resolve_visibility(pool, org_id, user.user_id, is_admin)
        .await
        .map_err(db_err)?;
    if visible == Visible::Nothing {
        return Ok(Json(json!({ "results": [] })).into_response());
    }

    let vector = embedder.embed_one(query).await.map_err(|e| {
        tracing::error!("search query embedding failed: {e}");
        (StatusCode::BAD_GATEWAY, "embedding failed").into_response()
    })?;
    let qvec = pgvector::Vector::from(vector);

    let rows = knn(
        pool,
        org_id,
        user.user_id,
        &visible,
        q.project_id,
        &filters,
        qvec,
        limit,
    )
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

/// The vector search itself, with its scope and filters. Split out of the handler so the
/// scoping SQL can be exercised against a real database with a hand-made vector (producing a
/// real one needs the embedding provider).
///
/// KNN over BOTH call and webinar embeddings (Phase C). The two owner kinds are LEFT-JOINed
/// to their source so `room`/`started_at` are coalesced (call → the call_sessions row;
/// webinar → the webinars row); the `owner_excl` CHECK means exactly one join matches per
/// embedding row.
///
/// Scope: `Everything` is the admin path (whole org, incl. project-less calls AND
/// project-less webinars). A member passes their visible project set; a row passes if its
/// `project_id` is in that set OR — for a webinar — the member is the host. This mirrors the
/// meet rule (see own/participated projects) and additionally lets a host find their OWN
/// webinar even when it is not bound to a project. `project_id` optionally narrows to one
/// project.
#[allow(clippy::too_many_arguments)]
pub async fn knn(
    pool: &crate::db::Pool,
    org_id: Uuid,
    user_id: Uuid,
    visible: &Visible,
    project_id: Option<Uuid>,
    filters: &Filters,
    qvec: pgvector::Vector,
    limit: i64,
) -> Result<Vec<ResultRow>, sqlx::Error> {
    let scope: Option<&[Uuid]> = match visible {
        Visible::Everything => None,
        Visible::Projects(p) => Some(p.as_slice()),
        Visible::Nothing => return Ok(Vec::new()),
    };
    // `started_at` of a Live Connect hit is when the CONVERSATION started (a conversation that
    // lived through several rooms has one meeting-side row per room); everything else is as
    // before, because `ls` is NULL for them.
    sqlx::query_as(
        "SELECT te.session_id, te.webinar_id, w.code AS webinar_code,
                te.project_id, p.name AS project_name,
                COALESCE(cs.room, w.title) AS room,
                COALESCE(ls.started_at, cs.started_at, w.actual_start, w.created_at) AS started_at,
                te.content, te.speaker_name, te.start_ms,
                1 - (te.embedding <=> $2) AS score,
                CASE WHEN ls.id IS NOT NULL THEN cs.kind END AS session_kind,
                ls.id AS live_session_id
         FROM transcript_embeddings te
         LEFT JOIN call_sessions cs ON cs.id = te.session_id
         LEFT JOIN live_sessions ls ON ls.call_session_id = te.session_id
         LEFT JOIN webinars w       ON w.id = te.webinar_id
         LEFT JOIN projects p       ON p.id = te.project_id
         LEFT JOIN transcripts t    ON t.id = te.transcript_id
         WHERE te.org_id = $1
           AND (te.session_id IS NOT NULL OR te.webinar_id IS NOT NULL)
           AND ($3::uuid[] IS NULL
                OR te.project_id = ANY($3)
                OR (te.webinar_id IS NOT NULL AND w.host_user_id = $6))
           AND ($4::uuid IS NULL OR te.project_id = $4)
           AND ($7::text IS NULL OR cs.kind = $7)
           AND ($8::text IS NULL
                OR t.source_language = $8
                OR (ls.id IS NOT NULL AND EXISTS (
                        SELECT 1 FROM live_participants lp
                        WHERE lp.session_id = ls.id AND lp.lang = $8)))
           AND ($9::timestamptz IS NULL
                OR COALESCE(ls.started_at, cs.started_at, w.actual_start, w.created_at) >= $9)
           AND ($10::timestamptz IS NULL
                OR COALESCE(ls.started_at, cs.started_at, w.actual_start, w.created_at) <= $10)
           AND ($11::uuid IS NULL OR te.session_id = $11 OR ls.id = $11)
           AND ($12::text IS NULL OR te.speaker_name ILIKE '%' || $12 || '%')
         ORDER BY te.embedding <=> $2
         LIMIT $5",
    )
    .bind(org_id)
    .bind(qvec)
    .bind(scope)
    .bind(project_id)
    .bind(limit)
    .bind(user_id)
    .bind(&filters.kind)
    .bind(&filters.lang)
    .bind(filters.from)
    .bind(filters.to)
    .bind(filters.session_id)
    .bind(&filters.speaker_like)
    .fetch_all(pool)
    .await
}

/// The source kind of a search hit: `"call"` when it came from a call session,
/// `"webinar"` when it came from a finished webinar. The `owner_excl` CHECK
/// (migration 050) guarantees a row is never both; a webinar owner wins if present.
pub fn result_kind(r: &ResultRow) -> &'static str {
    if r.webinar_id.is_some() {
        "webinar"
    } else {
        "call"
    }
}

/// Serialize one hit to the API shape. Split out so the call-vs-webinar mapping is
/// unit-testable without a live database.
pub fn result_json(r: &ResultRow) -> Value {
    let mut hit = json!({
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
    });
    // A Live Connect hit says what it is and which conversation to open. The keys are absent
    // for every other kind, so those hits keep exactly the shape they had.
    if let Some(live) = r.live_session_id {
        hit["live_session_id"] = json!(live);
        hit["session_kind"] = json!(r.session_kind);
    }
    hit
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
            session_kind: None,
            live_session_id: None,
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
            session_kind: None,
            live_session_id: None,
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

    fn query(f: impl FnOnce(&mut SearchQuery)) -> SearchQuery {
        let mut q = SearchQuery::default();
        f(&mut q);
        q
    }

    #[test]
    fn no_filters_is_the_search_that_existed_before() {
        let f = Filters::parse(&SearchQuery::default()).unwrap();
        assert!(f.kind.is_none() && f.lang.is_none() && f.from.is_none() && f.to.is_none());
        assert!(f.session_id.is_none() && f.speaker_like.is_none());
        // Blank values are "not given", not errors.
        let blank = query(|q| {
            q.kind = Some("  ".into());
            q.lang = Some(String::new());
            q.from = Some(" ".into());
            q.speaker = Some("".into());
        });
        let f = Filters::parse(&blank).unwrap();
        assert!(
            f.kind.is_none() && f.lang.is_none() && f.from.is_none() && f.speaker_like.is_none()
        );
    }

    #[test]
    fn each_filter_is_validated() {
        for ok in ["call", "voice_message", "phone", "live_connect"] {
            let f = Filters::parse(&query(|q| q.kind = Some(ok.into()))).unwrap();
            assert_eq!(f.kind.as_deref(), Some(ok));
        }
        assert!(Filters::parse(&query(|q| q.kind = Some("webinar".into()))).is_err());
        assert!(Filters::parse(&query(|q| q.kind = Some("CALL".into()))).is_err());

        let f = Filters::parse(&query(|q| q.lang = Some(" PT-br ".into()))).unwrap();
        assert_eq!(f.lang.as_deref(), Some("pt-br"));
        for bad in ["it_IT", "a b", "abcdefghi", "it'--"] {
            assert!(
                Filters::parse(&query(|q| q.lang = Some(bad.into()))).is_err(),
                "{bad}"
            );
        }

        let f = Filters::parse(&query(|q| {
            q.from = Some("2026-03-01T10:00:00+02:00".into());
            q.to = Some("2026-03-02T00:00:00Z".into());
        }))
        .unwrap();
        assert_eq!(f.from.unwrap().to_rfc3339(), "2026-03-01T08:00:00+00:00");
        assert!(f.to.is_some());
        for bad in ["2026-03-01", "yesterday", "2026-13-01T00:00:00Z"] {
            assert!(
                Filters::parse(&query(|q| q.from = Some(bad.into()))).is_err(),
                "{bad}"
            );
            assert!(
                Filters::parse(&query(|q| q.to = Some(bad.into()))).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_speaker_is_searched_as_text_never_as_a_pattern() {
        let f = Filters::parse(&query(|q| q.speaker = Some("  50%_off\\ ".into()))).unwrap();
        assert_eq!(f.speaker_like.as_deref(), Some("50\\%\\_off\\\\"));
        let long = "x".repeat(500);
        let f = Filters::parse(&query(|q| q.speaker = Some(long))).unwrap();
        assert_eq!(f.speaker_like.unwrap().chars().count(), 100);
    }

    #[test]
    fn a_live_hit_names_its_conversation_and_every_other_hit_is_unchanged() {
        let mut live = call_row();
        live.live_session_id = Some(Uuid::from_u128(77));
        live.session_kind = Some("live_connect".into());
        let v = result_json(&live);
        assert_eq!(v["kind"], "call", "existing consumers keep their `kind`");
        assert_eq!(v["session_kind"], "live_connect");
        assert_eq!(v["live_session_id"], Uuid::from_u128(77).to_string());
        assert_eq!(v["session_id"], Uuid::from_u128(1).to_string());
        assert_eq!(v["project_id"], Uuid::from_u128(9).to_string());

        let plain = result_json(&call_row());
        assert!(plain.get("live_session_id").is_none() && plain.get("session_kind").is_none());
        let webinar = result_json(&webinar_row());
        assert!(webinar.get("live_session_id").is_none() && webinar.get("session_kind").is_none());
    }
}
