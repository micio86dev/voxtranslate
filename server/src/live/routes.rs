//! Live Connect REST: create, join, read and end a session.
//!
//! Mounted only when the feature is enabled, so switching it off is a real kill switch — the
//! routes are absent and a request 404s.
//!
//! Two kinds of caller, two credentials:
//! * **Creating** a session needs a signed-in account (`AuthUser`): somebody has to pay.
//! * **Everything after** — reading, ending, and the WebSocket — uses the Live Connect token
//!   issued on create/join, so a person with no account is a full participant and nobody's
//!   account JWT ever travels in a URL.
//!
//! Errors carry a stable machine code (`{"code","message"}`) because the page routes on it:
//! `session_full` is a different screen from `invite_expired`.
#![allow(clippy::result_large_err)]

use std::time::Duration;

use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::sessions::{self, AuthError, CreateError, JoinError, NewSession, Owner, PeerAuth};
use super::tokens::{self, GuestClaims, PeerRole};
use crate::business::{db_err, require_pool, require_role, ws_gate, MEMBER};
use crate::middleware::AuthUser;
use crate::AppState;

/// Path the clients open the WebSocket on.
pub const WS_PATH: &str = "/ws/live";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/live/sessions", post(create))
        .route("/api/live/join", post(join))
        .route("/api/live/sessions/{id}", get(get_session))
        .route("/api/live/sessions/{id}/end", post(end))
        .route(WS_PATH, get(super::ws::ws_live))
}

// ---- helpers ---------------------------------------------------------------------------

pub fn api_err(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({ "code": code, "message": message }))).into_response()
}

fn disabled() -> Response {
    api_err(
        StatusCode::NOT_FOUND,
        "live_disabled",
        "Live Connect is not available",
    )
}

/// Refuse up front when the default engine cannot open the two upstream sessions a
/// conversation needs. Standard never reports `AtCapacity`; it silently skips a direction,
/// which the user experiences as one person who cannot be heard. A snapshot, not a
/// reservation — but it turns the common case into an honest "busy, try again".
fn engine_has_room(state: &AppState) -> bool {
    state
        .engines
        .default()
        .free_capacity()
        .is_none_or(|free| free >= 2)
}

fn busy() -> Response {
    api_err(
        StatusCode::SERVICE_UNAVAILABLE,
        "live_busy",
        "All translation lines are busy right now. Please try again in a moment.",
    )
}

fn rate_limited() -> Response {
    api_err(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        "Too many requests",
    )
}

fn token_for(state: &AppState, auth_session: &sessions::Created, role: PeerRole) -> Option<String> {
    sessions::issue_token(
        &state.live.config,
        auth_session.session_id,
        auth_session.host_peer,
        role,
        auth_session.created_at,
    )
}

// ---- the participant credential --------------------------------------------------------

/// A verified Live Connect token: `Authorization: Bearer <token>`.
pub struct LiveAuth(pub GuestClaims);

impl FromRequestParts<AppState> for LiveAuth {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let keys = state.live.config.keys.as_ref().ok_or_else(disabled)?;
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| api_err(StatusCode::UNAUTHORIZED, "unauthorized", "missing token"))?;
        keys.verify_guest_token(token, Utc::now().timestamp())
            .map(LiveAuth)
            .map_err(|e| api_err(StatusCode::UNAUTHORIZED, e.code(), "invalid token"))
    }
}

/// The token must name the session in the path, and the seat must still be open.
async fn authorize(
    state: &AppState,
    claims: &GuestClaims,
    path_id: Uuid,
) -> Result<PeerAuth, Response> {
    let pool = require_pool(state)?;
    if claims.sid != path_id {
        return Err(api_err(
            StatusCode::FORBIDDEN,
            "wrong_session",
            "token is for another session",
        ));
    }
    sessions::authorize_peer(pool, claims)
        .await
        .map_err(|AuthError::Gone| {
            api_err(
                StatusCode::GONE,
                "session_ended",
                "this conversation has ended",
            )
        })
}

