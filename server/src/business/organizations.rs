//! Organization CRUD (spec 0106). The creator becomes the `owner` member.

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::FromRow;
use uuid::Uuid;

use crate::business::{
    bad_request, conflict, db_err, forbidden, is_unique_violation, not_found, require_pool,
    require_role, ADMIN, MEMBER,
};
use crate::middleware::AuthUser;
use crate::AppState;

#[derive(Deserialize)]
pub struct CreateOrg {
    pub name: String,
    pub slug: String,
    #[serde(default)]
    pub plan: Option<String>,
}

#[derive(Deserialize)]
pub struct PatchOrg {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub settings: Option<serde_json::Value>,
}

#[derive(FromRow, Serialize)]
struct OrgSummary {
    id: Uuid,
    name: String,
    slug: String,
    plan: String,
    /// 'none' | 'active' | 'past_due' | 'canceled', exactly as stored. Reported
    /// verbatim: it is what Stripe last told us, and clients that show lifecycle
    /// copy still need it.
    subscription_status: String,
    /// Whether the subscription is live RIGHT NOW — the stored status AND an
    /// unexpired period, the same rule the server gates on
    /// ([`crate::business::credits::SUBSCRIPTION_ACTIVE_SQL`]). Gate on this, not
    /// on `subscription_status`: a gifted subscription lapses by date with no
    /// webhook to change its status, so the two disagree and only this one is
    /// true.
    subscription_active: bool,
    /// End of the paid period, so the UI can say *when* it lapsed or renews.
    current_period_end: Option<chrono::DateTime<chrono::Utc>>,
    credits_balance: i32,
    role: String,
}

/// Accept a slug only if it's a clean subdomain label (the future `slug.voxtranslate.app`).
fn valid_slug(raw: &str) -> Option<String> {
    let s = raw.trim().to_ascii_lowercase();
    if s.len() < 2 || s.len() > 40 || s.starts_with('-') || s.ends_with('-') {
        return None;
    }
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
        .then_some(s)
}

fn valid_plan(raw: &Option<String>) -> Option<&'static str> {
    match raw.as_deref() {
        None | Some("business") => Some("business"),
        Some("enterprise") => Some("enterprise"),
        _ => None,
    }
}

fn valid_name(raw: &str) -> Option<&str> {
    let n = raw.trim();
    (!n.is_empty() && n.chars().count() <= 120).then_some(n)
}

/// `POST /api/business/organizations` — create an org; creator becomes `owner`.
pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<CreateOrg>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let name =
        valid_name(&body.name).ok_or_else(|| bad_request("name is required (max 120 chars)"))?;
    let slug = valid_slug(&body.slug)
        .ok_or_else(|| bad_request("slug must be 2–40 chars: a–z, 0–9, hyphens"))?;
    let plan = valid_plan(&body.plan)
        .ok_or_else(|| bad_request("plan must be 'business' or 'enterprise'"))?;

    let mut tx = pool.begin().await.map_err(db_err)?;
    let org_id: Uuid = match sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, plan, owner_id) VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(name)
    .bind(&slug)
    .bind(plan)
    .bind(user.user_id)
    .fetch_one(&mut *tx)
    .await
    {
        Ok(id) => id,
        Err(e) if is_unique_violation(&e) => return Err(conflict("slug already taken")),
        Err(e) => return Err(db_err(e)),
    };
    sqlx::query(
        "INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(org_id)
    .bind(user.user_id)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    tx.commit().await.map_err(db_err)?;

    Ok(Json(json!({
        "id": org_id,
        "name": name,
        "slug": slug,
        "plan": plan,
        "credits_balance": 0,
        "role": "owner",
    }))
    .into_response())
}

/// `GET /api/business/organizations` — the caller's orgs with their role.
pub async fn list_mine(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let rows: Vec<OrgSummary> = sqlx::query_as(&format!(
        "SELECT o.id, o.name, o.slug, o.plan, o.subscription_status,
                {active} AS subscription_active, o.current_period_end,
                o.credits_balance, m.role
         FROM organizations o
         JOIN organization_members m ON m.org_id = o.id
         WHERE m.user_id = $1
         ORDER BY o.created_at",
        active = crate::business::credits::SUBSCRIPTION_ACTIVE_SQL,
    ))
    .bind(user.user_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    Ok(Json(rows).into_response())
}

/// `GET /api/business/organizations/{org_id}` — org detail (members only).
pub async fn get(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let role = require_role(pool, org_id, user.user_id, MEMBER).await?;
    let row: Option<(String, String, String, i32, serde_json::Value)> = sqlx::query_as(
        "SELECT name, slug, plan, credits_balance, settings FROM organizations WHERE id = $1",
    )
    .bind(org_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    let (name, slug, plan, credits, settings) =
        row.ok_or_else(|| not_found("organization not found"))?;
    Ok(Json(json!({
        "id": org_id,
        "name": name,
        "slug": slug,
        "plan": plan,
        "credits_balance": credits,
        "settings": settings,
        "role": role,
    }))
    .into_response())
}

/// `PATCH /api/business/organizations/{org_id}` — update name/settings (owner/admin).
pub async fn patch(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Json(body): Json<PatchOrg>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;

    let name = match &body.name {
        Some(n) => Some(
            valid_name(n)
                .ok_or_else(|| bad_request("invalid name"))?
                .to_string(),
        ),
        None => None,
    };
    if let Some(s) = &body.settings {
        if !s.is_object() {
            return Err(bad_request("settings must be a JSON object"));
        }
    }

    // Compliance mode (and its audit log) is an Enterprise feature — a business-plan
    // org may not enable it.
    let enabling_compliance = body
        .settings
        .as_ref()
        .and_then(|s| s.get("compliance_mode"))
        .and_then(|v| v.as_bool())
        == Some(true);
    if enabling_compliance {
        let plan: String = sqlx::query_scalar("SELECT plan FROM organizations WHERE id = $1")
            .bind(org_id)
            .fetch_one(pool)
            .await
            .map_err(db_err)?;
        if plan != "enterprise" {
            return Err(forbidden(
                "compliance mode is available on the Enterprise plan",
            ));
        }
    }

    let updated: Option<(String, String, String, i32, serde_json::Value)> = sqlx::query_as(
        "UPDATE organizations
         SET name = COALESCE($2, name),
             settings = COALESCE($3::jsonb, settings),
             updated_at = now()
         WHERE id = $1
         RETURNING name, slug, plan, credits_balance, settings",
    )
    .bind(org_id)
    .bind(name)
    .bind(body.settings)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    let (name, slug, plan, credits, settings) =
        updated.ok_or_else(|| not_found("organization not found"))?;
    Ok(Json(json!({
        "id": org_id,
        "name": name,
        "slug": slug,
        "plan": plan,
        "credits_balance": credits,
        "settings": settings,
    }))
    .into_response())
}
