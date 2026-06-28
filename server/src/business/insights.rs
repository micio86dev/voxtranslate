//! Admin "team & project insights" assistant — the first org/business-scoped AI
//! feature.
//!
//! `POST /api/business/organizations/{org_id}/insights` lets a **team lead** (or org
//! **owner**) ask a natural-language question, or generate a structured report, about
//! a project ("how is it going?") or a team member ("how are they doing, where can
//! they improve?"). It grounds the answer in transcript CONTENT (semantic retrieval
//! over `transcript_embeddings`, reusing the search engine) plus activity METRICS
//! (reusing the analytics SQL), then synthesizes with Groq.
//!
//! Authorization (enforced here, never client-side):
//!   - **owner** → the whole org;
//!   - **lead** of ≥1 team → only members of led teams + the projects those members
//!     created or participated in;
//!   - anyone else → 403.
//!
//! An out-of-scope target id yields 403 (no leak).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::FromRow;
use uuid::Uuid;

use crate::business::{
    audit, bad_request, db_err, forbidden, require_pool, require_role, role_rank, MEMBER, OWNER,
};
use crate::groq::ChatRequest;
use crate::middleware::AuthUser;
use crate::AppState;

/// Top transcript chunks fed to the model — bounded so the synthesis is a single
/// LLM call (no proxy-timeout risk).
const TOP_K: i64 = 15;

#[derive(Deserialize)]
pub struct InsightRequest {
    /// "qa" | "project_report" | "member_report".
    mode: String,
    #[serde(default)]
    question: Option<String>,
    #[serde(default)]
    project_id: Option<Uuid>,
    #[serde(default)]
    member_id: Option<Uuid>,
}

/// What the caller may reach. `All` = owner (whole org).
enum Scope {
    All,
    Led {
        project_ids: Vec<Uuid>,
        member_ids: Vec<Uuid>,
    },
}

#[derive(FromRow)]
struct ChunkRow {
    session_id: Uuid,
    project_name: Option<String>,
    speaker_name: Option<String>,
    content: String,
    room: String,
}

fn svc_unavailable(msg: &str) -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, msg.to_string()).into_response()
}

