//! What the telephone did and what it cost (spec 0117).
//!
//! `business/analytics.rs` answers this for meetings and knows nothing about telephony,
//! because when it was written there was none. The gap matters more here: a meeting costs
//! translation minutes, while a telephone call costs translation minutes **plus a
//! carrier**, and the carrier's share varies by where you called — which is the first
//! number a finance person asks for.
//!
//! Everything is aggregated in SQL. Postgres is where the rows are, `voip_calls` already
//! carries every dimension this needs, and folding a window of calls in the process would
//! turn a cheap query into a memory profile that grows with the customer.

#![allow(clippy::result_large_err)]

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::FromRow;
use uuid::Uuid;

use crate::business::{db_err, require_pool, require_role, ADMIN};
use crate::middleware::AuthUser;
use crate::AppState;

#[derive(Deserialize, Default)]
pub struct Window {
    /// Look-back in days (default 30, clamped 1..=365) — the same shape the meeting
    /// analytics uses, so the two screens cannot disagree about what "this month" means.
    days: Option<i64>,
}

#[derive(FromRow, Serialize)]
struct Totals {
    calls: i64,
    inbound: i64,
    outbound: i64,
    answered: i64,
    missed: i64,
    /// Seconds, summed. Rendered as minutes by the client, which is where the reader's
    /// units belong.
    seconds: i64,
    credits: i64,
}

#[derive(FromRow, Serialize)]
struct Bucket {
    label: String,
    calls: i64,
    credits: i64,
}

#[derive(FromRow, Serialize)]
struct DayPoint {
    day: String,
    calls: i64,
}

/// `GET …/voip/analytics?days=30` (admin+).
///
/// ADMIN, matching the meeting analytics and the credits endpoint, and for the same
/// reason: spend is financial data.
pub async fn summary(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<Uuid>,
    Query(q): Query<Window>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;
    let days = q.days.unwrap_or(30).clamp(1, 365);

    // `missed` is counted from the column rather than inferred from a zero duration: a
    // call that connected and lasted no time is not the same event as one nobody answered
    // (spec 0116).
    let totals: Totals = sqlx::query_as(
        "SELECT count(*)::bigint AS calls,
                count(*) FILTER (WHERE direction = 'inbound')::bigint AS inbound,
                count(*) FILTER (WHERE direction = 'outbound')::bigint AS outbound,
                count(*) FILTER (WHERE NOT missed AND duration_seconds > 0)::bigint AS answered,
                count(*) FILTER (WHERE missed)::bigint AS missed,
                COALESCE(sum(duration_seconds), 0)::bigint AS seconds,
                COALESCE(sum(credits_consumed), 0)::bigint AS credits
           FROM voip_calls
          WHERE org_id = $1 AND started_at >= now() - make_interval(days => $2::int)",
    )
    .bind(org_id)
    .bind(days as i32)
    .fetch_one(pool)
    .await
    .map_err(db_err)?;

    let bucket = |sql: &'static str| async move {
        sqlx::query_as::<_, Bucket>(sql)
            .bind(org_id)
            .bind(days as i32)
            .fetch_all(pool)
            .await
    };

    let by_country = bucket(
        "SELECT COALESCE(NULLIF(recipient_country, ''), '??') AS label,
                count(*)::bigint AS calls,
                COALESCE(sum(credits_consumed), 0)::bigint AS credits
           FROM voip_calls
          WHERE org_id = $1 AND started_at >= now() - make_interval(days => $2::int)
          GROUP BY 1 ORDER BY calls DESC LIMIT 10",
    )
    .await
    .map_err(db_err)?;

    let by_language = bucket(
        "SELECT source_language || ' → ' || target_language AS label,
                count(*)::bigint AS calls,
                COALESCE(sum(credits_consumed), 0)::bigint AS credits
           FROM voip_calls
          WHERE org_id = $1 AND started_at >= now() - make_interval(days => $2::int)
          GROUP BY 1 ORDER BY calls DESC LIMIT 10",
    )
    .await
    .map_err(db_err)?;

    let by_tier = bucket(
        "SELECT engine_id AS label,
                count(*)::bigint AS calls,
                COALESCE(sum(credits_consumed), 0)::bigint AS credits
           FROM voip_calls
          WHERE org_id = $1 AND started_at >= now() - make_interval(days => $2::int)
          GROUP BY 1 ORDER BY calls DESC",
    )
    .await
    .map_err(db_err)?;

    let by_project = bucket(
        "SELECT COALESCE(p.name, '—') AS label,
                count(*)::bigint AS calls,
                COALESCE(sum(c.credits_consumed), 0)::bigint AS credits
           FROM voip_calls c
           LEFT JOIN projects p ON p.id = c.project_id
          WHERE c.org_id = $1 AND c.started_at >= now() - make_interval(days => $2::int)
          GROUP BY 1 ORDER BY calls DESC LIMIT 10",
    )
    .await
    .map_err(db_err)?;

    let by_day: Vec<DayPoint> = sqlx::query_as(
        "SELECT to_char(date_trunc('day', started_at), 'YYYY-MM-DD') AS day,
                count(*)::bigint AS calls
           FROM voip_calls
          WHERE org_id = $1 AND started_at >= now() - make_interval(days => $2::int)
          GROUP BY 1 ORDER BY 1",
    )
    .bind(org_id)
    .bind(days as i32)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;

    // The ledger, not the calls. `credits_consumed` on a call is what the CALL metered;
    // this is what the organisation actually paid, and it includes the number purchases
    // and renewals a call row knows nothing about. Two different questions, answered
    // separately rather than added into one number that means neither.
    let telephony_spend: i64 = sqlx::query_scalar(
        "SELECT COALESCE(-sum(amount), 0)::bigint
           FROM organization_credits_transactions
          WHERE org_id = $1
            AND created_at >= now() - make_interval(days => $2::int)
            AND type IN ('voip_hold', 'voip_overage', 'voip_number_purchase',
                         'voip_number_renewal')",
    )
    .bind(org_id)
    .bind(days as i32)
    .fetch_one(pool)
    .await
    .map_err(db_err)?;

    // No provider cost and no margin anywhere in here — spec 0112 R6, which applies to
    // every surface and not only to the call record.
    Ok(Json(json!({
        "days": days,
        "totals": totals,
        "by_country": by_country,
        "by_language": by_language,
        "by_tier": by_tier,
        "by_project": by_project,
        "calls_by_day": by_day,
        "telephony_credits_spent": telephony_spend,
    }))
    .into_response())
}
