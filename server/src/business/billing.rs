//! Org billing (spec 0106): auto-renewing Stripe subscriptions, the Billing
//! Customer Portal, one-off credit top-ups, and the webhook that keeps the
//! subscription state + credit pool in sync.
//!
//! UX: subscriptions are started via Stripe Checkout (`mode=subscription`) and
//! managed via the Stripe-hosted Customer Portal (cancel auto-renew, switch plan,
//! update card) — no custom billing UI, and renewals are automatic. This webhook
//! is a SEPARATE endpoint from the consumer `/api/billing/webhook` and never
//! touches it; idempotency rides on the shared `stripe_events` table.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::FromRow;
use uuid::Uuid;

use crate::business::{bad_request, db_err, require_pool, require_role, ADMIN, OWNER};
use crate::config::{BillingConfig, OrgBillingConfig};
use crate::db::Pool;
use crate::middleware::AuthUser;
use crate::{stripe_handler, AppState};

#[derive(Deserialize)]
pub struct SubscribeReq {
    plan: String,
    interval: String,
}

#[derive(Deserialize)]
pub struct PurchaseReq {
    credits_amount: i32,
}

#[derive(FromRow, Serialize)]
struct LedgerRow {
    amount: i32,
    #[sqlx(rename = "type")]
    #[serde(rename = "type")]
    kind: String,
    description: Option<String>,
    created_at: DateTime<Utc>,
}

fn svc_unavailable(msg: &str) -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, msg.to_string()).into_response()
}

fn bad_gateway() -> Response {
    (StatusCode::BAD_GATEWAY, "payment provider error").into_response()
}

/// Billing + org-billing config, or 503 when either isn't configured.
fn billing_and_org(state: &AppState) -> Result<(&BillingConfig, &OrgBillingConfig), Response> {
    let billing = state
        .config
        .billing
        .as_ref()
        .ok_or_else(|| svc_unavailable("billing not configured"))?;
    if billing.stripe_secret_key.is_empty() {
        return Err(svc_unavailable("payments not configured"));
    }
    let org = billing
        .org_billing
        .as_ref()
        .ok_or_else(|| svc_unavailable("org billing not configured"))?;
    Ok((billing, org))
}

/// `POST /api/business/organizations/{org_id}/subscription` — start an
/// auto-renewing subscription via Stripe Checkout (owner).
pub async fn subscribe(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Json(body): Json<SubscribeReq>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, OWNER).await?;
    let (billing, org_cfg) = billing_and_org(&state)?;

    let plan = match body.plan.as_str() {
        p @ ("business" | "enterprise") => p,
        _ => return Err(bad_request("plan must be business or enterprise")),
    };
    let interval = match body.interval.as_str() {
        i @ ("month" | "year") => i,
        _ => return Err(bad_request("interval must be month or year")),
    };
    let price = org_cfg
        .price_id(plan, interval)
        .ok_or_else(|| svc_unavailable("that plan/interval is not available"))?;

    let url = stripe_handler::create_org_subscription_checkout(
        &state.http,
        billing,
        org_cfg,
        &org_id,
        price,
        plan,
        interval,
    )
    .await
    .map_err(|e| {
        tracing::error!("org subscribe checkout failed: {e}");
        bad_gateway()
    })?;
    Ok(Json(json!({ "url": url })).into_response())
}

/// `POST /api/business/organizations/{org_id}/subscription/portal` — open the
/// Stripe Billing Customer Portal (owner/admin).
pub async fn portal(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;
    let (billing, org_cfg) = billing_and_org(&state)?;

    let customer: Option<String> =
        sqlx::query_scalar("SELECT stripe_customer_id FROM organizations WHERE id = $1")
            .bind(org_id)
            .fetch_one(pool)
            .await
            .map_err(db_err)?;
    let customer =
        customer.ok_or_else(|| (StatusCode::CONFLICT, "no active subscription").into_response())?;

    let url = stripe_handler::create_portal_session(&state.http, billing, org_cfg, &customer)
        .await
        .map_err(|e| {
            tracing::error!("org portal session failed: {e}");
            bad_gateway()
        })?;
    Ok(Json(json!({ "url": url })).into_response())
}

