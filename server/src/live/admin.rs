//! Platform-operator administration of Live Connect (spec 0121, S15), called by Directus Flows.
//!
//! Same contract as the rest of `/api/admin/*` (see [`crate::admin`]): the shared
//! `ADMIN_API_SECRET` in `X-Admin-Secret` / `Authorization: Bearer`, checked in constant time by
//! the very same [`AdminAuth`] extractor before any body is parsed, a self-reported `actor` (the
//! Directus user's email) and the caller's IP written to `admin_audit`.
//!
//! What is different, on purpose, is what an operator may hold:
//!
//! * **No credential ever leaves here.** Regenerating a desk reports the new `token_version`
//!   and how many waiting sessions were closed — not the QR, not the MAC, not even the public
//!   handle. The org's own admin downloads the new print from the dashboard; a platform operator
//!   never has the means to impersonate a desk.
//! * **No content without a reason on record.** `session-transcript` is the one door to what was
//!   said. It demands a real reason and writes the audit row FIRST; if that write fails nothing
//!   is read. Every other endpoint here moves or reports metadata only.
//! * **Nothing is invented.** Ending a session, revoking or regenerating a desk call the same
//!   functions the owners' own endpoints call, so an operator cannot do anything an owner could
//!   not do — only on someone else's behalf, with a trace.
//!
//! The audit `detail` carries ids, counts and the operator's reason. Never tokens, QR material,
//! participant names or transcript text.
#![allow(clippy::result_large_err)]

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::finalize::{room_session_ids, room_sessions_sql};
use super::routes::api_err;
use super::sessions;
use crate::admin::AdminAuth;
use crate::db::Pool;
use crate::AppState;

/// `end_reason` written when an operator closes a conversation.
pub const ENDED_BY_ADMIN: &str = "ended_by_admin";
/// Shortest reason accepted for reading a conversation.
pub const MIN_TRANSCRIPT_REASON_CHARS: usize = 10;
/// Different non-whitespace characters a reason must contain.
const MIN_DISTINCT_REASON_CHARS: usize = 4;
/// Longest reason or actor stored.
const MAX_REASON_CHARS: usize = 500;
const MAX_ACTOR_CHARS: usize = 200;
/// A conversation is capped at two hours; this bounds the privileged read all the same. A read
/// that hits it says so (`truncated`).
const MAX_UTTERANCES: i64 = 5000;
/// Ledger `type` the org debits of Live Connect carry (see [`super::billing`]).
const ORG_LEDGER_TYPE: &str = "live_connect";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/admin/live/desk-revoke", post(desk_revoke))
        .route("/api/admin/live/desk-regenerate", post(desk_regenerate))
        .route("/api/admin/live/session-end", post(session_end))
        .route("/api/admin/live/session-reconcile", post(session_reconcile))
        .route(
            "/api/admin/live/session-transcript",
            post(session_transcript),
        )
}

// ---- shared plumbing -------------------------------------------------------------------

fn unavailable() -> Response {
    api_err(
        StatusCode::SERVICE_UNAVAILABLE,
        "admin_unavailable",
        "the database is not configured",
    )
}

fn internal(what: &str, e: impl std::fmt::Display) -> Response {
    tracing::error!(error = %e, "live admin: {what}");
    api_err(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        "the request could not be completed",
    )
}

fn bad(code: &str, message: &str) -> Response {
    api_err(StatusCode::BAD_REQUEST, code, message)
}

/// The operator's name as the Flow reports it. Required: an action nobody can be asked about
/// is not an action this API performs.
fn clean_actor(raw: &Option<String>) -> Result<String, Response> {
    let actor = raw.as_deref().map(str::trim).unwrap_or_default();
    if actor.is_empty() {
        return Err(bad("actor_required", "actor is required"));
    }
    if actor.chars().count() > MAX_ACTOR_CHARS {
        return Err(bad("actor_too_long", "actor is too long"));
    }
    if actor.chars().any(char::is_control) {
        return Err(bad(
            "actor_invalid",
            "actor must not contain control characters",
        ));
    }
    Ok(actor.to_string())
}

