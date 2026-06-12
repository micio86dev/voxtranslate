//! Backoffice admin API. The Directus Data Studio triggers privileged actions —
//! ban, credit adjust, GDPR delete, resolve report — here via Flows, guarded by
//! the shared `ADMIN_API_SECRET` (server-to-server), NOT a user JWT. This keeps
//! money + ban logic behind the server while Directus stays a thin admin UI over
//! the data. Every action writes an `admin_audit` row.

use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{header::AUTHORIZATION, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rust_decimal::Decimal;
use serde::Deserialize;
use uuid::Uuid;

use crate::billing::usd;
use crate::db::Pool;
use crate::email::OutboundEmail;
use crate::AppState;

/// Extractor that authenticates a backoffice request by the shared
/// `ADMIN_API_SECRET`, accepting `Authorization: Bearer <secret>` or
/// `X-Admin-Secret: <secret>`. Being a `FromRequestParts` extractor it runs
/// BEFORE the JSON body is parsed, so an unauthorized caller always gets `403`
/// (never a `422` body-validation error) and the body is never deserialized.
pub struct AdminAuth;

impl FromRequestParts<AppState> for AdminAuth {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let Some(secret) = state
            .config
            .billing
            .as_ref()
            .and_then(|b| b.admin_api_secret.as_deref())
        else {
            return Err((StatusCode::SERVICE_UNAVAILABLE, "admin not configured").into_response());
        };
        let bearer = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        let xhdr = parts
            .headers
            .get("x-admin-secret")
            .and_then(|v| v.to_str().ok());
        if constant_eq(bearer, secret) || constant_eq(xhdr, secret) {
            Ok(AdminAuth)
        } else {
            Err((StatusCode::FORBIDDEN, "invalid admin secret").into_response())
        }
    }
}

/// Length-checked, branch-light comparison so a wrong secret doesn't leak its
/// length via timing. Good enough for a server-to-server shared secret.
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

/// Append one audit row. Failures are logged, never fatal to the action.
async fn audit(
    pool: &Pool,
    actor: &str,
    action: &str,
    target: Option<&str>,
    detail: serde_json::Value,
) {
    if let Err(e) = sqlx::query(
        "INSERT INTO admin_audit (actor, action, target, detail) VALUES ($1, $2, $3, $4::jsonb)",
    )
    .bind(actor)
    .bind(action)
    .bind(target)
    .bind(detail.to_string())
    .execute(pool)
    .await
    {
        tracing::error!("audit write failed: {e}");
    }
}

fn actor(a: &Option<String>) -> &str {
    a.as_deref().filter(|s| !s.is_empty()).unwrap_or("admin")
}

fn unavailable() -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, "admin not configured").into_response()
}

fn ok() -> Response {
    Json(serde_json::json!({ "ok": true })).into_response()
}

#[derive(Deserialize)]
pub struct BanRequest {
    pub user_id: Uuid,
    #[serde(default)]
    pub days: Option<i64>,
    pub reason: String,
    #[serde(default)]
    pub actor: Option<String>,
}

