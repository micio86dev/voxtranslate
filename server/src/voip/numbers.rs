//! Buying, verifying and keeping telephone numbers (spec 0115).
//!
//! The first thing in this product that costs money **before** a call: a one-off purchase,
//! then a monthly charge that recurs whether or not anybody dials. That is a different
//! billing shape from a metered call, and it is where the brief's *provider cost + 20%
//! markup* applies — via [`NumberMarkupPolicy`], which lives in `pricing.rs` and is the
//! only place that multiplication happens.

// Every handler returns `Result<Response, Response>` — the Business API convention.
#![allow(clippy::result_large_err)]

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::business::credits::{deduct_org_credits_tx, OrgCharge};
use crate::business::{db_err, not_found, require_pool, require_role, ADMIN, MEMBER};
use crate::middleware::AuthUser;
use crate::telephony::{
    NumberKind, NumberOffer, NumberSearch, NumberStatus, ProviderError, ProviderNumberId,
    PurchaseRequest, E164,
};
use crate::voip::pricing::{credits_ceil, NumberMarkupPolicy};
use crate::voip::routes::refuse;
use crate::AppState;

/// Ledger kinds. Distinct from the call kinds so a monthly number charge is never mistaken
/// for a call somebody made.
pub const KIND_PURCHASE: &str = "voip_number_purchase";
pub const KIND_RENEWAL: &str = "voip_number_renewal";

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    country: String,
    #[serde(default)]
    area_code: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    limit: Option<u8>,
}

#[derive(Debug, Deserialize)]
pub struct BuyBody {
    e164: String,
    country: String,
    #[serde(default)]
    area_code: Option<String>,
    /// The caller's key. The same key must buy the same number once, however many times a
    /// flaky network makes the client retry.
    purchase_key: String,
}

/// The markup policy from configuration, built once per request.
fn markup(state: &AppState) -> Result<NumberMarkupPolicy, Response> {
    let cfg = state
        .config
        .voip
        .as_ref()
        .ok_or_else(|| not_found("voip is not enabled"))?;
    let rate = Decimal::try_from(cfg.number_markup)
        .map_err(|_| refuse(StatusCode::SERVICE_UNAVAILABLE, "voip_misconfigured"))?;
    NumberMarkupPolicy::new(rate)
        .map_err(|_| refuse(StatusCode::SERVICE_UNAVAILABLE, "voip_misconfigured"))
}

fn provider(state: &AppState) -> Result<&dyn crate::telephony::TelephonyProvider, Response> {
    state
        .telephony
        .as_deref()
        .ok_or_else(|| not_found("voip is not enabled"))
}

/// Map a provider failure onto something a customer can act on.
///
/// `Unsupported` is its own code rather than a 500: an account that cannot search or buy
/// numbers is a configuration fact, and telling the user "try again" would be wrong.
fn provider_err(e: ProviderError) -> Response {
    match e {
        ProviderError::Unsupported { .. } => {
            refuse(StatusCode::NOT_IMPLEMENTED, "numbers_unsupported")
        }
        ProviderError::Unauthorized | ProviderError::AccountBlocked => {
            refuse(StatusCode::SERVICE_UNAVAILABLE, "voip_misconfigured")
        }
        _ => refuse(StatusCode::BAD_GATEWAY, "provider_unavailable"),
    }
}

/// `GET …/voip/numbers/search` — what is available, at what the CUSTOMER would pay.
pub async fn search(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Query(q): Query<SearchQuery>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    // Looking is a member action; spending the organisation's money is not (R7).
    require_role(pool, org_id, user.user_id, MEMBER).await?;
    let policy = markup(&state)?;

    let offers = provider(&state)?
        .search_numbers(NumberSearch {
            country: q.country.clone(),
            area_code: q.area_code.clone(),
            kind: q.kind.as_deref().map(NumberKind::parse),
            limit: q.limit.unwrap_or(10),
        })
        .await
        .map_err(provider_err)?;

    // Only customer prices cross the boundary. What the carrier charges us is ours, the
    // same rule `CallDetailRow` follows for a call's provider cost.
    let priced: Vec<_> = offers
        .iter()
        .map(|o| {
            Ok(json!({
                "e164": o.e164,
                "country": o.country,
                "kind": o.kind.as_str(),
                "monthly": policy.customer_price(o.monthly_cost)
                    .map_err(|_| refuse(StatusCode::SERVICE_UNAVAILABLE, "voip_misconfigured"))?
                    .to_string(),
                "setup": policy.customer_price(o.setup_cost)
                    .map_err(|_| refuse(StatusCode::SERVICE_UNAVAILABLE, "voip_misconfigured"))?
                    .to_string(),
                "currency": o.currency,
                "regulatory_requirement": o.regulatory_requirement,
            }))
        })
        .collect::<Result<_, Response>>()?;

    Ok(Json(json!({ "offers": priced })).into_response())
}

