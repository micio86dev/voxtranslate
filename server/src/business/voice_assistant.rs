//! B2B Voice Assistant WebSocket route handler.
//!
//! `GET /api/business/organizations/{org_id}/voice-assistant` upgrades to a
//! WebSocket and relays between the authenticated dashboard client and the
//! OpenAI Realtime API, grounded by RAG over the org's transcript embeddings.
//!
//! ## Authorization
//!
//! - JWT required (via [`AuthUser`]).
//! - Org membership with **owner** or **lead** role (member → 403). This mirrors
//!   the insights endpoint (only team leads and owners may query transcripts).
//! - Active Business/Enterprise subscription required (via
//!   [`credits::org_subscription_active`]).
//!
//! ## Semaphore
//!
//! The relay engine holds a process-wide [`Arc<Semaphore>`] capped at
//! [`VoiceAssistantConfig::max_sessions`]. If all slots are taken, the handler
//! sends a `capacity_full` error and closes the WS before any OpenAI call.

use axum::extract::ws::{Message as WsMessage, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::SinkExt as _;
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::verify_jwt;
use crate::business::{db_err, forbidden, require_pool, require_role, role_rank, ws_gate};
use crate::engine::voice_assistant::{run_relay, RelayDeps};
use crate::middleware::AuthUser;
use crate::AppState;

/// Query params for the voice-assistant WebSocket endpoint.
#[derive(Deserialize, Default)]
pub struct VaQuery {
    /// Optional project scope — RAG retrieval is narrowed to this project.
    #[serde(default)]
    pub project_id: Option<Uuid>,
    /// Optional member scope — RAG retrieval is narrowed to sessions this member
    /// participated in.
    #[serde(default)]
    pub member_id: Option<Uuid>,
    /// JWT passed as a query parameter — used by browser WebSocket connections
    /// that cannot set custom headers. Accepted only when the `Authorization`
    /// header is absent. The token is validated with the same logic as
    /// [`crate::middleware::AuthUser`].
    #[serde(default)]
    pub token: Option<String>,
}

/// Resolve an [`AuthUser`] for a WebSocket upgrade request.
///
/// Browser `WebSocket` cannot send custom headers, so dashboards pass the JWT
/// as `?token=<jwt>`. This helper tries the `Authorization: Bearer` header
/// first and falls back to the `token` query param.
fn resolve_ws_auth(
    state: &AppState,
    headers: &HeaderMap,
    query_token: Option<&str>,
) -> Result<AuthUser, Response> {
    let billing = state
        .config
        .billing
        .as_ref()
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, "unauthorized").into_response())?;

    // Prefer the Authorization header (non-browser callers, curl, tests).
    let raw_token = if let Some(value) = headers.get(header::AUTHORIZATION) {
        value
            .to_str()
            .ok()
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::to_string)
    } else {
        // Fallback: browser WebSocket sends JWT as ?token=
        query_token.map(str::to_string)
    };

    let token =
        raw_token.ok_or_else(|| (StatusCode::UNAUTHORIZED, "unauthorized").into_response())?;

    let claims = verify_jwt(&billing.jwt_secret, &token)
        .map_err(|_| (StatusCode::UNAUTHORIZED, "unauthorized").into_response())?;
    let user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| (StatusCode::UNAUTHORIZED, "unauthorized").into_response())?;

    Ok(AuthUser {
        user_id,
        email: claims.email,
        name: claims.name,
    })
}

/// Top-K transcript chunks injected into the session instructions.
const RAG_TOP_K: i64 = 10;

/// Minimum role rank that may open a voice-assistant session.
/// `OWNER` = 3, `ADMIN` = 2. We require admin-level (owner or admin/lead).
/// In this codebase "lead" maps to the `admin` role rank (role_rank("admin") >= ADMIN).
/// We use OWNER here but check >= ADMIN (i.e. admin or owner) — see code below.
const VA_MIN_RANK: u8 = crate::business::ADMIN;

