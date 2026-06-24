//! Members, invites, and role management (spec 0106).
//!
//! Invites carry a secret token (the bearer). Anyone authenticated who holds the
//! token can accept it. Owners can't be removed or re-roled through these routes
//! (that protects `organizations.owner_id`); ownership transfer is out of scope.

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

use crate::ai::email_draft::valid_email;
use crate::business::{
    audit, bad_request, db_err, forbidden, not_found, require_pool, require_role, role_rank, ADMIN,
    MEMBER, OWNER,
};
use crate::email::OutboundEmail;
use crate::email_template::{render_html, render_text, tagline, EmailButton, EmailLayout};
use crate::middleware::AuthUser;
use crate::AppState;

/// Roles assignable via invite / role-change. `owner` is excluded on purpose.
const ASSIGNABLE_ROLES: [&str; 3] = ["admin", "member", "guest"];

#[derive(FromRow, Serialize)]
struct MemberRow {
    user_id: Uuid,
    role: String,
    name: String,
    email: String,
    avatar_url: Option<String>,
    joined_at: DateTime<Utc>,
}

#[derive(Deserialize)]
pub struct CreateInvite {
    email: String,
    #[serde(default)]
    role: Option<String>,
}

#[derive(Deserialize)]
pub struct ChangeRole {
    role: String,
}

#[derive(FromRow)]
struct InviteView {
    org_name: String,
    email: String,
    role: String,
    expires_at: DateTime<Utc>,
    accepted_at: Option<DateTime<Utc>>,
}

#[derive(FromRow)]
struct InvitePending {
    id: Uuid,
    org_id: Uuid,
    role: String,
    accepted_at: Option<DateTime<Utc>>,
    expires_at: DateTime<Utc>,
}

/// `GET /api/business/organizations/{org_id}/members` — list members (any member).
pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;
    let rows: Vec<MemberRow> = sqlx::query_as(
        "SELECT m.user_id, m.role, u.name, u.email, u.avatar_url, m.joined_at
         FROM organization_members m
         JOIN users u ON u.id = m.user_id
         WHERE m.org_id = $1
         ORDER BY m.joined_at",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    Ok(Json(rows).into_response())
}

/// `POST /api/business/organizations/{org_id}/invites` — invite by email (owner/admin).
pub async fn create_invite(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Json(body): Json<CreateInvite>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;
    if !state
        .rate_limiter
        .allow(&format!("org-invite:{org_id}"), 20, Duration::from_secs(60))
    {
        return Err((StatusCode::TOO_MANY_REQUESTS, "too many invites").into_response());
    }

    let email = body.email.trim().to_lowercase();
    if !valid_email(&email) {
        return Err(bad_request("invalid email address"));
    }
    let role = match body.role.as_deref() {
        None => "member",
        Some(r) if ASSIGNABLE_ROLES.contains(&r) => r,
        Some(_) => return Err(bad_request("role must be admin, member, or guest")),
    };

    let org_name: String = sqlx::query_scalar("SELECT name FROM organizations WHERE id = $1")
        .bind(org_id)
        .fetch_one(pool)
        .await
        .map_err(db_err)?;

    let (invite_id, token, expires_at): (Uuid, String, DateTime<Utc>) = sqlx::query_as(
        "INSERT INTO organization_invites (org_id, email, role, invited_by)
         VALUES ($1, $2, $3, $4)
         RETURNING id, token, expires_at",
    )
    .bind(org_id)
    .bind(&email)
    .bind(role)
    .bind(user.user_id)
    .fetch_one(pool)
    .await
    .map_err(db_err)?;

    // Invites land on the dashboard (a separate origin from the call app), at its
    // locale-prefixed, trailing-slash join route — not `app_base_url/join`, which
    // is the consumer app and has no such page (it 404s).
    let join_url = format!(
        "{}/en/join/?token={}",
        state.config.dashboard_base_url.trim_end_matches('/'),
        token
    );
    let mut email_sent = false;
    if let Some(resend) = state.resend.as_ref() {
        let msg = build_invite_email(
            &email,
            &user.name,
            &org_name,
            role,
            &join_url,
            &state.config.app_base_url,
        );
        match resend.send(&msg).await {
            Ok(_) => email_sent = true,
            Err(e) => tracing::error!("org invite email failed: {e}"),
        }
    }

    audit::log_audit_event(
        pool,
        org_id,
        user.user_id,
        "member.invite",
        "invite",
        invite_id,
        json!({ "email": email, "role": role }),
    );

    Ok(Json(json!({
        "id": invite_id,
        "email": email,
        "role": role,
        "token": token,
        "expires_at": expires_at,
        "join_url": join_url,
        "email_sent": email_sent,
    }))
    .into_response())
}

/// `GET /api/business/invites/{token}` — invite info (public, but the token is secret).
pub async fn get_invite(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let row: Option<InviteView> = sqlx::query_as(
        "SELECT o.name AS org_name, i.email, i.role, i.expires_at, i.accepted_at
         FROM organization_invites i
         JOIN organizations o ON o.id = i.org_id
         WHERE i.token = $1",
    )
    .bind(&token)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    let inv = row.ok_or_else(|| not_found("invite not found"))?;
    if inv.accepted_at.is_some() {
        return Err((StatusCode::GONE, "invite already accepted").into_response());
    }
    if inv.expires_at < Utc::now() {
        return Err((StatusCode::GONE, "invite expired").into_response());
    }
    Ok(Json(json!({
        "org_name": inv.org_name,
        "email": inv.email,
        "role": inv.role,
        "expires_at": inv.expires_at,
    }))
    .into_response())
}