/// Find the offer for one specific number, so the price comes from the carrier rather than
/// from the client.
///
/// The purchase response does not carry a price, so this re-runs the search the user just
/// ran and matches the number. A number no longer on offer is refused rather than bought at
/// a guessed price — the same fail-closed rule `RateDeck::lookup` applies to a call.
async fn offer_for(state: &AppState, body: &BuyBody) -> Result<NumberOffer, Response> {
    let offers = provider(state)?
        .search_numbers(NumberSearch {
            country: body.country.clone(),
            area_code: body.area_code.clone(),
            kind: None,
            limit: 10,
        })
        .await
        .map_err(provider_err)?;
    offers
        .into_iter()
        .find(|o| o.e164 == body.e164)
        .ok_or_else(|| refuse(StatusCode::CONFLICT, "number_unavailable"))
}

/// `POST …/voip/numbers` — buy one.
pub async fn buy(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Json(body): Json<BuyBody>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;
    let policy = markup(&state)?;

    let dest =
        E164::parse(&body.e164).map_err(|_| refuse(StatusCode::BAD_REQUEST, "number_not_e164"))?;

    // Already bought under this key? Answer with the number rather than buying another.
    // Checked before the carrier is touched, so a retry costs nothing at all.
    let existing: Option<(Uuid, String, String)> = sqlx::query_as(
        "SELECT id, e164, status FROM voip_numbers WHERE org_id = $1 AND purchase_key = $2",
    )
    .bind(org_id)
    .bind(&body.purchase_key)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    if let Some((id, e164, status)) = existing {
        return Ok(Json(json!({ "id": id, "e164": e164, "status": status })).into_response());
    }

    let offer = offer_for(&state, &body).await?;
    let customer_monthly = policy
        .customer_price(offer.monthly_cost)
        .map_err(|_| refuse(StatusCode::SERVICE_UNAVAILABLE, "voip_misconfigured"))?;
    let customer_setup = policy
        .customer_price(offer.setup_cost)
        .map_err(|_| refuse(StatusCode::SERVICE_UNAVAILABLE, "voip_misconfigured"))?;

    // The carrier FIRST, the charge second. A number that was never bought must never be
    // charged for, and the reverse ordering — charge, then discover the carrier refused —
    // leaves a customer paying for nothing and us writing an apology.
    let bought = provider(&state)?
        .purchase_number(PurchaseRequest {
            e164: dest.as_str().to_string(),
            idempotency_key: body.purchase_key.clone(),
        })
        .await
        .map_err(provider_err)?;

    let mut tx = pool.begin().await.map_err(db_err)?;
    // The setup fee, charged once. The monthly one starts at the first renewal, so a
    // customer is never billed twice for the month they bought in.
    let charge = credits_ceil(customer_setup);
    if charge > 0 {
        match deduct_org_credits_tx(
            &mut tx,
            org_id,
            charge,
            KIND_PURCHASE,
            None,
            Some(user.user_id),
            &format!("Telephone number {}", dest.masked()),
        )
        .await
        .map_err(db_err)?
        {
            OrgCharge::Charged { .. } => {}
            OrgCharge::Insufficient { .. } => {
                // The number is already bought at this point. Releasing it immediately is
                // the honest repair: we do not keep something the customer cannot pay for,
                // and we do not bill them for it either.
                let _ = provider(&state)?
                    .release_number(&bought.provider_number_id)
                    .await;
                return Err(refuse(StatusCode::PAYMENT_REQUIRED, "insufficient_credits"));
            }
        }
    }

    let status = if bought.regulatory_requirement.is_some() {
        NumberStatus::PendingRegulatory
    } else {
        bought.status
    };

    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO voip_numbers
            (org_id, provider, provider_number_id, e164, country, outbound_enabled,
             verification_status, status, status_reason, regulatory_requirement,
             provider_monthly_usd, provider_setup_usd, markup_rate,
             customer_monthly_usd, customer_setup_usd, currency,
             purchase_key, next_renewal_at)
         VALUES ($1, $2, $3, $4, $5, TRUE, 'verified', $6, $7, $7, $8, $9, $10, $11, $12, $13,
                 $14, $15)
         RETURNING id",
    )
    .bind(org_id)
    .bind(provider(&state)?.metadata().id)
    .bind(bought.provider_number_id.as_str())
    .bind(dest.as_str())
    .bind(dest.region())
    .bind(status.as_str())
    .bind(bought.regulatory_requirement.as_deref())
    .bind(offer.monthly_cost)
    .bind(offer.setup_cost)
    .bind(policy.markup())
    .bind(customer_monthly)
    .bind(customer_setup)
    .bind(&offer.currency)
    .bind(&body.purchase_key)
    .bind(Utc::now() + Duration::days(30))
    .fetch_one(&mut *tx)
    .await
    .map_err(db_err)?;
    tx.commit().await.map_err(db_err)?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": id,
            "e164": dest.as_str(),
            "status": status.as_str(),
            "monthly": customer_monthly.to_string(),
            "setup": customer_setup.to_string(),
            "regulatory_requirement": bought.regulatory_requirement,
        })),
    )
        .into_response())
}