/// A reason a colleague could read later and understand. At least `min` characters once
/// trimmed and at least four different non-space characters, so one character repeated does not
/// pass: the point is to make the operator say something. No control characters (a NUL cannot
/// even be stored in the audit row, and a line break forges a second line in any log viewer).
fn clean_reason(raw: &Option<String>, min: usize) -> Result<String, Response> {
    let reason = raw.as_deref().map(str::trim).unwrap_or_default();
    if reason.is_empty() {
        return Err(bad("reason_required", "a reason is required"));
    }
    if reason.chars().any(char::is_control) {
        return Err(bad(
            "reason_invalid",
            "the reason must not contain control characters (line breaks, tabs, NUL)",
        ));
    }
    let len = reason.chars().count();
    let mut distinct: Vec<char> = Vec::new();
    for c in reason.chars().filter(|c| !c.is_whitespace()) {
        if !distinct.contains(&c) {
            distinct.push(c);
            if distinct.len() >= MIN_DISTINCT_REASON_CHARS {
                break;
            }
        }
    }
    if len < min || distinct.len() < MIN_DISTINCT_REASON_CHARS {
        return Err(bad(
            "reason_too_short",
            &format!(
                "the reason must say why: at least {min} characters, and at least \
                 {MIN_DISTINCT_REASON_CHARS} different non-space characters"
            ),
        ));
    }
    if len > MAX_REASON_CHARS {
        return Err(bad("reason_too_long", "the reason is too long"));
    }
    Ok(reason.to_string())
}

/// Append one audit row, reporting failure. Used where the action must not proceed (or must
/// not be answered) without its trace: handed a transaction's connection, the row commits or
/// rolls back with the action it records. `actor_ip` is folded in as the other admin routes do.
async fn audit_strict<'e, E: sqlx::PgExecutor<'e>>(
    exec: E,
    admin: &AdminAuth,
    actor: &str,
    action: &str,
    target: &str,
    mut detail: Value,
) -> Result<(), sqlx::Error> {
    if let Some(obj) = detail.as_object_mut() {
        obj.insert("actor_ip".to_string(), json!(admin.0));
    }
    sqlx::query(
        "INSERT INTO admin_audit (actor, action, target, detail) VALUES ($1, $2, $3, $4::jsonb)",
    )
    .bind(actor)
    .bind(action)
    .bind(target)
    .bind(detail.to_string())
    .execute(exec)
    .await
    .map(|_| ())
}

/// The audit of a READ that has already happened (a reconcile moves nothing): a failure is
/// logged loudly, not turned into an error. Every action that changes something writes its row
/// in the action's own transaction instead, through [`audit_strict`].
async fn audit_after(
    pool: &Pool,
    admin: &AdminAuth,
    actor: &str,
    action: &str,
    target: &str,
    detail: Value,
) {
    if let Err(e) = audit_strict(pool, admin, actor, action, target, detail).await {
        tracing::error!(action, target, error = %e, "live admin: audit write failed after the action");
    }
}

// ---- desks -----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct DeskBody {
    pub desk_id: Uuid,
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// The org a desk belongs to, or the 404 every unknown id gets.
async fn desk_org(pool: &Pool, desk_id: Uuid) -> Result<Uuid, Response> {
    sqlx::query_scalar::<_, Uuid>("SELECT org_id FROM live_desks WHERE id = $1")
        .bind(desk_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| internal("desk lookup", e))?
        .ok_or_else(|| api_err(StatusCode::NOT_FOUND, "desk_not_found", "no such desk"))
}

/// `POST /api/admin/live/desk-revoke` — switch a desk's QR off. Same effect as the org admin's
/// revoke: status `revoked`, everyone still waiting at the desk is closed. Idempotent.
pub async fn desk_revoke(
    State(state): State<AppState>,
    admin: AdminAuth,
    Json(body): Json<DeskBody>,
) -> Response {
    let Some(pool) = state.pool.as_ref() else {
        return unavailable();
    };
    let (actor, reason) = match (clean_actor(&body.actor), clean_reason(&body.reason, 3)) {
        (Ok(a), Ok(r)) => (a, r),
        (Err(e), _) | (_, Err(e)) => return e,
    };
    let org_id = match desk_org(pool, body.desk_id).await {
        Ok(o) => o,
        Err(e) => return e,
    };
    // The audit row is part of the action: written in the same transaction, so a revoke that
    // cannot be recorded does not happen, and the operator is never told it did.
    let closed = match async {
        let mut tx = pool.begin().await?;
        let closed = super::desks::revoke_in(&mut tx, body.desk_id).await?;
        audit_strict(
            &mut *tx,
            &admin,
            &actor,
            "live_desk_revoke",
            &body.desk_id.to_string(),
            json!({ "reason": reason, "org_id": org_id, "waiting_closed": closed }),
        )
        .await?;
        tx.commit().await?;
        Ok::<_, sqlx::Error>(closed)
    }
    .await
    {
        Ok(n) => n,
        Err(e) => return internal("desk revoke", e),
    };
    Json(json!({ "status": "revoked", "waiting_closed": closed })).into_response()
}