/// `POST /api/business/invites/{token}/accept` — redeem an invite (authenticated).
pub async fn accept_invite(
    State(state): State<AppState>,
    user: AuthUser,
    Path(token): Path<String>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let mut tx = pool.begin().await.map_err(db_err)?;
    let row: Option<InvitePending> = sqlx::query_as(
        "SELECT id, org_id, role, accepted_at, expires_at
         FROM organization_invites WHERE token = $1 FOR UPDATE",
    )
    .bind(&token)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err)?;
    let inv = row.ok_or_else(|| not_found("invite not found"))?;
    if inv.accepted_at.is_some() {
        return Err((StatusCode::GONE, "invite already accepted").into_response());
    }
    if inv.expires_at < Utc::now() {
        return Err((StatusCode::GONE, "invite expired").into_response());
    }

    sqlx::query(
        "INSERT INTO organization_members (org_id, user_id, role, invited_by)
         VALUES ($1, $2, $3, (SELECT invited_by FROM organization_invites WHERE id = $4))
         ON CONFLICT (org_id, user_id) DO NOTHING",
    )
    .bind(inv.org_id)
    .bind(user.user_id)
    .bind(&inv.role)
    .bind(inv.id)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    sqlx::query("UPDATE organization_invites SET accepted_at = now() WHERE id = $1")
        .bind(inv.id)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    tx.commit().await.map_err(db_err)?;

    Ok(Json(json!({ "org_id": inv.org_id, "role": inv.role })).into_response())
}

/// `DELETE /api/business/organizations/{org_id}/members/{user_id}` — remove (owner/admin) or self-leave.
pub async fn remove(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, target)): Path<(Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let caller_role = require_role(pool, org_id, user.user_id, MEMBER).await?;
    let self_leave = target == user.user_id;
    if !self_leave && role_rank(&caller_role) < ADMIN {
        return Err(forbidden("only owners/admins can remove members"));
    }

    // The owner can never be removed here (protects organizations.owner_id).
    let res = sqlx::query(
        "DELETE FROM organization_members WHERE org_id = $1 AND user_id = $2 AND role <> 'owner'",
    )
    .bind(org_id)
    .bind(target)
    .execute(pool)
    .await
    .map_err(db_err)?;
    if res.rows_affected() == 0 {
        let is_owner: Option<bool> = sqlx::query_scalar(
            "SELECT role = 'owner' FROM organization_members WHERE org_id = $1 AND user_id = $2",
        )
        .bind(org_id)
        .bind(target)
        .fetch_optional(pool)
        .await
        .map_err(db_err)?;
        return Err(match is_owner {
            Some(true) => forbidden("cannot remove the organization owner"),
            _ => not_found("member not found"),
        });
    }

    audit::log_audit_event(
        pool,
        org_id,
        user.user_id,
        "member.remove",
        "member",
        target,
        json!({ "self_leave": self_leave }),
    );
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `PATCH /api/business/organizations/{org_id}/members/{user_id}` — change role (owner only).
pub async fn change_role(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, target)): Path<(Uuid, Uuid)>,
    Json(body): Json<ChangeRole>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, OWNER).await?;
    let role = body.role.as_str();
    if !ASSIGNABLE_ROLES.contains(&role) {
        return Err(bad_request("role must be admin, member, or guest"));
    }

    // Can't re-role the owner (their role is the source of truth for ownership).
    let res = sqlx::query(
        "UPDATE organization_members SET role = $3
         WHERE org_id = $1 AND user_id = $2 AND role <> 'owner'",
    )
    .bind(org_id)
    .bind(target)
    .bind(role)
    .execute(pool)
    .await
    .map_err(db_err)?;
    if res.rows_affected() == 0 {
        return Err(not_found("member not found"));
    }

    audit::log_audit_event(
        pool,
        org_id,
        user.user_id,
        "member.role_change",
        "member",
        target,
        json!({ "role": role }),
    );
    Ok(Json(json!({ "user_id": target, "role": role })).into_response())
}

/// Build a branded, English org-invite email (reuses the shared email shell).
fn build_invite_email(
    to: &str,
    inviter: &str,
    org: &str,
    role: &str,
    join_url: &str,
    app_base_url: &str,
) -> OutboundEmail {
    let inviter = match inviter.trim() {
        "" => "A teammate",
        n => n,
    };
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    let subject = format!("{inviter} invited you to join {org} on VoxTranslate");
    let body_html = format!(
        "<p style=\"margin:0 0 16px;\">{inviter} has invited you to join \
         <strong>{org}</strong> on VoxTranslate as <strong>{role}</strong>.</p>\
         <p style=\"margin:0 0 4px;\">VoxTranslate gives your team real-time translated \
         video calls, shared call history, and transcripts.</p>",
        inviter = esc(inviter),
        org = esc(org),
        role = esc(role),
    );
    let html = render_html(&EmailLayout {
        app_base_url,
        preheader: "You've been invited to a VoxTranslate workspace.",
        heading: Some("You're invited to a workspace"),
        body_html: &body_html,
        button: Some(EmailButton {
            label: "Accept invitation",
            url: join_url,
        }),
        tagline: tagline("en"),
    });
    let body_text = format!(
        "{inviter} has invited you to join {org} on VoxTranslate as {role}.\n\n\
         VoxTranslate gives your team real-time translated video calls, shared call \
         history, and transcripts."
    );
    let text = render_text(
        &body_text,
        Some(&EmailButton {
            label: "Accept invitation",
            url: join_url,
        }),
        app_base_url,
        tagline("en"),
    );
    OutboundEmail {
        to: vec![to.to_string()],
        cc: vec![],
        subject,
        html,
        text,
    }
}