/// `POST …/voip/numbers/{id}/verify` — prove the organisation owns a number it holds
/// elsewhere, so it may be presented as caller id.
pub async fn verify(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, number_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;

    let e164: Option<String> =
        sqlx::query_scalar("SELECT e164 FROM voip_numbers WHERE id = $1 AND org_id = $2")
            .bind(number_id)
            .bind(org_id)
            .fetch_optional(pool)
            .await
            .map_err(db_err)?;
    let Some(e164) = e164 else {
        return Err(not_found("number not found"));
    };
    let parsed =
        E164::parse(&e164).map_err(|_| refuse(StatusCode::BAD_REQUEST, "number_not_e164"))?;

    let started = provider(&state)?
        .start_caller_id_verification(&parsed)
        .await
        .map_err(provider_err)?;

    // Recorded as PENDING, never as verified. The status that lets a number be presented
    // is only ever written when the provider says the check passed.
    sqlx::query(
        "UPDATE voip_numbers SET verification_id = $3, verification_status = 'pending',
                                 updated_at = now()
          WHERE id = $1 AND org_id = $2",
    )
    .bind(number_id)
    .bind(org_id)
    .bind(&started.id)
    .execute(pool)
    .await
    .map_err(db_err)?;

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "verification_status": "pending",
            "method": match started.method {
                crate::telephony::VerificationMethod::Call => "call",
                crate::telephony::VerificationMethod::Sms => "sms",
            },
            "code": started.code,
        })),
    )
        .into_response())
}