/// `POST /api/admin/live/desk-regenerate` — invalidate every printed QR of a desk and put the
/// desk back in service. The answer deliberately holds NO credential: the new QR is downloaded
/// by the org's own admin, so a platform operator never has it.
pub async fn desk_regenerate(
    State(state): State<AppState>,
    admin: AdminAuth,
    Json(body): Json<DeskBody>,
) -> Response {
    let Some(pool) = state.pool.as_ref() else {
        return unavailable();
    };
    let (actor, reason) = match (clean_actor(&body.actor), clean_reason(&body.reason, 3)) {
        (Ok(a), Ok(r)) => (a, r),
        (Err(e), _) | (_, Err(e)) => return e,
    };
    let org_id = match desk_org(pool, body.desk_id).await {
        Ok(o) => o,
        Err(e) => return e,
    };
    let (desk, closed) = match async {
        let mut tx = pool.begin().await?;
        let (desk, closed) = super::desks::regenerate_in(&mut tx, body.desk_id).await?;
        audit_strict(
            &mut *tx,
            &admin,
            &actor,
            "live_desk_regenerate",
            &body.desk_id.to_string(),
            json!({
                "reason": reason,
                "org_id": org_id,
                "token_version": desk.token_version,
                "waiting_closed": closed,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok::<_, sqlx::Error>((desk, closed))
    }
    .await
    {
        Ok(r) => r,
        Err(e) => return internal("desk regenerate", e),
    };
    Json(json!({
        "status": "active",
        "token_version": desk.token_version,
        "waiting_closed": closed,
    }))
    .into_response()
}

// ---- sessions --------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SessionBody {
    pub session_id: Uuid,
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

fn session_not_found() -> Response {
    api_err(
        StatusCode::NOT_FOUND,
        "session_not_found",
        "no such session",
    )
}

/// `(org_id, project_id, status)` of a session, or `None`.
async fn session_head(
    pool: &Pool,
    id: Uuid,
) -> Result<Option<(Option<Uuid>, Option<Uuid>, String)>, Response> {
    sqlx::query_as("SELECT org_id, project_id, status FROM live_sessions WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(|e| internal("session lookup", e))
}

/// `POST /api/admin/live/session-end` — close an open conversation (waiting or active) with
/// `end_reason = ended_by_admin` and start its finalisation, exactly as an owner ending it
/// would. The two phones are told at their next 3-second check.
pub async fn session_end(
    State(state): State<AppState>,
    admin: AdminAuth,
    Json(body): Json<SessionBody>,
) -> Response {
    let Some(pool) = state.pool.as_ref() else {
        return unavailable();
    };
    let (actor, reason) = match (clean_actor(&body.actor), clean_reason(&body.reason, 3)) {
        (Ok(a), Ok(r)) => (a, r),
        (Err(e), _) | (_, Err(e)) => return e,
    };
    let (org_id, project_id, status) = match session_head(pool, body.session_id).await {
        Ok(Some(h)) => h,
        Ok(None) => return session_not_found(),
        Err(e) => return e,
    };
    let already_over = || {
        api_err(
            StatusCode::CONFLICT,
            "session_over",
            "the session has already ended",
        )
    };
    if !matches!(status.as_str(), "waiting" | "active") {
        return already_over();
    }
    // The UPDATE is conditional on the session still being open, so of two operators (or an
    // operator and the sweeper) only one ends it; the other gets the same 409. The audit row is
    // written in the same transaction: an end that cannot be recorded does not happen.
    let ended = match async {
        let mut tx = pool.begin().await?;
        if !sessions::end_session(&mut *tx, body.session_id, ENDED_BY_ADMIN).await? {
            return Ok(false);
        }
        audit_strict(
            &mut *tx,
            &admin,
            &actor,
            "live_session_end",
            &body.session_id.to_string(),
            json!({
                "reason": reason,
                "org_id": org_id,
                "project_id": project_id,
                "was": status,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok::<_, sqlx::Error>(true)
    }
    .await
    {
        Ok(ended) => ended,
        Err(e) => return internal("session end", e),
    };
    if !ended {
        return already_over();
    }
    // The event is shown to the org's admins on the session timeline, so it holds no name.
    sessions::record_event(pool, body.session_id, "admin_ended", json!({})).await;
    super::finalize::on_session_ended(&state, body.session_id);
    Json(json!({ "status": "ended", "end_reason": ENDED_BY_ADMIN })).into_response()
}

// ---- reconciliation (read-only) --------------------------------------------------------

/// Verdict of a reconciliation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Ticks and ledger agree.
    Ok,
    /// They disagree: see `missing_ticks` / `duplicate_ticks` and the amounts.
    Mismatch,
    /// The ledger can no longer be tied to this conversation, so nothing can be proven either
    /// way. Never a guess.
    Unlinked,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Verdict::Ok => "ok",
            Verdict::Mismatch => "mismatch",
            Verdict::Unlinked => "unlinked",
        }
    }
}

/// What the tick table says, ledger-side facts excluded.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TickTotals {
    pub count: i64,
    pub org_count: i64,
    pub org_credits: Decimal,
    /// What each org tick recorded, sorted: compared one by one with the ledger's debits, so
    /// offsetting differences (a debit too small, another too large) cannot cancel in a sum.
    pub org_amounts: Vec<Decimal>,
    pub user_count: i64,
    pub user_usd: Decimal,
}

/// What the money ledger says about the same conversation.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LedgerTotals {
    /// Org debits of type `live_connect` that reference one of the conversation's call sessions.
    pub org_entries: i64,
    pub org_credits: Decimal,
    /// What each of those debits took, as a positive number, sorted.
    pub org_amounts: Vec<Decimal>,
    /// The owner's `usage_sessions` rows for the conversation's room, and what they accumulated.
    pub usage_sessions: i64,
    pub user_usd: Decimal,
}

/// How many meeting-side rows the conversation points at (even the ones that no longer exist)
/// and how many still exist. The difference is what erasure took.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RoomLinks {
    pub referenced: i64,
    pub existing: i64,
}

/// Decide, from the three sets of facts, what can be said. Pure, so the rules are testable
/// without a database. `org_paid` says which ledger the conversation's debits belong to.
pub fn judge(
    org_paid: bool,
    ticks: &TickTotals,
    ledger: &LedgerTotals,
    rooms: &RoomLinks,
) -> (Verdict, Option<i64>, Option<i64>, Vec<&'static str>) {
    let mut notes: Vec<&'static str> = Vec::new();
    let other_payer_ticks = if org_paid {
        ticks.user_count
    } else {
        ticks.org_count
    };
    if other_payer_ticks > 0 {
        notes.push("ticks exist for a payer other than the session's owner kind");
    }

    if org_paid {
        // An org debit points at the conversation only through `session_id` -> call_sessions,
        // which becomes NULL when erasure deletes the call session. If any room the
        // conversation referenced is gone (or none can be found for a conversation that
        // charged), the debits can no longer be listed: say so instead of guessing.
        let erased = rooms.existing < rooms.referenced;
        let unfindable = rooms.referenced == 0 && ticks.org_count > 0;
        if erased || unfindable {
            notes.push(
                "the meeting-side rows were erased: org ledger entries lost their session link \
                 (session_id SET NULL), so debits cannot be attributed to this conversation",
            );
            return (Verdict::Unlinked, None, None, notes);
        }
        let missing = (ticks.org_count - ledger.org_entries).max(0);
        let duplicate = (ledger.org_entries - ticks.org_count).max(0);
        // Sums AND each single amount: equal totals can hide a debit too small beside one too big.
        let amounts_agree =
            ticks.org_credits == ledger.org_credits && ticks.org_amounts == ledger.org_amounts;
        let verdict = if missing == 0 && duplicate == 0 && amounts_agree && other_payer_ticks == 0 {
            Verdict::Ok
        } else {
            Verdict::Mismatch
        };
        return (verdict, Some(missing), Some(duplicate), notes);
    }

    // Consumer-paid. The ledger (`credit_transactions`) has no column tying a debit to a
    // conversation; the link is the owner's `usage_sessions` row for the room, which the debit
    // updates in the same transaction. So only totals can be compared, never single debits.
    notes.push(
        "consumer debits cannot be counted one by one: credit_transactions carries no session \
         link, so the comparison is ticks vs usage_sessions.cost",
    );
    if ticks.user_count > 0 && ledger.usage_sessions == 0 {
        notes.push("no usage_sessions row for this room: the consumer side cannot be proven");
        return (Verdict::Unlinked, None, None, notes);
    }
    let verdict = if ticks.user_usd == ledger.user_usd && other_payer_ticks == 0 {
        Verdict::Ok
    } else {
        Verdict::Mismatch
    };
    (verdict, None, None, notes)
}

#[derive(Debug, Deserialize)]
pub struct ReconcileBody {
    pub session_id: Uuid,
    #[serde(default)]
    pub actor: Option<String>,
}

/// SQL: how many distinct `call_sessions` ids the conversation points at, whether or not the
/// rows still exist (the latest on `live_sessions`, earlier ones in `room_session` events).
const REFERENCED_ROOMS_SQL: &str = "
    SELECT count(*) FROM (
        SELECT call_session_id AS id FROM live_sessions
         WHERE id = $1 AND call_session_id IS NOT NULL
        UNION
        SELECT CASE WHEN detail->>'call_session_id'
                         ~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
                    THEN (detail->>'call_session_id')::uuid END
          FROM live_session_events
         WHERE session_id = $1 AND kind = 'room_session'
    ) refs WHERE id IS NOT NULL";

async fn load_ticks(pool: &Pool, id: Uuid) -> Result<TickTotals, sqlx::Error> {
    let rows: Vec<(String, i64, Decimal)> = sqlx::query_as(
        "SELECT payer_kind, count(*), COALESCE(sum(amount), 0)
         FROM live_usage_ticks WHERE session_id = $1 GROUP BY payer_kind",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    let org_amounts = sqlx::query_scalar(
        "SELECT amount FROM live_usage_ticks
         WHERE session_id = $1 AND payer_kind = 'org' ORDER BY amount",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    let mut t = TickTotals {
        org_amounts,
        ..Default::default()
    };
    for (kind, n, sum) in rows {
        t.count += n;
        match kind.as_str() {
            "org" => {
                t.org_count += n;
                t.org_credits += sum;
            }
            _ => {
                t.user_count += n;
                t.user_usd += sum.round_dp(6);
            }
        }
    }
    Ok(t)
}

async fn load_ledger(
    pool: &Pool,
    id: Uuid,
    owner_user: Option<Uuid>,
) -> Result<LedgerTotals, sqlx::Error> {
    let rooms = room_session_ids(pool, id).await?;
    let (org_entries, debited): (i64, i64) = sqlx::query_as(
        "SELECT count(*), COALESCE(-sum(amount), 0)::bigint
         FROM organization_credits_transactions
         WHERE type = $1 AND session_id = ANY($2)",
    )
    .bind(ORG_LEDGER_TYPE)
    .bind(&rooms)
    .fetch_one(pool)
    .await?;
    let org_amounts: Vec<i32> = sqlx::query_scalar(
        "SELECT (-amount)::int FROM organization_credits_transactions
         WHERE type = $1 AND session_id = ANY($2) ORDER BY 1",
    )
    .bind(ORG_LEDGER_TYPE)
    .bind(&rooms)
    .fetch_all(pool)
    .await?;
    let (usage_sessions, cost): (i64, Decimal) = match owner_user {
        Some(user) => {
            sqlx::query_as(
                "SELECT count(*), COALESCE(sum(cost), 0) FROM usage_sessions
                 WHERE user_id = $1 AND room = $2",
            )
            .bind(user)
            .bind(super::ws::room_name(id))
            .fetch_one(pool)
            .await?
        }
        None => (0, Decimal::ZERO),
    };
    Ok(LedgerTotals {
        org_entries,
        org_credits: Decimal::from(debited),
        org_amounts: org_amounts.into_iter().map(Decimal::from).collect(),
        usage_sessions,
        user_usd: cost.round_dp(6),
    })
}

/// `POST /api/admin/live/session-reconcile` — compare what the metering recorded
/// (`live_usage_ticks`) with what was actually debited, and say whether they agree.
///
/// READ-ONLY: it never repairs. Money corrections are a human decision made with the
/// existing `credit` endpoint, with this report in hand.
///
/// What it can prove: an org conversation whose rooms still exist (every debit then names the
/// conversation, so debits and ticks can be counted and summed), and a consumer conversation
/// whose `usage_sessions` row still exists (totals only). What it cannot: anything once erasure
/// deleted the meeting-side rows — the ledger link is `SET NULL` and the debits are
/// indistinguishable from the org's others — which it reports as `unlinked`.
pub async fn session_reconcile(
    State(state): State<AppState>,
    admin: AdminAuth,
    Json(body): Json<ReconcileBody>,
) -> Response {
    let Some(pool) = state.pool.as_ref() else {
        return unavailable();
    };
    // The actor is optional here (a read), but one the audit row cannot hold is refused rather
    // than silently left unrecorded.
    let actor = match body
        .actor
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
    {
        None => "admin".to_string(),
        Some(_) => match clean_actor(&body.actor) {
            Ok(a) => a,
            Err(e) => return e,
        },
    };
    let head: Option<(Option<Uuid>, Option<Uuid>)> =
        match sqlx::query_as("SELECT org_id, owner_user_id FROM live_sessions WHERE id = $1")
            .bind(body.session_id)
            .fetch_optional(pool)
            .await
        {
            Ok(h) => h,
            Err(e) => return internal("reconcile lookup", e),
        };
    let Some((org_id, owner_user)) = head else {
        return session_not_found();
    };

    let loaded = async {
        let ticks = load_ticks(pool, body.session_id).await?;
        let ledger = load_ledger(pool, body.session_id, owner_user).await?;
        let (referenced,): (i64,) = sqlx::query_as(REFERENCED_ROOMS_SQL)
            .bind(body.session_id)
            .fetch_one(pool)
            .await?;
        let (existing,): (i64,) = sqlx::query_as(&format!(
            "SELECT count(*) FROM ({}) r",
            room_sessions_sql("$1")
        ))
        .bind(body.session_id)
        .fetch_one(pool)
        .await?;
        Ok::<_, sqlx::Error>((
            ticks,
            ledger,
            RoomLinks {
                referenced,
                existing,
            },
        ))
    }
    .await;
    let (ticks, ledger, rooms) = match loaded {
        Ok(v) => v,
        Err(e) => return internal("reconcile", e),
    };

    let org_paid = org_id.is_some();
    let (verdict, missing, duplicate, notes) = judge(org_paid, &ticks, &ledger, &rooms);
    audit_after(
        pool,
        &admin,
        &actor,
        "live_session_reconcile",
        &body.session_id.to_string(),
        json!({ "status": verdict.as_str(), "missing_ticks": missing, "duplicate_ticks": duplicate }),
    )
    .await;

    Json(json!({
        "session_id": body.session_id,
        "payer": if org_paid { "org" } else { "user" },
        "status": verdict.as_str(),
        "ticks": {
            "org_credits": ticks.org_credits.normalize().to_string(),
            "user_usd": ticks.user_usd.normalize().to_string(),
            "count": ticks.count,
        },
        "ledger": {
            "org_credits": ledger.org_credits.normalize().to_string(),
            "org_entries": ledger.org_entries,
            "user_usd": ledger.user_usd.normalize().to_string(),
            "usage_sessions": ledger.usage_sessions,
        },
        // Ticks recorded for which no debit exists / debits that no tick accounts for (a
        // charge the idempotency ledger never saw). `null` where single debits cannot be counted.
        "missing_ticks": missing,
        "duplicate_ticks": duplicate,
        "notes": notes,
    }))
    .into_response()
}

// ---- the privileged read ---------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct UtteranceRow {
    ts: DateTime<Utc>,
    speaker_peer_id: String,
    speaker_name: String,
    original_lang: String,
    original_text: String,
    translations: Value,
}