/// `POST /api/admin/ban` — ban a user for `days` (omit for permanent).
pub async fn ban(
    State(state): State<AppState>,
    _admin: AdminAuth,
    Json(body): Json<BanRequest>,
) -> Response {
    let (Some(safety), Some(pool)) = (state.safety.as_ref(), state.pool.as_ref()) else {
        return unavailable();
    };
    match safety.ban_user(body.user_id, &body.reason, body.days).await {
        Ok(()) => {
            audit(
                pool,
                actor(&body.actor),
                "ban",
                Some(&body.user_id.to_string()),
                serde_json::json!({ "reason": body.reason, "days": body.days }),
            )
            .await;
            ok()
        }
        Err(e) => {
            tracing::error!("ban failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "ban failed").into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct UnbanRequest {
    pub user_id: Uuid,
    #[serde(default)]
    pub actor: Option<String>,
}

/// `POST /api/admin/unban` — lift a user's ban.
pub async fn unban(
    State(state): State<AppState>,
    _admin: AdminAuth,
    Json(body): Json<UnbanRequest>,
) -> Response {
    let (Some(safety), Some(pool)) = (state.safety.as_ref(), state.pool.as_ref()) else {
        return unavailable();
    };
    match safety.unban_user(body.user_id).await {
        Ok(()) => {
            audit(
                pool,
                actor(&body.actor),
                "unban",
                Some(&body.user_id.to_string()),
                serde_json::json!({}),
            )
            .await;
            ok()
        }
        Err(e) => {
            tracing::error!("unban failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "unban failed").into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct CreditRequest {
    pub user_id: Uuid,
    /// Signed: positive grants credits, negative deducts (manual adjustment).
    pub amount: f64,
    pub reason: String,
    #[serde(default)]
    pub actor: Option<String>,
}

/// `POST /api/admin/credit` — manually adjust a user's balance (grant/refund).
pub async fn credit(
    State(state): State<AppState>,
    _admin: AdminAuth,
    Json(body): Json<CreditRequest>,
) -> Response {
    let (Some(billing), Some(pool)) = (state.billing.as_ref(), state.pool.as_ref()) else {
        return unavailable();
    };
    match billing
        .add_credits(
            body.user_id,
            usd(body.amount),
            "admin_adjust",
            Some(&body.reason),
            None,
        )
        .await
    {
        Ok(new_balance) => {
            audit(
                pool,
                actor(&body.actor),
                "credit",
                Some(&body.user_id.to_string()),
                serde_json::json!({ "amount": body.amount, "reason": body.reason }),
            )
            .await;
            Json(serde_json::json!({ "ok": true, "balance": new_balance.to_string() }))
                .into_response()
        }
        Err(e) => {
            tracing::error!("credit failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "credit failed").into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct BonusRequest {
    pub user_id: Uuid,
    /// USD amount to gift; must be positive.
    pub amount: f64,
    /// Optional note from the admin, included in the notification email.
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub actor: Option<String>,
}

/// `POST /api/admin/bonus` — gift a bonus credit (positive USD) to a user and
/// email them a notification (issue #11). The credit grant is the source of
/// truth; the email is best-effort — a send failure (or Resend not configured)
/// never blocks the grant, it just reports `email_sent: false`.
pub async fn bonus(
    State(state): State<AppState>,
    _admin: AdminAuth,
    Json(body): Json<BonusRequest>,
) -> Response {
    let (Some(billing), Some(pool)) = (state.billing.as_ref(), state.pool.as_ref()) else {
        return unavailable();
    };
    // A bonus is strictly a gift — refunds/deductions go through `/credit`.
    if !(body.amount.is_finite() && body.amount > 0.0) {
        return (StatusCode::BAD_REQUEST, "amount must be a positive number").into_response();
    }
    let message = body
        .message
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let new_balance = match billing
        .add_credits(
            body.user_id,
            usd(body.amount),
            "bonus",
            Some(message.unwrap_or("bonus credit")),
            None,
        )
        .await
    {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("bonus credit failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "bonus failed").into_response();
        }
    };

    // Notify the recipient (best-effort).
    let email_sent = send_bonus_email(
        &state,
        pool,
        body.user_id,
        body.amount,
        new_balance,
        message,
    )
    .await;

    audit(
        pool,
        actor(&body.actor),
        "bonus",
        Some(&body.user_id.to_string()),
        serde_json::json!({ "amount": body.amount, "message": message, "email_sent": email_sent }),
    )
    .await;

    Json(serde_json::json!({
        "ok": true,
        "balance": new_balance.to_string(),
        "email_sent": email_sent,
    }))
    .into_response()
}

/// Look up the recipient's contact, build the notification, and send it via
/// Resend. Returns whether an email actually went out. Never panics/blocks the
/// grant: Resend-unconfigured, user-not-found, and send errors all → `false`.
async fn send_bonus_email(
    state: &AppState,
    pool: &Pool,
    user_id: Uuid,
    amount: f64,
    new_balance: Decimal,
    message: Option<&str>,
) -> bool {
    let Some(resend) = state.resend.as_ref() else {
        return false; // email feature not configured (RESEND_* unset)
    };
    let contact: Option<(String, String)> =
        match sqlx::query_as("SELECT email, name FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(pool)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("bonus email: user lookup failed: {e}");
                return false;
            }
        };
    let Some((email, name)) = contact else {
        tracing::error!("bonus email: user {user_id} not found");
        return false;
    };
    let outbound = bonus_email(&email, &name, amount, new_balance, message);
    match resend.send(&outbound).await {
        Ok(id) => {
            tracing::info!("bonus email sent to user {user_id}: {id}");
            true
        }
        Err(e) => {
            tracing::error!("bonus email send failed: {e}");
            false
        }
    }
}

/// Build the bonus-notification email. Pure (no I/O) so it can be unit-tested.
fn bonus_email(
    to: &str,
    name: &str,
    amount: f64,
    new_balance: Decimal,
    message: Option<&str>,
) -> OutboundEmail {
    let amt = format!("${amount:.2}");
    let bal = format!("${}", new_balance.round_dp(2));
    let greet_name = name.trim();
    let greeting = if greet_name.is_empty() {
        "Hi there".to_string()
    } else {
        format!("Hi {greet_name}")
    };
    let note_text = message
        .map(|m| format!("\n\nNote from the team: {m}"))
        .unwrap_or_default();
    let note_html = message
        .map(|m| format!("<p><em>Note from the team: {}</em></p>", html_escape(m)))
        .unwrap_or_default();

    let subject = format!("🎁 You've received a {amt} bonus on VoxTranslate");
    let text = format!(
        "{greeting},\n\n\
         Great news — you've received a {amt} bonus credit on VoxTranslate.{note_text}\n\n\
         Your new balance is {bal}.\n\n\
         Jump back into a call and enjoy real-time translated conversations.\n\n\
         — The VoxTranslate team"
    );
    let html = format!(
        "<div style=\"font-family:system-ui,sans-serif;line-height:1.5\">\
         <h2>🎁 You've received a {amt} bonus!</h2>\
         <p>{greeting},</p>\
         <p>Great news — you've received a <strong>{amt}</strong> bonus credit on VoxTranslate.</p>{note_html}\
         <p>Your new balance is <strong>{bal}</strong>.</p>\
         <p>Jump back into a call and enjoy real-time translated conversations.</p>\
         <p>— The VoxTranslate team</p>\
         </div>",
        amt = html_escape(&amt),
        greeting = html_escape(&greeting),
        bal = html_escape(&bal),
        note_html = note_html,
    );

    OutboundEmail {
        to: vec![to.to_string()],
        cc: vec![],
        subject,
        html,
        text,
    }
}

/// Minimal HTML-escape for the small set of values we interpolate into the email
/// body (the admin note especially, which is free text).
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[derive(Deserialize)]
pub struct ResolveRequest {
    pub report_id: Uuid,
    /// `resolved` or `dismissed`.
    pub action: String,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub actor: Option<String>,
}

/// `POST /api/admin/report/resolve` — close a report (resolved or dismissed).
pub async fn resolve_report(
    State(state): State<AppState>,
    _admin: AdminAuth,
    Json(body): Json<ResolveRequest>,
) -> Response {
    let Some(pool) = state.pool.as_ref() else {
        return unavailable();
    };
    let status = match body.action.as_str() {
        "resolved" | "dismissed" => body.action.as_str(),
        _ => return (StatusCode::BAD_REQUEST, "action must be resolved|dismissed").into_response(),
    };
    let who = actor(&body.actor);
    match sqlx::query(
        "UPDATE reports SET status = $2, resolved_at = now(), resolved_by = $3, action_note = $4
         WHERE id = $1",
    )
    .bind(body.report_id)
    .bind(status)
    .bind(who)
    .bind(body.note.as_deref())
    .execute(pool)
    .await
    {
        Ok(r) if r.rows_affected() == 0 => {
            (StatusCode::NOT_FOUND, "no such report").into_response()
        }
        Ok(_) => {
            audit(
                pool,
                who,
                "resolve_report",
                Some(&body.report_id.to_string()),
                serde_json::json!({ "action": status, "note": body.note }),
            )
            .await;
            ok()
        }
        Err(e) => {
            tracing::error!("resolve failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "resolve failed").into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct DeleteRequest {
    pub user_id: Uuid,
    #[serde(default)]
    pub actor: Option<String>,
}

/// `POST /api/admin/user/delete` — erase a user and all linked data (GDPR).
pub async fn delete_user(
    State(state): State<AppState>,
    _admin: AdminAuth,
    Json(body): Json<DeleteRequest>,
) -> Response {
    let (Some(safety), Some(pool)) = (state.safety.as_ref(), state.pool.as_ref()) else {
        return unavailable();
    };
    // Audit BEFORE the delete — the cascade would otherwise erase nothing of the
    // audit (it's a free-standing table), but recording first is the safe order.
    audit(
        pool,
        actor(&body.actor),
        "delete_user",
        Some(&body.user_id.to_string()),
        serde_json::json!({}),
    )
    .await;
    match safety.delete_user(body.user_id).await {
        Ok(()) => ok(),
        Err(e) => {
            tracing::error!("admin delete failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "delete failed").into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{bonus_email, constant_eq, html_escape};
    use rust_decimal::Decimal;

    #[test]
    fn constant_eq_matches_only_exact() {
        assert!(constant_eq(Some("s3cret"), "s3cret"));
        assert!(!constant_eq(Some("s3cret"), "s3creT"));
        assert!(!constant_eq(Some("short"), "longer-secret"));
        assert!(!constant_eq(None, "s3cret"));
    }

    #[test]
    fn bonus_email_renders_amount_name_balance() {
        // 7.50 balance (scale 2 already); amount formats to 2 dp.
        let e = bonus_email("u@x.com", "Ada", 2.5, Decimal::new(750, 2), None);
        assert_eq!(e.to, vec!["u@x.com".to_string()]);
        assert!(e.subject.contains("$2.50"));
        assert!(e.text.contains("Hi Ada"));
        assert!(e.text.contains("$2.50") && e.text.contains("$7.50"));
        assert!(e.html.contains("$2.50") && e.html.contains("$7.50"));
        // No admin note → no note line.
        assert!(!e.text.contains("Note from the team"));
    }

    #[test]
    fn bonus_email_includes_and_escapes_message() {
        let e = bonus_email(
            "u@x.com",
            "",
            10.0,
            Decimal::new(12, 0),
            Some("thanks <3 & welcome"),
        );
        // Empty name falls back to a generic greeting.
        assert!(e.text.contains("Hi there"));
        // Balance with scale 0 still renders.
        assert!(e.text.contains("$12"));
        // The note appears; HTML variant is escaped, text variant is raw.
        assert!(e.text.contains("thanks <3 & welcome"));
        assert!(e.html.contains("thanks &lt;3 &amp; welcome"));
        assert!(!e.html.contains("thanks <3 & welcome"));
    }

    #[test]
    fn html_escape_covers_specials() {
        assert_eq!(html_escape("a<b>&\"c"), "a&lt;b&gt;&amp;&quot;c");
    }
}
