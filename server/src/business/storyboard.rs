//! AI project storyboard (spec 0106, Phase-4).
//!
//! On demand, an admin generates a narrative of a project's trajectory: its
//! timeline and history, per-member contribution, collaboration cadence, and —
//! when a `target_workflow` is supplied — how the observed activity aligns with or
//! diverges from that ideal, with recommendations. The model is fed a COMPACT,
//! bounded summary of the project's calls (dates, durations, participants, short
//! transcript excerpts) — never every transcript verbatim. One latest storyboard
//! is kept per project (upserted), and the underlying call/participant data is
//! never deleted on team/membership churn, so a member joining or leaving loses
//! nothing. The Markdown is downloadable client-side as a shared doc.

use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::FromRow;
use uuid::Uuid;

use crate::business::{
    bad_request, credits, db_err, not_found, require_pool, require_role, ADMIN, MEMBER,
};
use crate::groq::ChatRequest;
use crate::middleware::AuthUser;
use crate::AppState;

/// Flat org-credit cost to generate (or regenerate) a project storyboard.
const STORYBOARD_CREDITS: i32 = 10;
/// Cap the number of calls fed into the prompt so the context stays bounded.
const MAX_SESSIONS: i64 = 40;

#[derive(FromRow, Serialize)]
struct Storyboard {
    project_id: Uuid,
    target_workflow: Option<String>,
    markdown: String,
    model: String,
    updated_at: DateTime<Utc>,
}

#[derive(Deserialize, Default)]
pub struct GenerateBody {
    /// Optional ideal workflow to measure the project against (free text).
    #[serde(default)]
    target_workflow: Option<String>,
    /// Output language (BCP-47-ish code the model understands); defaults to English.
    #[serde(default)]
    lang: Option<String>,
}

#[derive(FromRow)]
struct SessionRow {
    started_at: DateTime<Utc>,
    minutes: f64,
    transcript_status: String,
    participants: String,
    excerpt: String,
}

#[derive(FromRow)]
struct MemberRow {
    name: String,
    calls: i64,
}

