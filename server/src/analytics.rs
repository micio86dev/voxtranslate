//! Append-only usage analytics (spec 0097 / issue #241).
//!
//! Layer 1 of the two-layer model: raw events are the source of truth and are
//! NEVER updated. Ingestion is **fire-and-forget** — it must never block or fail
//! the call path, so [`record_event`] spawns the insert and a DB error is logged
//! and dropped, never propagated. The read-optimized aggregates
//! (`plan_usage_daily`, `user_usage_stats`) are filled by a separate roll-up job
//! so Directus dashboards never scan the raw event table.

use uuid::Uuid;

use crate::db::Pool;

/// The `plan` analytics dimension from an engine tier. Both PAID tiers (`"pro"` and
/// `"premium"`) map to `"premium"`; everything else (the default Standard engine) maps to
/// `"standard"` — matching the `plan IN ('standard','premium')` CHECK on
/// `session_usage_events`.
pub fn plan_for_tier(tier: &str) -> &'static str {
    match tier {
        "premium" | "pro" => "premium",
        _ => "standard",
    }
}

/// One row to append to `session_usage_events`. Owned so it can move into the
/// fire-and-forget task. Build with [`UsageEvent::new`] then set the optional
/// fields you have.
#[derive(Clone, Debug)]
pub struct UsageEvent {
    pub session_id: Option<Uuid>,
    pub user_id: Option<Uuid>,
    pub plan: String,
    pub event_type: String,
    pub feature: Option<String>,
    pub mode: Option<String>,
    pub duration_seconds: i32,
    pub cost_cents: i32,
    pub language_from: Option<String>,
    pub language_to: Option<String>,
}

impl UsageEvent {
    /// A minimal event: the required `plan` + `event_type`; all else defaulted.
    pub fn new(plan: impl Into<String>, event_type: impl Into<String>) -> Self {
        Self {
            session_id: None,
            user_id: None,
            plan: plan.into(),
            event_type: event_type.into(),
            feature: None,
            mode: None,
            duration_seconds: 0,
            cost_cents: 0,
            language_from: None,
            language_to: None,
        }
    }

    pub fn session(mut self, id: Uuid) -> Self {
        self.session_id = Some(id);
        self
    }
    pub fn user(mut self, id: Option<Uuid>) -> Self {
        self.user_id = id;
        self
    }
    pub fn feature(mut self, feature: impl Into<String>) -> Self {
        self.feature = Some(feature.into());
        self
    }
}

/// Insert one event (awaitable; used by [`record_event`] and tests).
pub async fn insert_event(pool: &Pool, ev: &UsageEvent) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO session_usage_events
            (session_id, user_id, plan, event_type, feature, mode,
             duration_seconds, cost_cents, language_from, language_to)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(ev.session_id)
    .bind(ev.user_id)
    .bind(&ev.plan)
    .bind(&ev.event_type)
    .bind(&ev.feature)
    .bind(&ev.mode)
    .bind(ev.duration_seconds)
    .bind(ev.cost_cents)
    .bind(&ev.language_from)
    .bind(&ev.language_to)
    .execute(pool)
    .await?;
    Ok(())
}

/// Fire-and-forget append. Spawns the insert and never propagates errors — the
/// call path must be unaffected whether analytics succeeds, fails, or is slow.
pub fn record_event(pool: &Pool, ev: UsageEvent) {
    let pool = pool.clone();
    tokio::spawn(async move {
        if let Err(e) = insert_event(&pool, &ev).await {
            tracing::warn!("analytics event dropped (non-fatal): {e}");
        }
    });
}

/// Recompute `plan_usage_daily` from the raw events (idempotent upsert per day+plan).
const PLAN_DAILY_SQL: &str = "
    INSERT INTO plan_usage_daily
        (day, plan, total_sessions, total_users, total_minutes, total_cost_cents, avg_session_duration)
    SELECT
        date_trunc('day', created_at)::date AS day,
        plan,
        count(DISTINCT session_id) AS total_sessions,
        count(DISTINCT user_id)    AS total_users,
        (sum(duration_seconds) / 60)::int AS total_minutes,
        sum(cost_cents)::int       AS total_cost_cents,
        CASE WHEN count(DISTINCT session_id) > 0
             THEN (sum(duration_seconds) / count(DISTINCT session_id))::int ELSE 0 END
    FROM session_usage_events
    GROUP BY 1, 2
    ON CONFLICT (day, plan) DO UPDATE SET
        total_sessions       = EXCLUDED.total_sessions,
        total_users          = EXCLUDED.total_users,
        total_minutes        = EXCLUDED.total_minutes,
        total_cost_cents     = EXCLUDED.total_cost_cents,
        avg_session_duration = EXCLUDED.avg_session_duration";

/// Recompute `user_usage_stats` from the raw events (idempotent upsert per user).
const USER_STATS_SQL: &str = "
    INSERT INTO user_usage_stats
        (user_id, total_sessions, total_minutes, standard_minutes, premium_minutes, last_active_at, updated_at)
    SELECT
        user_id,
        count(DISTINCT session_id),
        (sum(duration_seconds) / 60)::int,
        (coalesce(sum(duration_seconds) FILTER (WHERE plan = 'standard'), 0) / 60)::int,
        (coalesce(sum(duration_seconds) FILTER (WHERE plan = 'premium'), 0) / 60)::int,
        max(created_at),
        now()
    FROM session_usage_events
    WHERE user_id IS NOT NULL
    GROUP BY user_id
    ON CONFLICT (user_id) DO UPDATE SET
        total_sessions   = EXCLUDED.total_sessions,
        total_minutes    = EXCLUDED.total_minutes,
        standard_minutes = EXCLUDED.standard_minutes,
        premium_minutes  = EXCLUDED.premium_minutes,
        last_active_at   = EXCLUDED.last_active_at,
        updated_at       = now()";

/// Fold raw events into the read-optimized aggregates (full recompute; idempotent).
/// Directus dashboards read ONLY these tables — never the raw event store.
pub async fn roll_up(pool: &Pool) -> Result<(), sqlx::Error> {
    sqlx::query(PLAN_DAILY_SQL).execute(pool).await?;
    sqlx::query(USER_STATS_SQL).execute(pool).await?;
    Ok(())
}

/// Periodic roll-up loop, spawned once at startup. Logs + drops errors so a failed
/// roll-up never takes down the task.
pub async fn run_rollup(pool: Pool, interval: std::time::Duration) {
    let mut tick = tokio::time::interval(interval);
    loop {
        tick.tick().await;
        if let Err(e) = roll_up(&pool).await {
            tracing::warn!("analytics roll-up failed (non-fatal): {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_for_tier_maps_paid_tiers_to_premium_else_standard() {
        assert_eq!(plan_for_tier("premium"), "premium");
        assert_eq!(plan_for_tier("pro"), "premium"); // Pro (OpenAI) is a paid tier too
        assert_eq!(plan_for_tier("standard"), "standard");
        assert_eq!(plan_for_tier("unknown-future-tier"), "standard");
        assert_eq!(plan_for_tier(""), "standard");
    }

    #[test]
    fn usage_event_builder_sets_fields() {
        let id = Uuid::new_v4();
        let ev = UsageEvent::new("premium", "translation_started")
            .session(id)
            .user(None)
            .feature("translation");
        assert_eq!(ev.plan, "premium");
        assert_eq!(ev.event_type, "translation_started");
        assert_eq!(ev.session_id, Some(id));
        assert_eq!(ev.feature.as_deref(), Some("translation"));
        assert_eq!(ev.duration_seconds, 0);
    }
}
