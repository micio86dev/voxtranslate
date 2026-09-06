//! Dashboard Help Assistant WebSocket route handler.
//!
//! `GET /api/business/organizations/{org_id}/help-assistant` upgrades to a
//! WebSocket and relays between the authenticated dashboard client and the
//! OpenAI Realtime API, using a STATIC product-knowledge prompt (no RAG).
//!
//! ## Authorization
//!
//! - JWT required (via [`resolve_ws_auth`]).
//! - Org membership at ANY role (member, admin, or owner).
//!   Unlike the Voice Assistant, the Help Assistant does NOT restrict to admin/owner —
//!   all active org members may use it (spec: Help Assistant Availability Gate).
//! - Active Business/Enterprise subscription required (402 if not).
//!
//! ## Semaphore
//!
//! The relay engine holds a process-wide `Arc<Semaphore>` capped at
//! `HelpAssistantConfig::max_sessions`. If all slots are taken, the handler
//! sends a `capacity_full` error JSON and closes the WS before any OpenAI call.

use axum::extract::ws::{Message as WsMessage, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::SinkExt as _;
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::verify_jwt;
use crate::business::{db_err, require_pool, require_role, ws_gate, MEMBER};
use crate::engine::help_assistant::{run_relay, RelayDeps};
use crate::middleware::AuthUser;
use crate::AppState;

/// Query params for the help-assistant WebSocket endpoint.
#[derive(Deserialize, Default)]
pub struct HaQuery {
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
/// first and falls back to the `token` query param. Mirrors `voice_assistant::resolve_ws_auth`.
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

    let raw_token = if let Some(value) = headers.get(header::AUTHORIZATION) {
        value
            .to_str()
            .ok()
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::to_string)
    } else {
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

/// `GET /api/business/organizations/{org_id}/help-assistant` — WS upgrade.
///
/// Authorization: any authenticated org member (no role restriction beyond membership).
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(org_id): Path<Uuid>,
    Query(query): Query<HaQuery>,
) -> Result<Response, Response> {
    // Resolve the authenticated user. Browser WebSocket cannot send custom
    // headers, so the JWT may arrive as `?token=` instead of `Authorization:
    // Bearer`. `resolve_ws_auth` handles both paths transparently.
    let user = resolve_ws_auth(&state, &headers, query.token.as_deref())?;

    let pool = require_pool(&state)?;

    // 1. Auth check: any org member (MEMBER or above). Non-members → 403/404.
    //    Unlike the Voice Assistant, there is NO minimum role beyond membership.
    require_role(pool, org_id, user.user_id, MEMBER).await?;

    // 2. Help assistant config must be present (route only registered when Some,
    //    but be defensive here too).
    let ha_cfg = state
        .config
        .help_assistant
        .as_ref()
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "help assistant not configured",
            )
                .into_response()
        })?
        .clone();

    // 3. Eligibility — active Business/Enterprise subscription, then enough credits
    //    to be worth starting. Both used to be a pre-upgrade 402, which the browser
    //    throws away (see `ws_gate`): the dashboard saw a bare `error` event and
    //    could only say "connection error" for a lapsed subscription. Report it
    //    in-band instead, the way `capacity_full` already does.
    let ineligible = ws_gate::check(pool, org_id).await.map_err(db_err)?;
    if let Some(reason) = ineligible {
        tracing::info!(%org_id, code = reason.code(), "help_assistant: session refused");
        let frame = reason.to_json();
        return Ok(ws.on_upgrade(move |mut socket| async move {
            let _ = socket.send(WsMessage::Text(frame.into())).await;
            let _ = socket.close().await;
        }));
    }

    // 5. Semaphore handle — process-wide cap from AppState.
    let semaphore = state
        .help_assistant_semaphore
        .as_ref()
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "help assistant not configured",
            )
                .into_response()
        })?
        .clone();

    let deps = RelayDeps {
        config: ha_cfg,
        semaphore,
        pool: pool.clone(),
        org_id,
        user_id: user.user_id,
    };

    // 6. WS upgrade — the relay runs in a spawned task.
    Ok(ws.on_upgrade(move |socket| async move {
        run_relay(socket, deps).await;
    }))
}
