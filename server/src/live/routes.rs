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

use std::sync::Arc;
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
use super::tiers;
use super::tokens::{self, GuestClaims, PeerRole};
use crate::billing::usd;
use crate::business::{db_err, require_pool, require_role, ws_gate, MEMBER};
use crate::engine::{TranslationEngine, CARTESIA_ID, GEMINI_ID, STANDARD_ID};
use crate::middleware::AuthUser;
use crate::AppState;

/// Path the clients open the WebSocket on.
pub const WS_PATH: &str = "/ws/live";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/live/engines", get(engines))
        .route("/api/live/sessions", post(create))
        .route("/api/live/join", post(join))
        .route("/api/live/sessions/{id}", get(get_session))
        .route("/api/live/sessions/{id}/end", post(end))
        .route(
            "/api/live/sessions/{id}/enhanced/session",
            post(super::enhanced::token),
        )
        .route(WS_PATH, get(super::ws::ws_live))
        .merge(super::desks::routes())
        .merge(super::dashboard::routes())
        .merge(super::summary::routes())
}

// ---- helpers ---------------------------------------------------------------------------

pub fn api_err(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({ "code": code, "message": message }))).into_response()
}

pub(super) fn disabled() -> Response {
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
pub(super) fn engine_has_room(state: &AppState) -> bool {
    engine_has_room_for(&*state.engines.default())
}

/// [`engine_has_room`] for the tier a conversation actually runs on. Premium meters nothing
/// itself (it reports no free capacity), so this passes for it: its limit shows up at the first
/// word as an `AtCapacity` the socket answers with a fallback, never as a refusal here.
pub(super) fn engine_has_room_for(engine: &dyn TranslationEngine) -> bool {
    engine.free_capacity().is_none_or(|free| free >= 2)
}

/// The engine's spare upstream sessions right now (`None` = it does not meter itself). A guest's
/// arrival at a room that already has speakers needs more than two (see
/// [`sessions::engine_sessions_for_join`]), so `join` hands this to the transaction that seats
/// them rather than a yes/no.
fn engine_free_capacity(engine: &dyn TranslationEngine) -> Option<usize> {
    engine.free_capacity()
}

/// Whether `owner` holds enough to open or Start a conversation on `engine`.
///
/// Standard keeps the platform minimum exactly (`BillingService::can_join`). A dearer tier needs
/// the same seconds of runway at its own rate, so the minimum scales with the rate: see
/// [`tiers::required_balance`].
pub(super) async fn owner_can_afford(
    state: &AppState,
    owner: Uuid,
    engine: &dyn TranslationEngine,
) -> Result<bool, sqlx::Error> {
    let Some(billing) = state.billing.as_ref() else {
        return Ok(false);
    };
    let Some(need) = min_balance_for(state, engine) else {
        return billing.can_join(owner).await;
    };
    Ok(billing.get_balance(owner).await? >= usd(need))
}

/// The balance a dearer-than-Standard `engine` requires, or `None` when the platform minimum
/// (what `can_join` checks) is all it takes.
pub(super) fn min_balance_for(state: &AppState, engine: &dyn TranslationEngine) -> Option<f64> {
    let base = state.config.billing.as_ref()?.pricing.min_balance_to_join;
    let standard = state.engines.default().metadata().user_rate_per_minute();
    let need = tiers::required_balance(base, engine.metadata().user_rate_per_minute(), standard);
    (need > base).then_some(need)
}

pub(super) fn busy() -> Response {
    api_err(
        StatusCode::SERVICE_UNAVAILABLE,
        "live_busy",
        "All translation lines are busy right now. Please try again in a moment.",
    )
}

pub(super) fn rate_limited() -> Response {
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
pub(super) async fn authorize(
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
    /// How many guests the link/QR admits: 1 to `LIVE_MAX_GUESTS` (3 at most). Omitted = 1, the
    /// two-person conversation. Kept as raw JSON so a wrong type or an out-of-range number
    /// answers the same stable `bad_max_guests` as any other bad value, not a framework 422.
    #[serde(default)]
    pub max_guests: Option<serde_json::Value>,
    /// The translation tier every seat of this conversation runs on: `standard` (default),
    /// `gemini_live_translate` (Premium) or `cartesia` (Enhanced, when this deployment has it).
    /// Anything else answers `engine_not_available`; a business conversation is Standard only
    /// (`engine_not_allowed`). See [`tiers`].
    #[serde(default)]
    pub engine: Option<String>,
    /// Enhanced only: speak the host's words on the other phones in the host's own cloned voice
    /// (`users.cartesia_voice_id`). Off unless asked for, per conversation. Needs `engine:
    /// "cartesia"`, cloning enabled on this deployment and a clone on the host's account, else
    /// `voice_clone_unavailable`. Raw JSON so a non-boolean answers `bad_voice_clone`.
    #[serde(default)]
    pub voice_clone: Option<serde_json::Value>,
}

/// The validated `voice_clone` flag of a create request (absent is off).
fn parse_voice_clone(raw: Option<&serde_json::Value>) -> Result<bool, Response> {
    match raw {
        None | Some(serde_json::Value::Null) => Ok(false),
        Some(serde_json::Value::Bool(b)) => Ok(*b),
        Some(_) => Err(api_err(
            StatusCode::BAD_REQUEST,
            "bad_voice_clone",
            "voice_clone must be true or false",
        )),
    }
}

/// The validated guest count of a create request.
fn parse_max_guests(raw: Option<&serde_json::Value>, ceiling: i64) -> Result<i16, Response> {
    let Some(v) = raw else {
        return Ok(1);
    };
    v.as_i64()
        .filter(|n| (1..=ceiling).contains(n))
        .and_then(|n| i16::try_from(n).ok())
        .ok_or_else(|| {
            api_err(
                StatusCode::BAD_REQUEST,
                "bad_max_guests",
                "max_guests must be a whole number from 1 to the allowed maximum",
            )
        })
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

    let max_guests = parse_max_guests(body.max_guests.as_ref(), cfg.max_guests)?;
    let want_clone = parse_voice_clone(body.voice_clone.as_ref())?;
    let engine_id = tiers::choose(
        &state.engines,
        body.engine.as_deref(),
        body.org_id.is_some(),
    )
    .map_err(|e| api_err(StatusCode::BAD_REQUEST, e.code(), e.message()))?;
    let engine = state.engines.get(&engine_id).ok_or_else(|| {
        // `choose` just confirmed it; a registry cannot lose an engine between two lines.
        api_err(
            StatusCode::BAD_REQUEST,
            tiers::TierError::NotAvailable.code(),
            tiers::TierError::NotAvailable.message(),
        )
    })?;

    if !state.rate_limiter.allow(
        &format!("livecreate:{}", user.user_id),
        20,
        Duration::from_secs(60),
    ) {
        return Err(rate_limited());
    }
    if !engine_has_room_for(&*engine) {
        return Err(busy());
    }
    // The host's cloned voice is used for THIS conversation only when they asked for it, on the
    // tier that can play it, and have one. Checked here, once, so a conversation never starts with
    // a voice flag nothing can honour.
    let unavailable = || {
        api_err(
            StatusCode::BAD_REQUEST,
            "voice_clone_unavailable",
            "a cloned voice is not available for this conversation",
        )
    };
    if want_clone {
        let cloning = state
            .config
            .cartesia
            .as_ref()
            .is_some_and(|c| c.voice_cloning_enabled);
        if engine_id != crate::engine::CARTESIA_ID || !cloning {
            return Err(unavailable());
        }
        let has_clone = match state.billing.as_ref() {
            Some(b) => b
                .get_cartesia_voice_id(user.user_id)
                .await
                .map_err(db_err)?
                .is_some_and(|v| !v.is_empty()),
            None => false,
        };
        if !has_clone {
            return Err(unavailable());
        }
    }

    let owner = match body.org_id {
        None => {
            if state.billing.is_none() {
                return Err(api_err(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "billing_unavailable",
                    "billing not configured",
                ));
            }
            if !owner_can_afford(&state, user.user_id, &*engine)
                .await
                .map_err(db_err)?
            {
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
            engine_id: Some(&engine_id),
            max_guests,
            use_voice_clone: want_clone,
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
        json!({
            "b2b": body.org_id.is_some(),
            "max_guests": max_guests,
            "engine": engine_id,
            "voice_clone": want_clone,
        }),
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
        "max_guests": max_guests,
        "engine": engine_id,
        "voice_clone": want_clone,
    }))
    .into_response())
}

// ---- tiers -----------------------------------------------------------------------------

/// `GET /api/live/engines` — the tiers a host can pick when creating a conversation, each with
/// what one stream costs per minute, so the picker can say "from $x / min per language".
///
/// Only tiers a Live Connect conversation can actually run on are listed, and only those this
/// deployment has enabled (Premium needs its flag and key). `available` is a capacity hint, not
/// a promise: a tier that reports `false` would be refused `live_busy` right now. Every Live
/// Connect tier bills per distinct target language, whatever its own metadata says.
///
/// Enhanced (`cartesia`) is listed between Standard and Premium when registered, `client_direct`,
/// with a `voice_clone` object for THIS user: whether cloning is enabled here and whether their
/// account has a clone (`available` is both). The clone id itself is never listed.
pub async fn engines(State(state): State<AppState>, user: AuthUser) -> Response {
    let describe = |e: &Arc<dyn TranslationEngine>| {
        let m = e.metadata();
        let room = engine_has_room_for(&**e);
        let min_balance = min_balance_for(&state, &**e).or_else(|| {
            state
                .config
                .billing
                .as_ref()
                .map(|b| b.pricing.min_balance_to_join)
        });
        let mut v = json!({
            "id": m.id,
            "display_name": m.display_name,
            "tier": m.tier,
            "description": m.description,
            "rate_per_minute": m.user_rate_per_minute(),
            "min_balance": min_balance,
            "available": room,
            "scales_per_language": true,
            "client_direct": m.capabilities.client_direct,
        });
        if !room {
            v["reason"] = json!("at_capacity");
        }
        v
    };
    let mut list: Vec<serde_json::Value> = Vec::new();
    // Display order: Standard, Enhanced, Premium (the registry's own order).
    for id in [STANDARD_ID, CARTESIA_ID, GEMINI_ID] {
        let Some(e) = state.engines.get(id) else {
            continue;
        };
        let mut v = describe(&e);
        if id == CARTESIA_ID {
            // The host's own cloned voice, for THIS user: whether the deployment allows cloning
            // and whether their account has a clone. `available` is both.
            let cloning = state
                .config
                .cartesia
                .as_ref()
                .is_some_and(|c| c.voice_cloning_enabled);
            let has_clone = match state.billing.as_ref() {
                Some(b) => b
                    .get_cartesia_voice_id(user.user_id)
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|v| !v.is_empty()),
                None => false,
            };
            v["voice_clone"] = json!({
                "available": cloning && has_clone,
                "cloning_enabled": cloning,
                "has_clone": has_clone,
            });
        } else if !tiers::SELECTABLE.contains(&id) {
            continue;
        }
        list.push(v);
    }
    Json(json!({ "default": tiers::DEFAULT_TIER, "engines": list })).into_response()
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
    // Judge the tier the conversation runs on, not the platform default: a Premium conversation
    // must not be turned away because Standard is full, and one that fell back to Standard is
    // judged by Standard. An unknown code falls through to the default and then to `not_found`.
    let stored = sessions::engine_for_invite(pool, &body.code)
        .await
        .flatten();
    let engine = tiers::capacity_engine(&state.engines, stored.as_deref(), false);
    if !engine_has_room_for(&*engine) {
        return Err(busy());
    }

    let joined = sessions::join_guest(
        pool,
        &body.code,
        &body.name,
        &lang,
        engine_free_capacity(&*engine),
    )
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
            "this conversation is full",
        ),
        JoinError::Busy => busy(),
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
        "max_guests": joined.max_guests,
        "engine": sessions::effective_engine(joined.engine_id.as_deref()),
        "voice_clone": joined.use_voice_clone,
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
    // With several guests on one link, one of them hanging up must not end the conversation for
    // the others: that is the host's call (or the last phone gone). A guest "ends" by leaving.
    if auth.role == PeerRole::Guest {
        let max_guests: Option<i16> =
            sqlx::query_scalar("SELECT max_guests FROM live_sessions WHERE id = $1")
                .bind(id)
                .fetch_optional(pool)
                .await
                .map_err(db_err)?;
        if max_guests.is_some_and(|n| n > 1) {
            sessions::record_event(pool, id, "guest_left", json!({ "peer": auth.peer_id })).await;
            return Ok(Json(json!({ "ended": false, "left": true })).into_response());
        }
    }
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
        super::finalize::on_session_ended(&state, id);
    }
    Ok(Json(json!({ "ended": ended })).into_response())
}