/// `POST /api/admin/live/session-transcript` — what was said in one conversation.
///
/// The only way for the platform to read conversation content. The reason must be real, and
/// the audit row is written BEFORE anything is read: if it cannot be written the operator gets
/// an error and no content. The audit holds who, which session and why — never the content.
pub async fn session_transcript(
    State(state): State<AppState>,
    admin: AdminAuth,
    Json(body): Json<SessionBody>,
) -> Response {
    let Some(pool) = state.pool.as_ref() else {
        return unavailable();
    };
    let (actor, reason) = match (
        clean_actor(&body.actor),
        clean_reason(&body.reason, MIN_TRANSCRIPT_REASON_CHARS),
    ) {
        (Ok(a), Ok(r)) => (a, r),
        (Err(e), _) | (_, Err(e)) => return e,
    };
    // Existence is metadata; checking it first means an unknown id leaves no "read" on record.
    let (org_id, project_id, _) = match session_head(pool, body.session_id).await {
        Ok(Some(h)) => h,
        Ok(None) => return session_not_found(),
        Err(e) => return e,
    };

    // Fail closed: no audit row, no content.
    if let Err(e) = audit_strict(
        pool,
        &admin,
        &actor,
        "live_session_transcript_read",
        &body.session_id.to_string(),
        json!({ "reason": reason, "org_id": org_id, "project_id": project_id }),
    )
    .await
    {
        return internal("transcript audit", e);
    }

    match read_transcript(pool, body.session_id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => internal("transcript read", e),
    }
}

