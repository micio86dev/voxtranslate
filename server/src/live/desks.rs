//! Desks: permanent QR entry points for a business, and the staff side of a scan.
//!
//! A desk is a place (a reception, a counter) that has a printed QR. The QR is **not** a
//! conversation: it carries a public handle and a MAC, nothing else, and every scan creates a
//! brand-new temporary session — so the code on the wall can be photographed, shared or
//! revoked without ever pointing at somebody else's conversation.
//!
//! Who does what:
//! * an org **admin** creates, edits, revokes and regenerates desks and downloads the QR;
//! * **anyone** (no account) scans it, which seats them as the guest of a new `waiting` session;
//! * an org **member** sees the waiting sessions of a desk and answers one, taking the staff seat.
//!
//! The public scan answers an unknown handle, a forged MAC, a revoked desk and a regenerated
//! (old) QR with the very same response, so it is no oracle for which desks exist.
#![allow(clippy::result_large_err)]

use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::routes::{api_err, busy, disabled, engine_has_room, rate_limited, WS_PATH};
use super::sessions::{self, clean_name, normalize_lang};
use super::tokens::{self, PeerRole};
use crate::business::{db_err, require_pool, require_role, ws_gate, ADMIN, MEMBER};
use crate::db::Pool;
use crate::middleware::AuthUser;
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/business/organizations/{org_id}/live/desks",
            post(create_desk).get(list_desks),
        )
        .route(
            "/api/business/organizations/{org_id}/live/desks/{desk_id}",
            axum::routing::patch(update_desk),
        )
        .route(
            "/api/business/organizations/{org_id}/live/desks/{desk_id}/qr",
            get(desk_qr),
        )
        .route(
            "/api/business/organizations/{org_id}/live/desks/{desk_id}/revoke",
            post(revoke_desk),
        )
        .route(
            "/api/business/organizations/{org_id}/live/desks/{desk_id}/regenerate",
            post(regenerate_desk),
        )
        .route("/api/live/desks/scan", post(scan))
        .route("/api/live/desks/{desk_id}/waiting", get(waiting))
        .route("/api/live/my-desks", get(my_desks))
        .route("/api/live/sessions/{id}/answer", post(answer))
}

// ---- rows and views --------------------------------------------------------------------

#[derive(sqlx::FromRow)]
pub(super) struct DeskRow {
    id: Uuid,
    org_id: Uuid,
    project_id: Option<Uuid>,
    name: String,
    location_label: Option<String>,
    default_lang: String,
    languages: Vec<String>,
    public_id: String,
    /// Read by the platform-admin regenerate, which reports the new version and nothing else.
    pub(super) token_version: i32,
    status: String,
    created_at: DateTime<Utc>,
}

const DESK_COLS: &str = "id, org_id, project_id, name, location_label, default_lang, languages, \
                         public_id, token_version, status, created_at";

impl DeskRow {
    /// The desk as an admin sees it. No QR here — that is a separate, admin-only call.
    fn view(&self) -> serde_json::Value {
        json!({
            "id": self.id,
            "org_id": self.org_id,
            "project_id": self.project_id,
            "name": self.name,
            "location_label": self.location_label,
            "default_lang": self.default_lang,
            "languages": self.languages,
            "public_id": self.public_id,
            "status": self.status,
            "created_at": self.created_at,
        })
    }

    /// The URL printed in the QR. Derived, never stored.
    fn qr_url(&self, state: &AppState) -> Option<String> {
        let keys = state.live.config.keys.as_ref()?;
        let mac = keys.desk_mac(self.id, self.token_version);
        Some(tokens::desk_url(
            &state.config.app_base_url,
            &self.public_id,
            &mac,
        ))
    }
}