/// `POST /api/business/organizations/{org_id}/insights`.
pub async fn generate(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Json(body): Json<InsightRequest>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let role = require_role(pool, org_id, user.user_id, MEMBER).await?;
    let cfg = state
        .config
        .billing
        .as_ref()
        .ok_or_else(|| svc_unavailable("business not configured"))?;
    let embedder = state
        .embeddings
        .as_ref()
        .ok_or_else(|| svc_unavailable("insights not configured (no embeddings provider)"))?;

    let is_owner = role_rank(&role) >= OWNER;
    let scope = resolve_scope(pool, org_id, user.user_id, is_owner).await?;

    // Resolve + validate the target, and build the retrieval query text + a label.
    let mode = body.mode.as_str();
    let (query_text, target_label) = match mode {
        "qa" => {
            let q = body
                .question
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| bad_request("question is required for qa"))?;
            if let Some(pid) = body.project_id {
                ensure_project_in_scope(pool, org_id, pid, &scope).await?;
            }
            if let Some(mid) = body.member_id {
                ensure_member_in_scope(pool, org_id, mid, &scope).await?;
            }
            (q.to_string(), "question".to_string())
        }
        "project_report" => {
            let pid = body
                .project_id
                .ok_or_else(|| bad_request("project_id is required for project_report"))?;
            let name = ensure_project_in_scope(pool, org_id, pid, &scope).await?;
            (
                "Overall status, progress, momentum, risks, blockers and quality of this \
                 project based on its calls"
                    .to_string(),
                format!("project \"{name}\""),
            )
        }
        "member_report" => {
            let mid = body
                .member_id
                .ok_or_else(|| bad_request("member_id is required for member_report"))?;
            let name = ensure_member_in_scope(pool, org_id, mid, &scope).await?;
            (
                "This member's contributions, communication quality, collaboration, \
                 strengths, and concrete areas to improve and how"
                    .to_string(),
                format!("member \"{name}\""),
            )
        }
        _ => {
            return Err(bad_request(
                "mode must be qa | project_report | member_report",
            ))
        }
    };

    // --- Retrieve grounding transcript chunks (semantic) ---
    let vector = embedder.embed_one(&query_text).await.map_err(|e| {
        tracing::error!("insight query embedding failed: {e}");
        (StatusCode::BAD_GATEWAY, "embedding failed").into_response()
    })?;
    let qvec = pgvector::Vector::from(vector);
    let scope_projects: Option<&[Uuid]> = match &scope {
        Scope::All => None,
        Scope::Led { project_ids, .. } => Some(project_ids),
    };
    let chunks: Vec<ChunkRow> = sqlx::query_as(
        "SELECT te.session_id, p.name AS project_name, te.speaker_name, te.content, cs.room
         FROM transcript_embeddings te
         JOIN call_sessions cs ON cs.id = te.session_id
         LEFT JOIN projects p  ON p.id = te.project_id
         WHERE te.org_id = $1
           AND ($2::uuid[] IS NULL OR te.project_id = ANY($2))
           AND ($3::uuid IS NULL OR te.project_id = $3)
           AND ($4::uuid IS NULL OR te.session_id IN (
                 SELECT sp.session_id FROM session_participants sp WHERE sp.user_id = $4))
         ORDER BY te.embedding <=> $5
         LIMIT $6",
    )
    .bind(org_id)
    .bind(scope_projects)
    .bind(body.project_id)
    .bind(body.member_id)
    .bind(qvec)
    .bind(TOP_K)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;

    // --- Gather activity metrics (best-effort; never fail the insight) ---
    let metrics = gather_metrics(pool, org_id, mode, body.project_id, body.member_id).await;

    // --- Synthesize ---
    let system = system_prompt(mode);
    let user_content = build_user_content(mode, &query_text, &target_label, &metrics, &chunks);
    let ai = &cfg.ai;
    let make_req = |model: &str| {
        let mut r = ChatRequest::new(model, system.clone(), user_content.clone());
        r.max_tokens = 1500;
        r.temperature = 0.3;
        r.timeout = std::time::Duration::from_secs(60);
        r.max_retries = 1;
        r
    };
    let answer = match state.groq.chat(make_req(&ai.report_model)).await {
        Ok(a) => a,
        // Primary model decommissioned / rejected → one retry on the fallback model
        // (same policy as ai/report.rs).
        Err(_) => state
            .groq
            .chat(make_req(&ai.fallback_model))
            .await
            .map_err(|e| {
                tracing::error!("insight synthesis failed: {e}");
                (StatusCode::BAD_GATEWAY, "synthesis failed").into_response()
            })?,
    };

    // Compliance trail — log the shape, never the raw question.
    audit::log_audit_event(
        pool,
        org_id,
        user.user_id,
        "insight.generate",
        "insight",
        org_id,
        json!({
            "mode": mode,
            "project_id": body.project_id,
            "member_id": body.member_id,
            "chunks": chunks.len(),
        }),
    );

    let sources: Vec<Value> = chunks
        .iter()
        .map(|c| {
            json!({
                "session_id": c.session_id,
                "room": c.room,
                "project_name": c.project_name,
                "speaker_name": c.speaker_name,
                "snippet": c.content,
            })
        })
        .collect();

    Ok(Json(json!({
        "answer_markdown": answer,
        "sources": sources,
        "model": ai.report_model,
    }))
    .into_response())
}

// ---- Scope resolution --------------------------------------------------------

async fn resolve_scope(
    pool: &crate::db::Pool,
    org_id: Uuid,
    user_id: Uuid,
    is_owner: bool,
) -> Result<Scope, Response> {
    if is_owner {
        return Ok(Scope::All);
    }
    // Teams the caller leads.
    let led: Vec<Uuid> = sqlx::query_scalar(
        "SELECT tm.team_id FROM team_members tm
         JOIN teams t ON t.id = tm.team_id
         WHERE t.org_id = $1 AND t.archived_at IS NULL
           AND tm.user_id = $2 AND tm.role = 'lead'",
    )
    .bind(org_id)
    .bind(user_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    if led.is_empty() {
        return Err(forbidden(
            "insights are available to team leads and owners only",
        ));
    }
    // Members of those teams, and the projects those members created or participated in.
    let member_ids: Vec<Uuid> =
        sqlx::query_scalar("SELECT DISTINCT user_id FROM team_members WHERE team_id = ANY($1)")
            .bind(&led)
            .fetch_all(pool)
            .await
            .map_err(db_err)?;
    let project_ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM projects
         WHERE org_id = $1 AND archived_at IS NULL
           AND (created_by = ANY($2)
                OR id IN (
                    SELECT DISTINCT cs.project_id FROM call_sessions cs
                    JOIN session_participants sp ON sp.session_id = cs.id
                    WHERE cs.org_id = $1 AND sp.user_id = ANY($2) AND cs.project_id IS NOT NULL
                ))",
    )
    .bind(org_id)
    .bind(&member_ids)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    Ok(Scope::Led {
        project_ids,
        member_ids,
    })
}