/// The conversation as the dashboard's session detail shows it: participants, and every
/// utterance with the original kept apart from its translations. Same query shape as
/// [`super::dashboard::session_detail`] (the room-session set is the shared
/// [`room_sessions_sql`]), minus the org-role gating that does not apply here.
async fn read_transcript(pool: &Pool, id: Uuid) -> Result<Value, sqlx::Error> {
    let head: (
        String,
        Option<Uuid>,
        Option<Uuid>,
        Option<Uuid>,
        String,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT status, org_id, project_id, desk_id, host_lang, end_reason
             FROM live_sessions WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await?;
    let participants: Vec<(Uuid, String, String, String)> = sqlx::query_as(
        "SELECT peer_id, role, display_name, lang FROM live_participants
         WHERE session_id = $1 ORDER BY joined_at",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    let events: Vec<UtteranceRow> = sqlx::query_as(&format!(
        "SELECT te.ts, te.speaker_peer_id, te.speaker_name, te.original_lang, te.original_text, te.translations
         FROM transcript_events te
         WHERE te.event_type = 'speech' AND te.session_id IN ({})
         ORDER BY te.ts
         LIMIT {}",
        room_sessions_sql("$1"),
        MAX_UTTERANCES + 1
    ))
    .bind(id)
    .fetch_all(pool)
    .await?;
    // One row past the bound is read only to know whether the bound cut something off.
    let truncated = events.len() as i64 > MAX_UTTERANCES;
    let events = &events[..events.len().min(MAX_UTTERANCES as usize)];
    let first = events.first().map(|e| e.ts);
    let utterances: Vec<Value> = events
        .iter()
        .map(|e| {
            json!({
                "at": e.ts,
                "offset_ms": first.map(|f| (e.ts - f).num_milliseconds()),
                "speaker_id": e.speaker_peer_id,
                "speaker_name": e.speaker_name,
                "original_lang": e.original_lang,
                "original": e.original_text,
                "translations": e.translations,
            })
        })
        .collect();
    Ok(json!({
        "session": {
            "id": id,
            "status": head.0,
            "org_id": head.1,
            "project_id": head.2,
            "desk_id": head.3,
            "host_lang": head.4,
            "end_reason": head.5,
        },
        "participants": participants.iter().map(|(peer_id, role, name, lang)| {
            json!({ "peer_id": peer_id, "role": role, "display_name": name, "lang": lang })
        }).collect::<Vec<_>>(),
        "utterances": utterances,
        // `true` when the conversation has more utterances than are returned (the earliest
        // `MAX_UTTERANCES` are): the operator must not take a cut transcript for the whole one.
        "truncated": truncated,
        "audited": true,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(n: i64) -> Decimal {
        Decimal::from(n)
    }

    fn rooms(referenced: i64, existing: i64) -> RoomLinks {
        RoomLinks {
            referenced,
            existing,
        }
    }

    #[test]
    fn reasons_must_say_something() {
        let some = |s: &str| Some(s.to_string());
        assert!(clean_reason(&None, 3).is_err());
        assert!(clean_reason(&some("   "), 3).is_err());
        assert!(clean_reason(&some("short"), 10).is_err());
        assert!(clean_reason(&some("aaaaaaaaaaaa"), 10).is_err());
        assert!(clean_reason(&some("a a a a a a a a"), 10).is_err());
        assert_eq!(
            clean_reason(&some("  customer asked for a copy  "), 10).ok(),
            Some("customer asked for a copy".to_string())
        );
        assert!(clean_reason(&Some("x".repeat(501)), 10).is_err());
    }

    #[test]
    fn reasons_and_actors_refuse_control_characters_and_say_the_real_rule() {
        let some = |s: &str| Some(s.to_string());
        for bad in ["a\0b", "a\nb", "a\tb", "a\u{7f}b", "a\u{1b}[0mb"] {
            let reason = format!("customer asked for {bad} a copy");
            assert!(clean_reason(&some(&reason), 3).is_err(), "{bad:?}");
            assert!(clean_actor(&some(bad)).is_err(), "{bad:?}");
        }
        // Edge whitespace is trimmed, not refused.
        assert!(clean_actor(&some("  operator@x.test\n")).is_ok());
        // A repeated character is refused too (its message is pinned over HTTP).
        let res = clean_reason(&some("aaaaaaaaaaaa"), 3).unwrap_err();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn equal_totals_with_different_single_amounts_still_mismatch() {
        // Two ticks of 2 against debits of 1 and 3: the sums agree, the books do not.
        let ticks = TickTotals {
            count: 2,
            org_count: 2,
            org_credits: d(4),
            org_amounts: vec![d(2), d(2)],
            ..Default::default()
        };
        let off = LedgerTotals {
            org_entries: 2,
            org_credits: d(4),
            org_amounts: vec![d(1), d(3)],
            ..Default::default()
        };
        assert_eq!(judge(true, &ticks, &off, &rooms(1, 1)).0, Verdict::Mismatch);
        let right = LedgerTotals {
            org_amounts: vec![d(2), d(2)],
            ..off
        };
        assert_eq!(judge(true, &ticks, &right, &rooms(1, 1)).0, Verdict::Ok);
    }

    #[test]
    fn an_org_conversation_that_agrees_is_ok() {
        let ticks = TickTotals {
            count: 3,
            org_count: 3,
            org_credits: d(6),
            ..Default::default()
        };
        let ledger = LedgerTotals {
            org_entries: 3,
            org_credits: d(6),
            ..Default::default()
        };
        let (v, missing, dup, _) = judge(true, &ticks, &ledger, &rooms(1, 1));
        assert_eq!((v, missing, dup), (Verdict::Ok, Some(0), Some(0)));
    }

    #[test]
    fn a_tick_without_a_debit_and_a_debit_without_a_tick_are_told_apart() {
        let ticks = TickTotals {
            count: 3,
            org_count: 3,
            org_credits: d(6),
            ..Default::default()
        };
        let fewer = LedgerTotals {
            org_entries: 2,
            org_credits: d(4),
            ..Default::default()
        };
        let (v, missing, dup, _) = judge(true, &ticks, &fewer, &rooms(1, 1));
        assert_eq!((v, missing, dup), (Verdict::Mismatch, Some(1), Some(0)));
        let more = LedgerTotals {
            org_entries: 4,
            org_credits: d(8),
            ..Default::default()
        };
        let (v, missing, dup, _) = judge(true, &ticks, &more, &rooms(1, 1));
        assert_eq!((v, missing, dup), (Verdict::Mismatch, Some(0), Some(1)));
    }

    #[test]
    fn equal_counts_with_different_amounts_still_mismatch() {
        let ticks = TickTotals {
            count: 2,
            org_count: 2,
            org_credits: d(4),
            ..Default::default()
        };
        let ledger = LedgerTotals {
            org_entries: 2,
            org_credits: d(5),
            ..Default::default()
        };
        assert_eq!(
            judge(true, &ticks, &ledger, &rooms(1, 1)).0,
            Verdict::Mismatch
        );
    }

    #[test]
    fn erased_rooms_are_unlinked_never_guessed() {
        let ticks = TickTotals {
            count: 2,
            org_count: 2,
            org_credits: d(4),
            ..Default::default()
        };
        let none = LedgerTotals::default();
        // The rows were deleted: the ledger link is NULL, nothing can be said.
        assert_eq!(
            judge(true, &ticks, &none, &rooms(1, 0)).0,
            Verdict::Unlinked
        );
        // A conversation that charged but points at no room at all.
        assert_eq!(
            judge(true, &ticks, &none, &rooms(0, 0)).0,
            Verdict::Unlinked
        );
        // One that never charged and never had a room is simply fine.
        assert_eq!(
            judge(true, &TickTotals::default(), &none, &rooms(0, 0)).0,
            Verdict::Ok
        );
    }

    #[test]
    fn a_consumer_conversation_compares_totals_only() {
        let ticks = TickTotals {
            count: 2,
            user_count: 2,
            user_usd: Decimal::new(10, 2),
            ..Default::default()
        };
        let ledger = LedgerTotals {
            usage_sessions: 1,
            user_usd: Decimal::new(10, 2),
            ..Default::default()
        };
        let (v, missing, dup, _) = judge(false, &ticks, &ledger, &rooms(1, 1));
        assert_eq!((v, missing, dup), (Verdict::Ok, None, None));
        let off = LedgerTotals {
            usage_sessions: 1,
            user_usd: Decimal::new(15, 2),
            ..Default::default()
        };
        assert_eq!(
            judge(false, &ticks, &off, &rooms(1, 1)).0,
            Verdict::Mismatch
        );
        let gone = LedgerTotals::default();
        assert_eq!(
            judge(false, &ticks, &gone, &rooms(1, 1)).0,
            Verdict::Unlinked
        );
    }
}