/// `POST …/voip/numbers/{id}/verify/check` — has it passed yet?
pub async fn verify_check(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, number_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;

    let row: Option<Option<String>> = sqlx::query_scalar(
        "SELECT verification_id FROM voip_numbers WHERE id = $1 AND org_id = $2",
    )
    .bind(number_id)
    .bind(org_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    let Some(Some(verification_id)) = row else {
        return Err(not_found("no verification is in flight for that number"));
    };

    let state_now = provider(&state)?
        .check_caller_id_verification(&verification_id)
        .await
        .map_err(provider_err)?;

    sqlx::query(
        "UPDATE voip_numbers SET verification_status = $3, updated_at = now()
          WHERE id = $1 AND org_id = $2",
    )
    .bind(number_id)
    .bind(org_id)
    .bind(state_now.as_str())
    .execute(pool)
    .await
    .map_err(db_err)?;

    Ok(Json(json!({ "verification_status": state_now.as_str() })).into_response())
}

/// `DELETE …/voip/numbers/{id}` — give a number back.
///
/// Irreversible from the customer's point of view: somebody else may hold that number an
/// hour later. The product asks twice before it gets here.
pub async fn release(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, number_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;

    let row: Option<Option<String>> = sqlx::query_scalar(
        "SELECT provider_number_id FROM voip_numbers WHERE id = $1 AND org_id = $2",
    )
    .bind(number_id)
    .bind(org_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    let Some(provider_number_id) = row else {
        return Err(not_found("number not found"));
    };

    if let Some(id) = provider_number_id {
        provider(&state)?
            .release_number(&ProviderNumberId(id))
            .await
            .map_err(provider_err)?;
    }

    // Kept as a row in `released`, not deleted: calls made from this number still refer to
    // it, and an organisation asking "what happened to our Milan line" deserves an answer.
    sqlx::query(
        "UPDATE voip_numbers
            SET status = 'released', outbound_enabled = FALSE, inbound_enabled = FALSE,
                is_default = FALSE, next_renewal_at = NULL, updated_at = now()
          WHERE id = $1 AND org_id = $2",
    )
    .bind(number_id)
    .bind(org_id)
    .execute(pool)
    .await
    .map_err(db_err)?;

    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Charge the numbers whose month is up (spec 0115 R6).
///
/// The only irreversible act in a number's life is release, and this sweep may not perform
/// it. A renewal the wallet cannot cover **suspends** — the number stays ours, the
/// organisation is told, and a human decides before anything is lost. A business losing its
/// telephone number over a card that expired is a failure nobody recovers from by writing
/// an apology; the grace period costs us the carrier's monthly fee, which is a bounded,
/// deliberate loss.
///
/// Returns how many numbers were renewed, for the caller to log.
pub async fn renew_due(
    pool: &crate::db::Pool,
    grace_days: i64,
    batch: i64,
) -> Result<(u64, u64), sqlx::Error> {
    let due: Vec<(Uuid, Uuid, String, Option<Decimal>)> = sqlx::query_as(
        "SELECT id, org_id, e164, customer_monthly_usd
           FROM voip_numbers
          WHERE status IN ('active', 'suspended')
            AND next_renewal_at IS NOT NULL
            AND next_renewal_at <= now()
          ORDER BY next_renewal_at
          LIMIT $1",
    )
    .bind(batch)
    .fetch_all(pool)
    .await?;

    let mut renewed = 0u64;
    let mut suspended = 0u64;

    for (id, org_id, e164, monthly) in due {
        // The price recorded on the row, not today's. An eighteen-month-old number renews
        // at what the customer agreed to, which is the whole point of the snapshot.
        let charge = monthly.map(credits_ceil).unwrap_or(0);
        let mut tx = pool.begin().await?;

        let outcome = if charge > 0 {
            deduct_org_credits_tx(
                &mut tx,
                org_id,
                charge,
                KIND_RENEWAL,
                None,
                None,
                &format!("Monthly charge for telephone number {e164}"),
            )
            .await?
        } else {
            OrgCharge::Charged { balance_after: 0 }
        };

        match outcome {
            OrgCharge::Charged { .. } => {
                sqlx::query(
                    "UPDATE voip_numbers
                        SET next_renewal_at = next_renewal_at + interval '30 days',
                            status = 'active', suspended_at = NULL, status_reason = NULL,
                            outbound_enabled = TRUE, updated_at = now()
                      WHERE id = $1",
                )
                .bind(id)
                .execute(&mut *tx)
                .await?;
                renewed += 1;
            }
            OrgCharge::Insufficient { .. } => {
                // Suspended, never released. `outbound_enabled = FALSE` stops the number
                // being presented while it is unpaid; `resolve_caller_id` already refuses
                // anything that is not outbound-enabled, so nothing else has to remember.
                sqlx::query(
                    "UPDATE voip_numbers
                        SET status = 'suspended',
                            suspended_at = COALESCE(suspended_at, now()),
                            status_reason = 'insufficient_credits',
                            outbound_enabled = FALSE,
                            next_renewal_at = next_renewal_at + interval '1 day',
                            updated_at = now()
                      WHERE id = $1",
                )
                .bind(id)
                .execute(&mut *tx)
                .await?;
                suspended += 1;
                tracing::warn!(
                    number = %id,
                    org = %org_id,
                    grace_days,
                    "voip number suspended: the organisation could not cover its renewal"
                );
            }
        }
        tx.commit().await?;
    }

    Ok((renewed, suspended))
}