// ---- create ----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateBody {
    /// The creator's own language.
    pub lang: String,
    /// B2C opt-in. A business session always keeps its transcript (org retention applies).
    #[serde(default)]
    pub save_transcript: bool,
    /// Present for a business session; the org pays and the project is mandatory.
    pub org_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
}

pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<CreateBody>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let cfg = &state.live.config;

    let lang = sessions::normalize_lang(&body.lang).ok_or_else(|| {
        api_err(
            StatusCode::BAD_REQUEST,
            "bad_lang",
            "unsupported language code",
        )
    })?;

    if !state.rate_limiter.allow(
        &format!("livecreate:{}", user.user_id),
        20,
        Duration::from_secs(60),
    ) {
        return Err(rate_limited());
    }
    if !engine_has_room(&state) {
        return Err(busy());
    }

    let owner = match body.org_id {
        None => {
            let billing = state.billing.as_ref().ok_or_else(|| {
                api_err(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "billing_unavailable",
                    "billing not configured",
                )
            })?;
            if !billing.can_join(user.user_id).await.map_err(db_err)? {
                return Err(api_err(
                    StatusCode::PAYMENT_REQUIRED,
                    "insufficient_balance",
                    "Add credits to start a conversation.",
                ));
            }
            Owner::User(user.user_id)
        }
        Some(org_id) => {
            require_role(pool, org_id, user.user_id, MEMBER).await?;
            if let Some(why) = ws_gate::check(pool, org_id).await.map_err(db_err)? {
                return Err(api_err(
                    StatusCode::PAYMENT_REQUIRED,
                    why.code(),
                    "organization is not eligible",
                ));
            }
            let project_id = body.project_id.ok_or_else(|| {
                api_err(
                    StatusCode::BAD_REQUEST,
                    "project_required",
                    "a business conversation belongs to a project",
                )
            })?;
            // The project must belong to THIS org and still be live: a project id from
            // another tenant, or an archived one, is indistinguishable from a missing one.
            let ok: Option<i32> = sqlx::query_scalar(
                "SELECT 1 FROM projects WHERE id = $1 AND org_id = $2 AND archived_at IS NULL",
            )
            .bind(project_id)
            .bind(org_id)
            .fetch_optional(pool)
            .await
            .map_err(db_err)?;
            if ok.is_none() {
                return Err(api_err(
                    StatusCode::NOT_FOUND,
                    "project_not_found",
                    "project not found",
                ));
            }
            Owner::Org { org_id, project_id }
        }
    };

    let save_transcript = body.save_transcript || matches!(owner, Owner::Org { .. });
    let created = sessions::create_session(
        pool,
        cfg,
        NewSession {
            owner,
            host_user: user.user_id,
            host_name: &user.name,
            host_lang: &lang,
            save_transcript,
            engine_id: None,
        },
    )
    .await
    .map_err(|e| match e {
        CreateError::TooManySessions => api_err(
            StatusCode::TOO_MANY_REQUESTS,
            "too_many_sessions",
            "You have too many open conversations. End one first.",
        ),
        CreateError::Db(e) => db_err(e),
    })?;

    let token = token_for(&state, &created, PeerRole::Host).ok_or_else(disabled)?;
    sessions::store_token_hash(pool, created.host_peer, &token).await;
    sessions::record_event(
        pool,
        created.session_id,
        "created",
        json!({ "b2b": body.org_id.is_some() }),
    )
    .await;

    let invite_url = format!(
        "{}/lc/{}",
        state.config.app_base_url.trim_end_matches('/'),
        created.invite_code
    );
    Ok(Json(json!({
        "session_id": created.session_id,
        "invite_url": invite_url,
        // The QR encodes exactly the link; the client renders it.
        "qr_payload": invite_url,
        "expires_at": created.expires_at,
        "peer_id": created.host_peer,
        "token": token,
        "ws_path": WS_PATH,
    }))
    .into_response())
}