async fn desk_in_org(pool: &Pool, org_id: Uuid, desk_id: Uuid) -> Result<DeskRow, Response> {
    sqlx::query_as::<_, DeskRow>(&format!(
        "SELECT {DESK_COLS} FROM live_desks WHERE id = $1 AND org_id = $2"
    ))
    .bind(desk_id)
    .bind(org_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?
    .ok_or_else(|| api_err(StatusCode::NOT_FOUND, "desk_not_found", "desk not found"))
}

/// The project must belong to this org and still be live: a foreign or archived project id is
/// indistinguishable from a missing one.
async fn project_in_org(pool: &Pool, org_id: Uuid, project_id: Uuid) -> Result<(), Response> {
    let ok: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM projects WHERE id = $1 AND org_id = $2 AND archived_at IS NULL",
    )
    .bind(project_id)
    .bind(org_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    ok.map(|_| ()).ok_or_else(|| {
        api_err(
            StatusCode::NOT_FOUND,
            "project_not_found",
            "project not found",
        )
    })
}

fn clean_text(raw: &str, max: usize) -> String {
    raw.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(max)
        .collect()
}

fn lang_or_400(raw: &str) -> Result<String, Response> {
    normalize_lang(raw).ok_or_else(|| {
        api_err(
            StatusCode::BAD_REQUEST,
            "bad_lang",
            "unsupported language code",
        )
    })
}

fn languages_or_400(raw: &[String]) -> Result<Vec<String>, Response> {
    if raw.len() > 20 {
        return Err(api_err(
            StatusCode::BAD_REQUEST,
            "bad_lang",
            "too many languages",
        ));
    }
    raw.iter().map(|l| lang_or_400(l)).collect()
}

// ---- admin: create / list / edit / revoke / regenerate ---------------------------------

#[derive(Debug, Deserialize)]
pub struct DeskBody {
    pub name: String,
    pub location_label: Option<String>,
    pub default_lang: String,
    #[serde(default)]
    pub languages: Vec<String>,
    /// Mandatory: a business conversation belongs to a project.
    pub project_id: Uuid,
}

pub async fn create_desk(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Json(body): Json<DeskBody>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;
    if !state.rate_limiter.allow(
        &format!("livedesk:{}", user.user_id),
        30,
        Duration::from_secs(60),
    ) {
        return Err(rate_limited());
    }
    let name = clean_text(&body.name, 100);
    if name.is_empty() {
        return Err(api_err(
            StatusCode::BAD_REQUEST,
            "name_required",
            "a desk needs a name",
        ));
    }
    let location = Some(clean_text(
        body.location_label.as_deref().unwrap_or(""),
        150,
    ))
    .filter(|l| !l.is_empty());
    let default_lang = lang_or_400(&body.default_lang)?;
    let languages = languages_or_400(&body.languages)?;
    project_in_org(pool, org_id, body.project_id).await?;

    // The handle is short and random; on the (very unlikely) collision, draw another.
    for _ in 0..5 {
        let public_id = tokens::new_desk_public_id();
        let inserted = sqlx::query_as::<_, DeskRow>(&format!(
            "INSERT INTO live_desks (org_id, project_id, name, location_label, default_lang, languages, public_id, created_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             RETURNING {DESK_COLS}"
        ))
        .bind(org_id)
        .bind(body.project_id)
        .bind(&name)
        .bind(&location)
        .bind(&default_lang)
        .bind(&languages)
        .bind(&public_id)
        .bind(user.user_id)
        .fetch_one(pool)
        .await;
        match inserted {
            Ok(desk) => {
                let mut view = desk.view();
                view["qr_url"] = json!(desk.qr_url(&state));
                return Ok((StatusCode::CREATED, Json(view)).into_response());
            }
            Err(e) if crate::business::is_unique_violation(&e) => continue,
            Err(e) => return Err(db_err(e)),
        }
    }
    Err(api_err(
        StatusCode::SERVICE_UNAVAILABLE,
        "try_again",
        "could not allocate a desk handle",
    ))
}

pub async fn list_desks(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;
    let rows = sqlx::query_as::<_, DeskRow>(&format!(
        "SELECT {DESK_COLS} FROM live_desks WHERE org_id = $1 ORDER BY created_at DESC"
    ))
    .bind(org_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    Ok(
        Json(json!({ "desks": rows.iter().map(DeskRow::view).collect::<Vec<_>>() }))
            .into_response(),
    )
}

#[derive(Debug, Deserialize)]
pub struct DeskPatch {
    pub name: Option<String>,
    pub location_label: Option<String>,
    pub default_lang: Option<String>,
    pub languages: Option<Vec<String>>,
    pub project_id: Option<Uuid>,
}

pub async fn update_desk(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, desk_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<DeskPatch>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;
    let current = desk_in_org(pool, org_id, desk_id).await?;

    let name = match body.name.as_deref().map(|n| clean_text(n, 100)) {
        Some(n) if n.is_empty() => {
            return Err(api_err(
                StatusCode::BAD_REQUEST,
                "name_required",
                "a desk needs a name",
            ))
        }
        Some(n) => n,
        None => current.name.clone(),
    };
    let location = match body.location_label.as_deref() {
        Some(l) => Some(clean_text(l, 150)).filter(|l| !l.is_empty()),
        None => current.location_label.clone(),
    };
    let default_lang = match body.default_lang.as_deref() {
        Some(l) => lang_or_400(l)?,
        None => current.default_lang.clone(),
    };
    let languages = match body.languages.as_deref() {
        Some(l) => languages_or_400(l)?,
        None => current.languages.clone(),
    };
    let project = match body.project_id {
        Some(p) => {
            project_in_org(pool, org_id, p).await?;
            Some(p)
        }
        None => current.project_id,
    };

    let desk = sqlx::query_as::<_, DeskRow>(&format!(
        "UPDATE live_desks SET name = $3, location_label = $4, default_lang = $5, languages = $6, project_id = $7
         WHERE id = $1 AND org_id = $2 RETURNING {DESK_COLS}"
    ))
    .bind(desk_id)
    .bind(org_id)
    .bind(&name)
    .bind(&location)
    .bind(&default_lang)
    .bind(&languages)
    .bind(project)
    .fetch_one(pool)
    .await
    .map_err(db_err)?;
    Ok(Json(desk.view()).into_response())
}

/// The URL to print. Admin-only: it is the credential.
pub async fn desk_qr(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, desk_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;
    let desk = desk_in_org(pool, org_id, desk_id).await?;
    let url = desk.qr_url(&state).ok_or_else(disabled)?;
    Ok(Json(
        json!({ "url": url, "public_id": desk.public_id, "status": desk.status, "name": desk.name,
                    "location_label": desk.location_label }),
    )
    .into_response())
}

/// Close every session still waiting at a desk — they came in through a QR that was just
/// revoked or replaced.
async fn expire_waiting(pool: &Pool, desk_id: Uuid, why: &str) -> Result<u64, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE live_sessions SET status = 'expired', ended_at = now(), end_reason = $2
         WHERE desk_id = $1 AND status = 'waiting'",
    )
    .bind(desk_id)
    .bind(why)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Revoke a desk: its printed QR stops working and everyone still waiting at it is closed.
/// Returns how many waiting sessions were closed. Shared by the org admin's endpoint and the
/// platform admin's, so both do exactly the same thing; idempotent.
pub(super) async fn revoke(pool: &Pool, desk_id: Uuid) -> Result<u64, sqlx::Error> {
    sqlx::query("UPDATE live_desks SET status = 'revoked', revoked_at = now() WHERE id = $1")
        .bind(desk_id)
        .execute(pool)
        .await?;
    expire_waiting(pool, desk_id, "desk_revoked").await
}

/// Move a desk on to a fresh QR: the version bumps (every earlier print stops verifying) and a
/// revoked desk is active again. Returns the updated row and how many waiting sessions were
/// closed. The caller decides whether the new credential may be shown to anyone.
pub(super) async fn regenerate(pool: &Pool, desk_id: Uuid) -> Result<(DeskRow, u64), sqlx::Error> {
    let desk = sqlx::query_as::<_, DeskRow>(&format!(
        "UPDATE live_desks SET token_version = token_version + 1, status = 'active', revoked_at = NULL
         WHERE id = $1 RETURNING {DESK_COLS}"
    ))
    .bind(desk_id)
    .fetch_one(pool)
    .await?;
    let closed = expire_waiting(pool, desk_id, "desk_regenerated").await?;
    Ok((desk, closed))
}

pub async fn revoke_desk(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, desk_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;
    desk_in_org(pool, org_id, desk_id).await?;
    let closed = revoke(pool, desk_id).await.map_err(db_err)?;
    Ok(Json(json!({ "status": "revoked", "waiting_closed": closed })).into_response())
}

/// A fresh QR for the desk: the version moves on, so every earlier print stops verifying. Also
/// re-activates a revoked desk — issuing a new code is the explicit act of putting it back.
pub async fn regenerate_desk(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, desk_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;
    desk_in_org(pool, org_id, desk_id).await?;
    let (desk, closed) = regenerate(pool, desk_id).await.map_err(db_err)?;
    let mut view = desk.view();
    view["qr_url"] = json!(desk.qr_url(&state));
    view["waiting_closed"] = json!(closed);
    Ok(Json(view).into_response())
}

// ---- public: scan ----------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ScanBody {
    /// The path segment of the scanned URL: `<public_id>.<mac>`.
    pub segment: String,
    pub name: Option<String>,
    pub lang: String,
    pub consent: bool,
}

