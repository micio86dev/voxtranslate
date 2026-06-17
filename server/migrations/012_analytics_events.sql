-- 012 — Analytics event store + aggregates (spec 0097 / issue #241)
--
-- Two-layer, event-sourced analytics for Standard vs Premium usage.
--
-- RECONCILED WITH THE EXISTING SCHEMA (the issue said "do NOT break existing
-- schema, prefer extending"): the issue's proposed `sessions` table duplicates the
-- existing `usage_sessions` (per-user, per-call, cost, engine/plan) and
-- `call_sessions`. We therefore do NOT add a `sessions` table — `session_id` here
-- references the existing `call_sessions(id)` (the room-lifetime call id). `plan`
-- is the engine TIER ('standard' | 'premium'), already chosen per connection
-- (spec 0093/0094).
--
-- Layer 1 — append-only raw events (source of truth). NEVER updated.
-- Layer 2 — precomputed aggregates for fast Directus dashboards (Directus must
--           never scan the raw events).

-- ---- Layer 1: raw event store ----------------------------------------------
CREATE TABLE IF NOT EXISTS session_usage_events (
    id          BIGSERIAL PRIMARY KEY,
    session_id  UUID REFERENCES call_sessions(id) ON DELETE CASCADE,
    user_id     UUID REFERENCES users(id) ON DELETE CASCADE, -- NULL for guests
    plan        TEXT NOT NULL CHECK (plan IN ('standard', 'premium')),
    event_type  TEXT NOT NULL, -- translation_started | subtitles_on | voice_generated | screen_shared | recording_started | ...
    feature     TEXT,          -- translation | subtitles | voice | screen_share | recording
    mode        TEXT,          -- live | realtime | async
    duration_seconds INTEGER NOT NULL DEFAULT 0,
    cost_cents       INTEGER NOT NULL DEFAULT 0,
    language_from    TEXT,
    language_to      TEXT,
    metadata    JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_evt_session ON session_usage_events (session_id);
CREATE INDEX IF NOT EXISTS idx_evt_user    ON session_usage_events (user_id);
CREATE INDEX IF NOT EXISTS idx_evt_plan    ON session_usage_events (plan);
CREATE INDEX IF NOT EXISTS idx_evt_type    ON session_usage_events (event_type);
CREATE INDEX IF NOT EXISTS idx_evt_feature ON session_usage_events (feature);
CREATE INDEX IF NOT EXISTS idx_evt_time    ON session_usage_events (created_at);

-- ---- Layer 2: read-optimized aggregates ------------------------------------
CREATE TABLE IF NOT EXISTS plan_usage_daily (
    id                   BIGSERIAL PRIMARY KEY,
    day                  DATE NOT NULL,
    plan                 TEXT NOT NULL CHECK (plan IN ('standard', 'premium')),
    total_sessions       INTEGER NOT NULL DEFAULT 0,
    total_users          INTEGER NOT NULL DEFAULT 0,
    total_minutes        INTEGER NOT NULL DEFAULT 0,
    total_cost_cents     INTEGER NOT NULL DEFAULT 0,
    avg_session_duration INTEGER NOT NULL DEFAULT 0,
    UNIQUE (day, plan) -- one row per (day, plan); the aggregator upserts
);
CREATE INDEX IF NOT EXISTS idx_plan_day  ON plan_usage_daily (day);
CREATE INDEX IF NOT EXISTS idx_plan_plan ON plan_usage_daily (plan);

CREATE TABLE IF NOT EXISTS user_usage_stats (
    user_id          UUID PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    total_sessions   INTEGER NOT NULL DEFAULT 0,
    total_minutes    INTEGER NOT NULL DEFAULT 0,
    standard_minutes INTEGER NOT NULL DEFAULT 0,
    premium_minutes  INTEGER NOT NULL DEFAULT 0,
    last_active_at   TIMESTAMPTZ,
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- RLS lockdown for the new tables, consistent with spec 0096/#239: the Rust API
-- (owner role) is exempt; the Supabase anon/authenticated roles get default-deny.
ALTER TABLE session_usage_events ENABLE ROW LEVEL SECURITY;
ALTER TABLE plan_usage_daily     ENABLE ROW LEVEL SECURITY;
ALTER TABLE user_usage_stats     ENABLE ROW LEVEL SECURITY;