// ---- join ------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct JoinBody {
    pub code: String,
    pub name: String,
    pub lang: String,
    /// The guest accepted the notice (no account, so this is where consent is taken).
    pub consent: bool,
}

pub async fn join(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<JoinBody>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let ip = crate::observability::client_ip(&headers);
    if !state
        .rate_limiter
        .allow(&format!("livejoin:{ip}"), 30, Duration::from_secs(60))
    {
        return Err(rate_limited());
    }
    if !body.consent {
        return Err(api_err(
            StatusCode::BAD_REQUEST,
            "consent_required",
            "please accept the notice to continue",
        ));
    }
    let lang = sessions::normalize_lang(&body.lang).ok_or_else(|| {
        api_err(
            StatusCode::BAD_REQUEST,
            "bad_lang",
            "unsupported language code",
        )
    })?;

    // Shape first: junk never reaches the database, and a malformed code answers exactly
    // like an unknown one so the response is no oracle.
    let not_found = || {
        api_err(
            StatusCode::NOT_FOUND,
            "invite_not_found",
            "this invitation is not valid",
        )
    };
    if !tokens::is_well_formed_invite(&body.code) {
        return Err(not_found());
    }
    if !engine_has_room(&state) {
        return Err(busy());
    }

    let joined = sessions::join_guest(pool, &body.code, &body.name, &lang)
        .await
        .map_err(|e| match e {
            JoinError::NotFound => not_found(),
            JoinError::Expired => api_err(
                StatusCode::GONE,
                "invite_expired",
                "this invitation has expired",
            ),
            JoinError::Ended => api_err(
                StatusCode::GONE,
                "session_ended",
                "this conversation has ended",
            ),
            JoinError::Full => api_err(
                StatusCode::CONFLICT,
                "session_full",
                "someone has already joined this conversation",
            ),
            JoinError::Db(e) => db_err(e),
        })?;

    let token = sessions::issue_token(
        &state.live.config,
        joined.session_id,
        joined.guest_peer,
        PeerRole::Guest,
        joined.created_at,
    )
    .ok_or_else(disabled)?;
    sessions::store_token_hash(pool, joined.guest_peer, &token).await;
    sessions::record_event(
        pool,
        joined.session_id,
        "guest_joined",
        json!({ "lang": lang }),
    )
    .await;

    Ok(Json(json!({
        "session_id": joined.session_id,
        "peer_id": joined.guest_peer,
        "token": token,
        "ws_path": WS_PATH,
        "host": { "name": joined.host_name, "lang": joined.host_lang },
    }))
    .into_response())
}

// ---- read / end ------------------------------------------------------------------------

pub async fn get_session(
    State(state): State<AppState>,
    LiveAuth(claims): LiveAuth,
    Path(id): Path<Uuid>,
) -> Result<Response, Response> {
    let auth = authorize(&state, &claims, id).await?;
    let pool = require_pool(&state)?;
    let view = sessions::session_view(pool, id)
        .await
        .map_err(db_err)?
        .ok_or_else(|| {
            api_err(
                StatusCode::GONE,
                "session_ended",
                "this conversation has ended",
            )
        })?;
    Ok(
        Json(json!({ "session": view, "you": { "peer_id": auth.peer_id, "role": auth.role } }))
            .into_response(),
    )
}

pub async fn end(
    State(state): State<AppState>,
    LiveAuth(claims): LiveAuth,
    Path(id): Path<Uuid>,
) -> Result<Response, Response> {
    let auth = authorize(&state, &claims, id).await?;
    let pool = require_pool(&state)?;
    let reason = match auth.role {
        PeerRole::Guest => "ended_by_guest",
        PeerRole::Host => "ended_by_host",
        PeerRole::Staff => "ended_by_staff",
    };
    let ended = sessions::end_session(pool, id, reason)
        .await
        .map_err(db_err)?;
    if ended {
        sessions::record_event(pool, id, "ended", json!({ "by": auth.role })).await;
    }
    Ok(Json(json!({ "ended": ended })).into_response())
}