/// Validate a project target is within scope; returns its name. Out-of-scope ⇒ 403.
async fn ensure_project_in_scope(
    pool: &crate::db::Pool,
    org_id: Uuid,
    project_id: Uuid,
    scope: &Scope,
) -> Result<String, Response> {
    if let Scope::Led { project_ids, .. } = scope {
        if !project_ids.contains(&project_id) {
            return Err(forbidden("project is outside your team scope"));
        }
    }
    let name: Option<String> =
        sqlx::query_scalar("SELECT name FROM projects WHERE id = $1 AND org_id = $2")
            .bind(project_id)
            .bind(org_id)
            .fetch_optional(pool)
            .await
            .map_err(db_err)?;
    name.ok_or_else(|| forbidden("project is outside your team scope"))
}

/// Validate a member target is within scope; returns their name. Out-of-scope ⇒ 403.
async fn ensure_member_in_scope(
    pool: &crate::db::Pool,
    org_id: Uuid,
    member_id: Uuid,
    scope: &Scope,
) -> Result<String, Response> {
    if let Scope::Led { member_ids, .. } = scope {
        if !member_ids.contains(&member_id) {
            return Err(forbidden("member is outside your team scope"));
        }
    }
    // Must be an org member regardless.
    let name: Option<String> = sqlx::query_scalar(
        "SELECT u.name FROM users u
         JOIN organization_members om ON om.user_id = u.id
         WHERE u.id = $1 AND om.org_id = $2",
    )
    .bind(member_id)
    .bind(org_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    name.ok_or_else(|| forbidden("member is outside your team scope"))
}

// ---- Metrics gathering (best-effort) -----------------------------------------

async fn gather_metrics(
    pool: &crate::db::Pool,
    org_id: Uuid,
    mode: &str,
    project_id: Option<Uuid>,
    member_id: Option<Uuid>,
) -> Value {
    let mut m = json!({});
    if let Some(pid) = project_id.filter(|_| mode != "member_report") {
        if let Ok(row) = sqlx::query_as::<_, (i64, i64, i64)>(
            "SELECT count(*)::bigint,
                    COALESCE(SUM(EXTRACT(EPOCH FROM (COALESCE(cs.ended_at, now()) - cs.started_at)) / 60), 0)::bigint,
                    count(*) FILTER (WHERE cs.transcript_status = 'ready')::bigint
             FROM call_sessions cs
             WHERE cs.org_id = $1 AND cs.project_id = $2
               AND cs.started_at >= now() - make_interval(days => 90)",
        )
        .bind(org_id)
        .bind(pid)
        .fetch_one(pool)
        .await
        {
            m["project"] = json!({
                "calls_90d": row.0, "minutes_90d": row.1, "transcripts_ready_90d": row.2,
            });
        }
        if let Ok(parts) = sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT u.name FROM session_participants sp
             JOIN call_sessions cs ON cs.id = sp.session_id
             JOIN users u ON u.id = sp.user_id
             WHERE cs.org_id = $1 AND cs.project_id = $2 LIMIT 20",
        )
        .bind(org_id)
        .bind(pid)
        .fetch_all(pool)
        .await
        {
            m["project"]["participants"] = json!(parts);
        }
    }
    if let Some(mid) = member_id.filter(|_| mode != "project_report") {
        if let Ok(row) = sqlx::query_as::<_, (i64, i64)>(
            "SELECT count(DISTINCT sp.session_id)::bigint,
                    COALESCE(SUM(EXTRACT(EPOCH FROM (COALESCE(sp.left_at, cs.ended_at, now()) - sp.joined_at)) / 60), 0)::bigint
             FROM session_participants sp JOIN call_sessions cs ON cs.id = sp.session_id
             WHERE cs.org_id = $1 AND sp.user_id = $2
               AND sp.joined_at >= now() - make_interval(days => 90)",
        )
        .bind(org_id)
        .bind(mid)
        .fetch_one(pool)
        .await
        {
            m["member"] = json!({ "calls_90d": row.0, "minutes_in_calls_90d": row.1 });
        }
        if let Ok(collab) = sqlx::query_as::<_, (String, i64)>(
            "SELECT other.name, count(DISTINCT other.session_id)::bigint
             FROM session_participants me
             JOIN session_participants other
               ON other.session_id = me.session_id AND other.user_id IS DISTINCT FROM me.user_id
             JOIN call_sessions cs ON cs.id = me.session_id
             WHERE cs.org_id = $1 AND me.user_id = $2
               AND me.joined_at >= now() - make_interval(days => 90)
             GROUP BY other.name ORDER BY count(DISTINCT other.session_id) DESC, other.name LIMIT 8",
        )
        .bind(org_id)
        .bind(mid)
        .fetch_all(pool)
        .await
        {
            m["member"]["top_collaborators"] =
                json!(collab.into_iter().map(|(n, c)| json!({ "name": n, "calls": c })).collect::<Vec<_>>());
        }
    }
    m
}