/// `GET /api/business/organizations/{org_id}/voice-assistant` — WS upgrade.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(org_id): Path<Uuid>,
    Query(query): Query<VaQuery>,
) -> Result<Response, Response> {
    // Resolve the authenticated user. Browser WebSocket cannot send custom
    // headers, so the JWT may arrive as `?token=` instead of `Authorization:
    // Bearer`. `resolve_ws_auth` handles both paths transparently.
    let user = resolve_ws_auth(&state, &headers, query.token.as_deref())?;

    let pool = require_pool(&state)?;

    // 1. Auth + role check: owner or admin (lead) only; members → 403.
    let role = require_role(pool, org_id, user.user_id, VA_MIN_RANK).await?;

    // Owners have rank 3, admins have rank 2. We require admin-or-owner.
    // member (rank 1) is already rejected by require_role above.
    // Extra guard: if somehow rank < ADMIN, 403 (defense in depth).
    if role_rank(&role) < VA_MIN_RANK {
        return Err(forbidden(
            "voice assistant requires team lead or owner role",
        ));
    }

    // 2. Voice assistant config must be present (route should only be registered
    //    when it is, but be defensive here too).
    let va_cfg = state
        .config
        .voice_assistant
        .as_ref()
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "voice assistant not configured",
            )
                .into_response()
        })?
        .clone();

    // 3. Embeddings must be configured for RAG.
    let embedder = state.embeddings.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "voice assistant not configured (no embeddings provider)",
        )
            .into_response()
    })?;

    // 4. Eligibility — active Business/Enterprise subscription, then enough credits.
    //    Reported in-band after the upgrade rather than as a 402: a browser cannot
    //    read the status of a failed WebSocket handshake, so a pre-upgrade rejection
    //    reaches the dashboard as an unexplained "connection error" (see `ws_gate`).
    let ineligible = ws_gate::check(pool, org_id).await.map_err(db_err)?;
    if let Some(reason) = ineligible {
        tracing::info!(%org_id, code = reason.code(), "voice_assistant: session refused");
        let frame = reason.to_json();
        return Ok(ws.on_upgrade(move |mut socket| async move {
            let _ = socket.send(WsMessage::Text(frame.into())).await;
            let _ = socket.close().await;
        }));
    }

    // 5. Retrieve org name (for RAG instructions).
    let org_name: Option<String> =
        sqlx::query_scalar("SELECT name FROM organizations WHERE id = $1")
            .bind(org_id)
            .fetch_optional(pool)
            .await
            .map_err(db_err)?;
    let org_name = org_name.unwrap_or_else(|| "your organization".to_string());

    // 6. Resolve project and member names for RAG focus (best-effort).
    let project_name: Option<String> = if let Some(pid) = query.project_id {
        sqlx::query_scalar("SELECT name FROM projects WHERE id = $1 AND org_id = $2")
            .bind(pid)
            .bind(org_id)
            .fetch_optional(pool)
            .await
            .map_err(db_err)?
    } else {
        None
    };
    let member_name: Option<String> = if let Some(mid) = query.member_id {
        sqlx::query_scalar(
            "SELECT u.name FROM users u
             JOIN organization_members om ON om.user_id = u.id
             WHERE u.id = $1 AND om.org_id = $2",
        )
        .bind(mid)
        .bind(org_id)
        .fetch_optional(pool)
        .await
        .map_err(db_err)?
    } else {
        None
    };

    // 7. RAG retrieval — embed a context seed and KNN over transcript_embeddings.
    //    The seed query describes what the user is likely to ask about.
    let seed_query =
        build_rag_seed_query(&org_name, project_name.as_deref(), member_name.as_deref());
    let rag_chunks = retrieve_rag_chunks(
        pool,
        embedder,
        org_id,
        query.project_id,
        query.member_id,
        &seed_query,
    )
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(
            %org_id,
            "voice_assistant: RAG retrieval failed (continuing with empty KB): {e}"
        );
        vec![]
    });

    // 8. Semaphore is held in the engine's Arc<Semaphore>. We need a handle here.
    //    The engine's semaphore is stored on AppState via the engine registry.
    //    For the voice assistant we keep a dedicated Arc<Semaphore> on AppState
    //    (see lib.rs) but for now we read it from the config and construct it
    //    per-request — the actual cap is enforced at `try_acquire` inside `run_relay`.
    //
    //    TODO (follow-up): store the semaphore on AppState so it's truly process-wide.
    //    For this PR we gate on it via the route-level semaphore passed through RelayDeps.
    let semaphore = state
        .voice_assistant_semaphore
        .as_ref()
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "voice assistant not configured",
            )
                .into_response()
        })?
        .clone();

    let deps = RelayDeps {
        config: va_cfg,
        semaphore,
        pool: pool.clone(),
        embedder: embedder.clone(),
        org_id,
        user_id: user.user_id,
        project_id: query.project_id,
        member_id: query.member_id,
        org_name,
        project_name,
        member_name,
        rag_chunks,
    };

    // 9. WS upgrade — the relay runs in a spawned task.
    Ok(ws.on_upgrade(move |socket| async move {
        run_relay(socket, deps).await;
    }))
}

/// Build the embedding seed query for RAG retrieval.
///
/// A generic summary of the org/project/member scope so the KNN retrieves
/// broadly relevant chunks before any specific user question is known.
fn build_rag_seed_query(org: &str, project: Option<&str>, member: Option<&str>) -> String {
    let mut q = format!("organization {org} call transcripts key topics discussions decisions");
    if let Some(p) = project {
        q.push_str(&format!(" project {p}"));
    }
    if let Some(m) = member {
        q.push_str(&format!(" team member {m} contributions feedback"));
    }
    q
}

/// KNN retrieval over `transcript_embeddings` scoped to the org (and optionally
/// a project or member). Returns up to [`RAG_TOP_K`] formatted text chunks.
pub async fn retrieve_rag_chunks(
    pool: &crate::db::Pool,
    embedder: &crate::embeddings::OpenAiEmbeddings,
    org_id: Uuid,
    project_id: Option<Uuid>,
    member_id: Option<Uuid>,
    seed_query: &str,
) -> Result<Vec<String>, String> {
    let vector = embedder
        .embed_one(seed_query)
        .await
        .map_err(|e| format!("embedding failed: {e}"))?;
    let qvec = pgvector::Vector::from(vector);

    let rows: Vec<(Option<String>, Option<String>, String)> = sqlx::query_as(
        "SELECT p.name AS project_name, te.speaker_name, te.content
         FROM transcript_embeddings te
         LEFT JOIN projects p ON p.id = te.project_id
         WHERE te.org_id = $1
           AND ($2::uuid IS NULL OR te.project_id = $2)
           AND ($3::uuid IS NULL OR te.session_id IN (
                 SELECT sp.session_id FROM session_participants sp WHERE sp.user_id = $3))
         ORDER BY te.embedding <=> $4
         LIMIT $5",
    )
    .bind(org_id)
    .bind(project_id)
    .bind(member_id)
    .bind(qvec)
    .bind(RAG_TOP_K)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("knn query failed: {e}"))?;

    let chunks = rows
        .into_iter()
        .map(|(project_name, speaker_name, content)| {
            let proj = project_name.as_deref().unwrap_or("—");
            let spk = speaker_name.as_deref().unwrap_or("Speaker");
            format!("[{proj} · {spk}] {content}")
        })
        .collect();

    Ok(chunks)
}