/// `GET /api/business/organizations/{org_id}/projects/{project_id}/storyboard`
/// — the latest stored storyboard for the project (member). 404 if none yet.
pub async fn get(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, project_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;
    let row: Option<Storyboard> = sqlx::query_as(
        "SELECT project_id, target_workflow, markdown, model, updated_at
         FROM project_storyboards WHERE project_id = $1 AND org_id = $2",
    )
    .bind(project_id)
    .bind(org_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    let row = row.ok_or_else(|| not_found("no storyboard yet"))?;
    Ok(Json(row).into_response())
}

/// `POST /api/business/organizations/{org_id}/projects/{project_id}/storyboard`
/// — generate (or regenerate) the storyboard (admin). Charges a flat fee.
pub async fn generate(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, project_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<GenerateBody>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;

    // The AI stack (Groq + pricing) only exists when billing is configured.
    let ai = state
        .config
        .billing
        .as_ref()
        .map(|b| &b.ai)
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "AI not configured").into_response())?;

    // Project must belong to the org.
    let project_name: Option<String> =
        sqlx::query_scalar("SELECT name FROM projects WHERE id = $1 AND org_id = $2")
            .bind(project_id)
            .bind(org_id)
            .fetch_optional(pool)
            .await
            .map_err(db_err)?;
    let project_name = project_name.ok_or_else(|| not_found("project not found"))?;

    let target = body
        .target_workflow
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(4000).collect::<String>());
    let lang = body
        .lang
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("en")
        .to_string();

    // Compact, bounded summary of the project's activity.
    let (summary, session_count) = gather_summary(pool, project_id, &project_name).await?;
    if session_count == 0 {
        return Err(bad_request(
            "no calls in this project yet — nothing to summarize",
        ));
    }

    // Pre-check the balance so we don't spend a Groq call an org can't pay for.
    let balance: i32 =
        sqlx::query_scalar("SELECT credits_balance FROM organizations WHERE id = $1")
            .bind(org_id)
            .fetch_one(pool)
            .await
            .map_err(db_err)?;
    if balance < STORYBOARD_CREDITS {
        return Err((
            StatusCode::PAYMENT_REQUIRED,
            Json(json!({
                "error": "insufficient_org_credits",
                "balance": balance,
                "required": STORYBOARD_CREDITS,
            })),
        )
            .into_response());
    }

    // Generate the Markdown.
    let system = storyboard_prompt(&lang, target.as_deref());
    let mut req = ChatRequest::new(ai.report_model.clone(), system, summary);
    req.max_tokens = 3000;
    req.temperature = 0.4;
    req.timeout = Duration::from_secs(45);
    req.max_retries = 2;
    let fallback = ChatRequest {
        model: ai.fallback_model.clone(),
        ..req.clone()
    };
    let (markdown, model) = match state.groq.chat(req).await {
        Ok(md) => (md, ai.report_model.clone()),
        Err(e) if e.contains("groq returned 4") && ai.fallback_model != ai.report_model => {
            tracing::warn!("storyboard model failed ({e}); retrying on fallback");
            match state.groq.chat(fallback).await {
                Ok(md) => (md, ai.fallback_model.clone()),
                Err(e) => return Err(groq_err(&e)),
            }
        }
        Err(e) => return Err(groq_err(&e)),
    };

    // Charge after a successful generation (authoritative; handles a race on balance).
    match credits::deduct_org_credits(
        pool,
        org_id,
        STORYBOARD_CREDITS,
        "storyboard",
        None,
        Some(user.user_id),
        "AI project storyboard",
    )
    .await
    .map_err(db_err)?
    {
        credits::OrgCharge::Insufficient { balance, required } => {
            return Err((
                StatusCode::PAYMENT_REQUIRED,
                Json(json!({
                    "error": "insufficient_org_credits",
                    "balance": balance,
                    "required": required,
                })),
            )
                .into_response());
        }
        credits::OrgCharge::Charged { .. } => {}
    }

    // Upsert the latest storyboard for this project.
    let row: Storyboard = sqlx::query_as(
        "INSERT INTO project_storyboards (project_id, org_id, target_workflow, markdown, model, generated_by)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (project_id) DO UPDATE
            SET target_workflow = EXCLUDED.target_workflow,
                markdown = EXCLUDED.markdown,
                model = EXCLUDED.model,
                generated_by = EXCLUDED.generated_by,
                updated_at = now()
         RETURNING project_id, target_workflow, markdown, model, updated_at",
    )
    .bind(project_id)
    .bind(org_id)
    .bind(target.as_deref())
    .bind(&markdown)
    .bind(&model)
    .bind(user.user_id)
    .fetch_one(pool)
    .await
    .map_err(db_err)?;

    Ok(Json(row).into_response())
}

fn groq_err(e: &str) -> Response {
    tracing::error!("storyboard generation failed: {e}");
    (StatusCode::BAD_GATEWAY, "could not generate storyboard").into_response()
}