// ---- Prompt building (pure — unit-tested) ------------------------------------

fn system_prompt(mode: &str) -> String {
    let base = "You are an analyst assisting a B2B team lead. You are given (a) excerpts from \
        call transcripts and (b) activity metrics. Ground EVERY claim in the provided evidence — \
        never invent facts beyond it; if the evidence is thin, say so. Be fair, constructive and \
        specific. When suggesting improvements, give concrete, actionable steps (the 'how', not \
        just the 'what'). Output GitHub-flavored Markdown. Do not include private data that isn't \
        in the context.";
    let tail = match mode {
        "project_report" => {
            " Produce a concise project report with sections: Status, \
            Momentum & trends, Risks/blockers, What's going well, Recommended next steps."
        }
        "member_report" => {
            " Produce a fair, supportive performance overview with sections: \
            Contributions, Strengths, Areas to improve (with concrete how-to steps), Suggested \
            next steps. Focus on growth, not judgement."
        }
        _ => " Answer the question directly and concisely, citing what the evidence shows.",
    };
    format!("{base}{tail}")
}

fn build_user_content(
    mode: &str,
    query_text: &str,
    target_label: &str,
    metrics: &Value,
    chunks: &[ChunkRow],
) -> String {
    let mut s = String::new();
    if mode == "qa" {
        s.push_str(&format!(
            "QUESTION (about {target_label}):\n{query_text}\n\n"
        ));
    } else {
        s.push_str(&format!("TASK: {query_text} — for {target_label}.\n\n"));
    }
    s.push_str("ACTIVITY METRICS (last 90 days):\n");
    s.push_str(&serde_json::to_string_pretty(metrics).unwrap_or_else(|_| "{}".into()));
    s.push_str("\n\nTRANSCRIPT EXCERPTS (most relevant):\n");
    if chunks.is_empty() {
        s.push_str("(no transcript excerpts available for this scope)\n");
    } else {
        for c in chunks {
            let proj = c.project_name.as_deref().unwrap_or("—");
            let spk = c.speaker_name.as_deref().unwrap_or("Speaker");
            s.push_str(&format!("[{proj} · {} · {spk}] {}\n", c.room, c.content));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(content: &str) -> ChunkRow {
        ChunkRow {
            session_id: Uuid::nil(),
            project_name: Some("Acme".into()),
            speaker_name: Some("Alice".into()),
            content: content.into(),
            room: "room-1".into(),
        }
    }

    #[test]
    fn system_prompt_varies_by_mode_and_stays_grounded() {
        assert!(system_prompt("project_report").contains("project report"));
        assert!(system_prompt("member_report").contains("Areas to improve"));
        assert!(system_prompt("qa").contains("Answer the question"));
        // Anti-hallucination instruction present in every mode.
        for m in ["qa", "project_report", "member_report"] {
            assert!(system_prompt(m).contains("never invent facts"));
        }
    }

    #[test]
    fn user_content_packs_question_metrics_and_excerpts() {
        let metrics = json!({ "project": { "calls_90d": 3 } });
        let chunks = vec![chunk("we discussed pricing")];
        let out = build_user_content(
            "qa",
            "How is Acme going?",
            "project \"Acme\"",
            &metrics,
            &chunks,
        );
        assert!(out.contains("QUESTION"));
        assert!(out.contains("How is Acme going?"));
        assert!(out.contains("calls_90d"));
        assert!(out.contains("we discussed pricing"));
        assert!(out.contains("Acme · room-1 · Alice"));
    }

    #[test]
    fn user_content_handles_empty_excerpts() {
        let out = build_user_content("member_report", "x", "member \"Bob\"", &json!({}), &[]);
        assert!(out.contains("no transcript excerpts available"));
        assert!(out.contains("TASK:"));
    }
}