/// `POST /api/business/organizations/{org_id}/credits/purchase` — one-off top-up
/// via Stripe Checkout (owner/admin).
pub async fn purchase(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Json(body): Json<PurchaseReq>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;
    let (billing, org_cfg) = billing_and_org(&state)?;
    if body.credits_amount <= 0 || body.credits_amount > 1_000_000 {
        return Err(bad_request(
            "credits_amount must be between 1 and 1,000,000",
        ));
    }

    let url = stripe_handler::create_org_purchase_checkout(
        &state.http,
        billing,
        org_cfg,
        &org_id,
        body.credits_amount,
    )
    .await
    .map_err(|e| {
        tracing::error!("org purchase checkout failed: {e}");
        bad_gateway()
    })?;
    Ok(Json(json!({ "url": url })).into_response())
}

/// `GET /api/business/organizations/{org_id}/credits` — balance + last 50 ledger
/// rows (owner/admin).
pub async fn credits(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;

    let balance: i32 =
        sqlx::query_scalar("SELECT credits_balance FROM organizations WHERE id = $1")
            .bind(org_id)
            .fetch_one(pool)
            .await
            .map_err(db_err)?;
    let txns: Vec<LedgerRow> = sqlx::query_as(
        "SELECT amount, type, description, created_at
         FROM organization_credits_transactions
         WHERE org_id = $1 ORDER BY created_at DESC LIMIT 50",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;

    Ok(Json(json!({ "balance": balance, "transactions": txns })).into_response())
}

/// `POST /api/business/stripe/webhook` — Stripe events for org billing. Verified
/// with the org webhook secret; grants are idempotent via `stripe_events`.
pub async fn webhook(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(org_cfg) = state
        .config
        .billing
        .as_ref()
        .and_then(|b| b.org_billing.as_ref())
    else {
        return svc_unavailable("org billing not configured");
    };
    let Some(pool) = state.pool.as_ref() else {
        return svc_unavailable("database not configured");
    };

    let sig = headers
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !stripe_handler::verify_stripe_signature(&org_cfg.webhook_secret, &body, sig) {
        return (StatusCode::BAD_REQUEST, "invalid signature").into_response();
    }
    let Ok(event) = serde_json::from_slice::<Value>(&body) else {
        return (StatusCode::BAD_REQUEST, "invalid json").into_response();
    };
    let event_id = event["id"].as_str().unwrap_or("");
    let event_type = event["type"].as_str().unwrap_or("");
    if event_id.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing event id").into_response();
    }

    match handle_event(
        pool,
        org_cfg,
        event_id,
        event_type,
        &event["data"]["object"],
    )
    .await
    {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => {
            // 500 so Stripe retries a transient DB failure.
            tracing::error!("org webhook {event_type} failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "webhook error").into_response()
        }
    }
}

async fn handle_event(
    pool: &Pool,
    org_cfg: &OrgBillingConfig,
    event_id: &str,
    event_type: &str,
    obj: &Value,
) -> Result<(), sqlx::Error> {
    match event_type {
        "checkout.session.completed" => match obj["mode"].as_str() {
            Some("subscription") => {
                if let Some(org_id) = obj_org_id(obj) {
                    sqlx::query(
                        "UPDATE organizations SET
                            stripe_customer_id = $2,
                            stripe_subscription_id = $3,
                            plan = COALESCE($4, plan),
                            subscription_interval = $5,
                            subscription_status = 'active',
                            updated_at = now()
                         WHERE id = $1",
                    )
                    .bind(org_id)
                    .bind(obj["customer"].as_str())
                    .bind(obj["subscription"].as_str())
                    .bind(obj["metadata"]["plan"].as_str())
                    .bind(obj["metadata"]["interval"].as_str())
                    .execute(pool)
                    .await?;
                }
            }
            Some("payment") => {
                let credits = obj["metadata"]["credits"]
                    .as_str()
                    .and_then(|s| s.parse::<i32>().ok())
                    .unwrap_or(0);
                if let (Some(org_id), true) = (obj_org_id(obj), credits > 0) {
                    grant(
                        pool, event_id, event_type, org_id, credits, "purchase", None, None,
                    )
                    .await?;
                }
            }
            _ => {}
        },
        "invoice.payment_succeeded" => {
            if let Some(customer) = obj["customer"].as_str() {
                let org: Option<(Uuid, String)> = sqlx::query_as(
                    "SELECT id, plan FROM organizations WHERE stripe_customer_id = $1",
                )
                .bind(customer)
                .fetch_optional(pool)
                .await?;
                if let Some((org_id, org_plan)) = org {
                    let price_id = obj
                        .pointer("/lines/data/0/price/id")
                        .and_then(|v| v.as_str());
                    let (plan, interval) = price_id
                        .and_then(|p| org_cfg.plan_interval_for_price(p))
                        .unwrap_or((leak_plan(&org_plan), "month"));
                    let credits = org_cfg.grant_credits(plan, interval);
                    let period_end = obj
                        .pointer("/lines/data/0/period/end")
                        .or_else(|| obj.get("period_end"))
                        .and_then(|v| v.as_i64());
                    grant(
                        pool,
                        event_id,
                        event_type,
                        org_id,
                        credits,
                        "subscription_grant",
                        period_end,
                        Some((plan, interval)),
                    )
                    .await?;
                }
            }
        }
        "customer.subscription.updated" => {
            if let Some(sub_id) = obj["id"].as_str() {
                let status = map_status(obj["status"].as_str());
                let period_end = obj["current_period_end"]
                    .as_i64()
                    .and_then(|s| DateTime::from_timestamp(s, 0));
                sqlx::query(
                    "UPDATE organizations SET
                        subscription_status = $2,
                        cancel_at_period_end = COALESCE($3, cancel_at_period_end),
                        current_period_end = COALESCE($4, current_period_end),
                        updated_at = now()
                     WHERE stripe_subscription_id = $1",
                )
                .bind(sub_id)
                .bind(status)
                .bind(obj["cancel_at_period_end"].as_bool())
                .bind(period_end)
                .execute(pool)
                .await?;
            }
        }
        "customer.subscription.deleted" => {
            if let Some(sub_id) = obj["id"].as_str() {
                sqlx::query(
                    "UPDATE organizations SET subscription_status = 'canceled', updated_at = now()
                     WHERE stripe_subscription_id = $1",
                )
                .bind(sub_id)
                .execute(pool)
                .await?;
            }
        }
        "invoice.payment_failed" => {
            if let Some(customer) = obj["customer"].as_str() {
                sqlx::query(
                    "UPDATE organizations SET subscription_status = 'past_due', updated_at = now()
                     WHERE stripe_customer_id = $1",
                )
                .bind(customer)
                .execute(pool)
                .await?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Atomic, idempotent credit grant: dedupe the event via `stripe_events`, then
/// (only on first sight) add credits + write the ledger row and optionally bump
/// the subscription status/period/plan — all in one transaction.
#[allow(clippy::too_many_arguments)]
async fn grant(
    pool: &Pool,
    event_id: &str,
    event_type: &str,
    org_id: Uuid,
    credits: i32,
    kind: &str,
    period_end_unix: Option<i64>,
    plan_interval: Option<(&str, &str)>,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO stripe_events (id, type) VALUES ($1, $2) ON CONFLICT (id) DO NOTHING",
    )
    .bind(event_id)
    .bind(event_type)
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
        tx.rollback().await?; // already processed
        return Ok(());
    }
    if credits > 0 {
        sqlx::query(
            "UPDATE organizations SET credits_balance = credits_balance + $2, updated_at = now()
             WHERE id = $1",
        )
        .bind(org_id)
        .bind(credits)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO organization_credits_transactions (org_id, amount, type, description)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(org_id)
        .bind(credits)
        .bind(kind)
        .bind("stripe grant")
        .execute(&mut *tx)
        .await?;
    }
    if let Some((plan, interval)) = plan_interval {
        let period_end = period_end_unix.and_then(|s| DateTime::from_timestamp(s, 0));
        sqlx::query(
            "UPDATE organizations SET
                subscription_status = 'active',
                current_period_end = COALESCE($2, current_period_end),
                plan = $3,
                subscription_interval = $4,
                updated_at = now()
             WHERE id = $1",
        )
        .bind(org_id)
        .bind(period_end)
        .bind(plan)
        .bind(interval)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// `org_id` from a Checkout Session object: metadata first, then client_reference_id.
fn obj_org_id(obj: &Value) -> Option<Uuid> {
    obj["metadata"]["org_id"]
        .as_str()
        .or_else(|| obj["client_reference_id"].as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
}

/// Map a Stripe subscription status to our coarse status.
fn map_status(status: Option<&str>) -> &'static str {
    match status {
        Some("active") | Some("trialing") => "active",
        Some("past_due") | Some("unpaid") => "past_due",
        Some("canceled") | Some("incomplete_expired") => "canceled",
        _ => "active",
    }
}

/// Coerce a stored plan string to a static fallback for credit lookup.
fn leak_plan(plan: &str) -> &'static str {
    if plan == "enterprise" {
        "enterprise"
    } else {
        "business"
    }
}