/// Build the compact activity summary fed to the model. Returns `(text, count)`.
async fn gather_summary(
    pool: &crate::db::Pool,
    project_id: Uuid,
    project_name: &str,
) -> Result<(String, i64), Response> {
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM call_sessions WHERE project_id = $1")
        .bind(project_id)
        .fetch_one(pool)
        .await
        .map_err(db_err)?;
    if total == 0 {
        return Ok((String::new(), 0));
    }

    // Per-member participation across the whole project (persists across churn).
    let members: Vec<MemberRow> = sqlx::query_as(
        "SELECT sp.name AS name, count(DISTINCT sp.session_id)::bigint AS calls
         FROM session_participants sp
         JOIN call_sessions cs ON cs.id = sp.session_id
         WHERE cs.project_id = $1
         GROUP BY sp.name
         ORDER BY calls DESC
         LIMIT 30",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;

    // Recent calls: date, duration, roster, transcript status + a short excerpt.
    let sessions: Vec<SessionRow> = sqlx::query_as(
        "SELECT cs.started_at,
                (EXTRACT(EPOCH FROM (COALESCE(cs.ended_at, cs.started_at) - cs.started_at)) / 60)::double precision AS minutes,
                cs.transcript_status,
                COALESCE((SELECT string_agg(DISTINCT sp.name, ', ')
                          FROM session_participants sp WHERE sp.session_id = cs.id), '') AS participants,
                COALESCE((SELECT left(string_agg(seg->>'text', ' '), 400)
                          FROM transcripts tr, jsonb_array_elements(tr.segments) seg
                          WHERE tr.session_id = cs.id), '') AS excerpt
         FROM call_sessions cs
         WHERE cs.project_id = $1
         ORDER BY cs.started_at DESC
         LIMIT $2",
    )
    .bind(project_id)
    .bind(MAX_SESSIONS)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;

    let mut s = String::new();
    s.push_str(&format!("PROJECT: {project_name}\n"));
    s.push_str(&format!("TOTAL CALLS: {total}\n"));
    if total > MAX_SESSIONS {
        s.push_str(&format!("(showing the {MAX_SESSIONS} most recent calls)\n"));
    }
    s.push_str("\nPARTICIPANTS (by calls attended):\n");
    for m in &members {
        s.push_str(&format!("- {} — {} call(s)\n", m.name, m.calls));
    }
    s.push_str("\nCALL TIMELINE (most recent first):\n");
    for row in &sessions {
        let when = row.started_at.format("%Y-%m-%d %H:%M");
        let mins = row.minutes.max(0.0).round() as i64;
        s.push_str(&format!(
            "- {when} · {mins} min · with: {} · transcript: {}",
            if row.participants.is_empty() {
                "—"
            } else {
                &row.participants
            },
            row.transcript_status
        ));
        if !row.excerpt.is_empty() {
            s.push_str(&format!(
                " · excerpt: \"{}\"",
                row.excerpt.replace('\n', " ")
            ));
        }
        s.push('\n');
    }
    Ok((s, total))
}

/// System prompt for the storyboard. The target workflow (if any) is passed as
/// DATA with explicit precedence — never interpolated into directives.
fn storyboard_prompt(lang: &str, target_workflow: Option<&str>) -> String {
    let mut p = String::from(
        "You are an analyst writing a concise, well-structured project storyboard from a \
         summary of a team's translated video calls. The input lists the project, its \
         participants (by calls attended), and a reverse-chronological timeline of calls \
         (date, duration, roster, transcript status, and short excerpts).\n\n\
         Write a Markdown document with these `##` sections (TRANSLATE every heading into \
         the output language, never keep the English label): \
         Overview, Timeline & History, Collaboration & Cadence, Per-Member Contribution, \
         Highlights, Recommendations.\n\
         - Overview: what the project appears to be about and its current momentum.\n\
         - Timeline & History: the arc of the project across the dated calls.\n\
         - Collaboration & Cadence: meeting frequency, regular vs occasional participants, gaps.\n\
         - Per-Member Contribution: a short paragraph or bullet per notable participant. \
         Treat people who stopped or recently started appearing as natural team changes — \
         never invent data about anyone not listed.\n\
         - Highlights: concrete moments grounded in the excerpts; do not fabricate.\n\
         - Recommendations: practical next steps.\n",
    );
    if target_workflow.is_some() {
        p.push_str(
            "\nAdd one more `##` section titled (translated) \"Alignment with the Target \
             Workflow\": compare the observed activity against the target workflow provided \
             below, calling out where the project follows it, where it diverges, and how to \
             close the gap.\n",
        );
    }
    p.push_str(&format!(
        "\nWrite the ENTIRE document in {}. Be specific and grounded in the data; do not \
         invent calls, names, or outcomes. Output ONLY the Markdown — no preamble.\n",
        crate::groq::lang_name(lang),
    ));
    if let Some(w) = target_workflow {
        p.push_str(&format!(
            "\nTARGET WORKFLOW (provided by the admin):\n{w}\n"
        ));
    }
    p
}