fn invalid_qr() -> Response {
    api_err(
        StatusCode::NOT_FOUND,
        "invalid_desk_qr",
        "this QR code is not valid",
    )
}

pub async fn scan(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ScanBody>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let cfg = &state.live.config;
    let keys = cfg.keys.as_ref().ok_or_else(disabled)?;
    let ip = crate::observability::client_ip(&headers);
    if !state
        .rate_limiter
        .allow(&format!("livescan:{ip}"), 20, Duration::from_secs(60))
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
    let lang = lang_or_400(&body.lang)?;

    // Shape, then handle, then MAC — and every failure is the same answer.
    let (public_id, mac) = tokens::split_desk_segment(&body.segment).ok_or_else(invalid_qr)?;
    let desk = sqlx::query_as::<_, DeskRow>(&format!(
        "SELECT {DESK_COLS} FROM live_desks WHERE public_id = $1"
    ))
    .bind(public_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?
    .ok_or_else(invalid_qr)?;
    if desk.status != "active" || !keys.verify_desk_mac(desk.id, desk.token_version, mac) {
        return Err(invalid_qr());
    }
    // A flood against one desk is bounded separately from a flood from one address.
    if !state.rate_limiter.allow(
        &format!("livescan-desk:{}", desk.id),
        60,
        Duration::from_secs(60),
    ) {
        return Err(rate_limited());
    }

    // The org must be able to pay for a conversation. The caller is a stranger: say only that
    // the desk is unavailable, never why (that would reveal an org's wallet to the public).
    let unavailable = || {
        api_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "desk_unavailable",
            "This service is not available right now.",
        )
    };
    if ws_gate::check(pool, desk.org_id)
        .await
        .map_err(db_err)?
        .is_some()
    {
        return Err(unavailable());
    }
    if !engine_has_room(&state) {
        return Err(busy());
    }

    let created = create_desk_session(
        pool,
        &state,
        &desk,
        &clean_name(body.name.as_deref().unwrap_or("")),
        &lang,
    )
    .await
    .map_err(|e| match e {
        DeskScanError::TooManyWaiting => api_err(
            StatusCode::TOO_MANY_REQUESTS,
            "desk_busy",
            "Too many people are waiting at this desk. Please try again shortly.",
        ),
        DeskScanError::Db(e) => db_err(e),
    })?;

    let token = sessions::issue_token(
        cfg,
        created.session_id,
        created.guest_peer,
        PeerRole::Guest,
        created.created_at,
    )
    .ok_or_else(disabled)?;
    sessions::store_token_hash(pool, created.guest_peer, &token).await;
    sessions::record_event(
        pool,
        created.session_id,
        "desk_scan",
        json!({ "desk": desk.id }),
    )
    .await;
    Ok(Json(json!({
        "session_id": created.session_id,
        "peer_id": created.guest_peer,
        "token": token,
        "ws_path": WS_PATH,
        "desk": { "name": desk.name, "location_label": desk.location_label },
        "wait_secs": cfg.desk_wait_secs,
    }))
    .into_response())
}

#[derive(Debug)]
enum DeskScanError {
    TooManyWaiting,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for DeskScanError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

struct DeskSession {
    session_id: Uuid,
    guest_peer: Uuid,
    created_at: DateTime<Utc>,
}

/// A NEW session for this scan, with the scanner seated as the guest. Always new: nothing here
/// looks for an existing conversation, because a QR must never be one.
async fn create_desk_session(
    pool: &Pool,
    state: &AppState,
    desk: &DeskRow,
    guest_name: &str,
    guest_lang: &str,
) -> Result<DeskSession, DeskScanError> {
    let cfg = &state.live.config;
    let mut tx = pool.begin().await?;
    // Serialise scans per desk so the waiting cap cannot be beaten by a burst.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("live-desk:{}", desk.id))
        .execute(&mut *tx)
        .await?;
    let waiting: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM live_sessions WHERE desk_id = $1 AND status = 'waiting' AND expires_at > now()",
    )
    .bind(desk.id)
    .fetch_one(&mut *tx)
    .await?;
    // A flood of scans cannot fill the table or the staff console: past the cap the scan is
    // refused until somebody answers or the rest expire.
    if waiting >= cfg.desk_max_waiting {
        return Err(DeskScanError::TooManyWaiting);
    }
    let (session_id, created_at): (Uuid, DateTime<Utc>) = sqlx::query_as(
        "INSERT INTO live_sessions (org_id, project_id, desk_id, status, host_lang, save_transcript, expires_at)
         VALUES ($1, $2, $3, 'waiting', $4, TRUE, now() + make_interval(secs => $5::int))
         RETURNING id, created_at",
    )
    .bind(desk.org_id)
    .bind(desk.project_id)
    .bind(desk.id)
    .bind(&desk.default_lang)
    .bind(cfg.desk_wait_secs as i32)
    .fetch_one(&mut *tx)
    .await?;
    let guest_peer: Uuid = sqlx::query_scalar(
        "INSERT INTO live_participants (session_id, role, display_name, lang, consent_at)
         VALUES ($1, 'guest', $2, $3, now()) RETURNING peer_id",
    )
    .bind(session_id)
    .bind(guest_name)
    .bind(guest_lang)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(DeskSession {
        session_id,
        guest_peer,
        created_at,
    })
}

// ---- staff: who is waiting, and answering ----------------------------------------------

pub async fn waiting(
    State(state): State<AppState>,
    user: AuthUser,
    Path(desk_id): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let org_id: Option<Uuid> = sqlx::query_scalar("SELECT org_id FROM live_desks WHERE id = $1")
        .bind(desk_id)
        .fetch_optional(pool)
        .await
        .map_err(db_err)?;
    // An unknown desk and a desk of another org look the same to a non-member.
    let org_id =
        org_id.ok_or_else(|| api_err(StatusCode::NOT_FOUND, "desk_not_found", "desk not found"))?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;
    let rows: Vec<(Uuid, String, String, i32)> = sqlx::query_as(
        "SELECT s.id, p.display_name, p.lang, extract(epoch FROM now() - s.created_at)::int
         FROM live_sessions s
         JOIN live_participants p ON p.session_id = s.id AND p.role = 'guest'
         WHERE s.desk_id = $1 AND s.status = 'waiting' AND s.expires_at > now()
         ORDER BY s.created_at",
    )
    .bind(desk_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    let waiting: Vec<_> = rows
        .into_iter()
        .map(|(session_id, guest_name, guest_lang, waited_secs)| {
            json!({ "session_id": session_id, "guest_name": guest_name, "guest_lang": guest_lang, "waited_secs": waited_secs })
        })
        .collect();
    Ok(Json(json!({ "waiting": waiting })).into_response())
}

#[derive(sqlx::FromRow)]
struct MyDeskRow {
    id: Uuid,
    org_id: Uuid,
    org_name: String,
    name: String,
    location_label: Option<String>,
    default_lang: String,
    waiting: i64,
}

/// The desks of every org the caller belongs to, with how many are waiting — the staff list in
/// the main app. Members only: a `guest` org membership ranks below member and sees nothing.
pub async fn my_desks(State(state): State<AppState>, user: AuthUser) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let rows: Vec<MyDeskRow> = sqlx::query_as(
        "SELECT d.id, d.org_id, o.name AS org_name, d.name, d.location_label, d.default_lang,
                (SELECT count(*) FROM live_sessions s
                  WHERE s.desk_id = d.id AND s.status = 'waiting' AND s.expires_at > now()) AS waiting
         FROM live_desks d
         JOIN organizations o ON o.id = d.org_id
         JOIN organization_members m ON m.org_id = d.org_id AND m.user_id = $1
         WHERE d.status = 'active' AND m.role IN ('owner', 'admin', 'member')
         ORDER BY o.name, d.name",
    )
    .bind(user.user_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    let desks: Vec<_> = rows
        .into_iter()
        .map(|d| {
            json!({ "id": d.id, "org_id": d.org_id, "org_name": d.org_name, "name": d.name,
                    "location_label": d.location_label, "default_lang": d.default_lang, "waiting": d.waiting })
        })
        .collect();
    Ok(Json(json!({ "desks": desks })).into_response())
}

#[derive(Debug, Deserialize)]
pub struct AnswerBody {
    /// The language the staff member will speak in this conversation.
    pub lang: String,
}

#[derive(Debug)]
enum AnswerError {
    NotFound,
    AlreadyAnswered,
    Ended,
    Expired,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for AnswerError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

pub async fn answer(
    State(state): State<AppState>,
    user: AuthUser,
    Path(session_id): Path<Uuid>,
    Json(body): Json<AnswerBody>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let cfg = &state.live.config;
    let lang = lang_or_400(&body.lang)?;

    // Which org owns this desk session? Only a member of THAT org may answer it, and a session
    // of any other kind (or of another org) is simply not found.
    let org_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT org_id FROM live_sessions WHERE id = $1 AND desk_id IS NOT NULL",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?
    .flatten();
    let org_id = org_id.ok_or_else(|| {
        api_err(
            StatusCode::NOT_FOUND,
            "session_not_found",
            "session not found",
        )
    })?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;
    if let Some(why) = ws_gate::check(pool, org_id).await.map_err(db_err)? {
        return Err(api_err(
            StatusCode::PAYMENT_REQUIRED,
            why.code(),
            "organization is not eligible",
        ));
    }
    if !engine_has_room(&state) {
        return Err(busy());
    }

    let answered = answer_session(
        pool,
        session_id,
        user.user_id,
        &clean_name(&user.name),
        &lang,
    )
    .await
    .map_err(|e| match e {
        AnswerError::NotFound => api_err(
            StatusCode::NOT_FOUND,
            "session_not_found",
            "session not found",
        ),
        AnswerError::AlreadyAnswered => api_err(
            StatusCode::CONFLICT,
            "already_answered",
            "a colleague already answered this one",
        ),
        AnswerError::Ended => api_err(
            StatusCode::GONE,
            "session_ended",
            "this conversation has ended",
        ),
        AnswerError::Expired => api_err(
            StatusCode::GONE,
            "session_expired",
            "the customer stopped waiting",
        ),
        AnswerError::Db(e) => db_err(e),
    })?;

    let token = sessions::issue_token(
        cfg,
        session_id,
        answered.staff_peer,
        PeerRole::Staff,
        answered.created_at,
    )
    .ok_or_else(disabled)?;
    sessions::store_token_hash(pool, answered.staff_peer, &token).await;
    sessions::record_event(pool, session_id, "answered", json!({ "by": user.user_id })).await;
    Ok(Json(json!({
        "session_id": session_id,
        "peer_id": answered.staff_peer,
        "token": token,
        "ws_path": WS_PATH,
        "guest": { "name": answered.guest_name, "lang": answered.guest_lang },
    }))
    .into_response())
}

struct Answered {
    staff_peer: Uuid,
    created_at: DateTime<Utc>,
    guest_name: String,
    guest_lang: String,
}

/// Take the staff seat of a waiting desk session. Exactly one member can: the seat is a unique
/// index, so two colleagues tapping Answer at once leave one winner and one `already_answered`.
async fn answer_session(
    pool: &Pool,
    session_id: Uuid,
    user_id: Uuid,
    name: &str,
    lang: &str,
) -> Result<Answered, AnswerError> {
    let mut tx = pool.begin().await?;
    let row: Option<(String, bool, DateTime<Utc>)> = sqlx::query_as(
        "SELECT status, (expires_at <= now()), created_at FROM live_sessions
         WHERE id = $1 AND desk_id IS NOT NULL FOR UPDATE",
    )
    .bind(session_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((status, past_expiry, created_at)) = row else {
        return Err(AnswerError::NotFound);
    };
    match status.as_str() {
        "waiting" if past_expiry => {
            sqlx::query("UPDATE live_sessions SET status = 'expired', ended_at = now(), end_reason = 'expired' WHERE id = $1")
                .bind(session_id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Err(AnswerError::Expired);
        }
        "waiting" => {}
        "active" => return Err(AnswerError::AlreadyAnswered),
        "expired" => return Err(AnswerError::Expired),
        _ => return Err(AnswerError::Ended),
    }
    let staff_peer: Result<Uuid, sqlx::Error> = sqlx::query_scalar(
        "INSERT INTO live_participants (session_id, role, user_id, display_name, lang, consent_at)
         VALUES ($1, 'staff', $2, $3, $4, now()) RETURNING peer_id",
    )
    .bind(session_id)
    .bind(user_id)
    .bind(name)
    .bind(lang)
    .fetch_one(&mut *tx)
    .await;
    let staff_peer = match staff_peer {
        Ok(p) => p,
        Err(e) if crate::business::is_unique_violation(&e) => {
            return Err(AnswerError::AlreadyAnswered)
        }
        Err(e) => return Err(e.into()),
    };
    sqlx::query("UPDATE live_sessions SET status = 'active', started_at = now(), host_lang = $2 WHERE id = $1")
        .bind(session_id)
        .bind(lang)
        .execute(&mut *tx)
        .await?;
    let (guest_name, guest_lang): (String, String) = sqlx::query_as(
        "SELECT display_name, lang FROM live_participants WHERE session_id = $1 AND role = 'guest'",
    )
    .bind(session_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Answered {
        staff_peer,
        created_at,
        guest_name,
        guest_lang,
    })
}
