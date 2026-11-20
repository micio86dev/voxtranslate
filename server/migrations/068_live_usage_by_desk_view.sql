-- Live Connect (spec 0121, S14/S15): credits per service desk per day.
--
-- `live_usage_ticks` has a composite primary key (session, peer, tick), which Directus cannot
-- manage as a collection, so the per-desk credit chart operators asked for had nothing to read.
-- Two objects fix that, both metadata only (ids, a UTC day, a count of conversations, a sum of
-- credits: no names, no text, no language, no token material):
--
--   live_desk_usage        a VIEW: the authoritative, always-current aggregate of the
--                          organisation-paid ticks, one row per (desk, day). For SQL and BI.
--   live_desk_usage_daily  a TABLE with a single-column primary key, filled from the view by
--                          `refresh_live_desk_usage()`. Directus 11 only introspects BASE
--                          TABLEs (its Postgres schema inspector filters on
--                          `table_type = 'BASE TABLE'`), so a view can never be a Directus
--                          collection; this table is what it registers and charts.
--
-- Nothing is written on the billing path: the table is refreshed out of band (the application
-- calls `live::dashboard::refresh_desk_usage` from its hourly housekeeping; an operator can run
-- `SELECT refresh_live_desk_usage();` at any time). A desk that was deleted (its conversations
-- keep `desk_id` NULL) and consumer-paid ticks are not part of either. Days are UTC.
--
-- Access: a view runs with its owner's rights, so on Postgres 15+ it is created
-- `security_invoker` (the caller's rights apply, and RLS on the base tables stays default-deny
-- for anon/authenticated) and, where those roles exist (Supabase), every privilege on both
-- objects is revoked from them. Directus and the server connect as the owner and are unaffected.
--
-- Idempotent: CREATE OR REPLACE / IF NOT EXISTS, guarded constraints, ALTER and REVOKE.
CREATE OR REPLACE VIEW live_desk_usage AS
SELECT s.desk_id::text || ':' || to_char((t.ts AT TIME ZONE 'UTC')::date, 'YYYY-MM-DD') AS id,
       s.org_id                                                                       AS org_id,
       s.desk_id                                                                      AS desk_id,
       (t.ts AT TIME ZONE 'UTC')::date                                                AS day,
       count(DISTINCT t.session_id)::bigint                                           AS sessions,
       COALESCE(round(sum(t.amount)), 0)::bigint                                      AS credits
FROM live_usage_ticks t
JOIN live_sessions s ON s.id = t.session_id
WHERE t.payer_kind = 'org'
  AND s.org_id IS NOT NULL
  AND s.desk_id IS NOT NULL
GROUP BY s.desk_id, s.org_id, (t.ts AT TIME ZONE 'UTC')::date;

-- `desk_id` has no foreign key on purpose: the history of a desk that was deleted stays.
CREATE TABLE IF NOT EXISTS live_desk_usage_daily (
    id           TEXT        PRIMARY KEY,                       -- '<desk id>:<YYYY-MM-DD>'
    org_id       UUID        NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    desk_id      UUID        NOT NULL,
    day          DATE        NOT NULL,
    sessions     INTEGER     NOT NULL DEFAULT 0,   -- INTEGER (not BIGINT): Directus serves bigints as strings
    credits      INTEGER     NOT NULL DEFAULT 0,   -- and the org wallet itself is an INTEGER
    refreshed_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_live_desk_usage_daily_org_day ON live_desk_usage_daily (org_id, day DESC);
CREATE INDEX IF NOT EXISTS idx_live_desk_usage_daily_desk_day ON live_desk_usage_daily (desk_id, day DESC);

DO $$
BEGIN
    IF to_regclass('public.live_desk_usage_daily') IS NOT NULL
       AND NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'live_desk_usage_daily_counts_check') THEN
        ALTER TABLE live_desk_usage_daily ADD CONSTRAINT live_desk_usage_daily_counts_check
            CHECK (sessions >= 0 AND credits >= 0) NOT VALID;
    END IF;
END
$$;

-- Bring the rolled-up table in line with the view for the last `p_days` UTC days (default 35;
-- at most 36500): rows are inserted or corrected, and rows the view no longer yields (a
-- conversation whose desk was deleted) are removed. Rows older than the window are never
-- touched, so history survives. Returns how many rows were inserted or changed. Safe to run
-- repeatedly and concurrently: a second run finds nothing to change.
CREATE OR REPLACE FUNCTION refresh_live_desk_usage(p_days integer DEFAULT 35)
RETURNS integer
LANGUAGE plpgsql
AS $fn$
DECLARE
    since date := (now() AT TIME ZONE 'UTC')::date - LEAST(GREATEST(COALESCE(p_days, 35), 1), 36500);
    changed integer;
BEGIN
    WITH up AS (
        INSERT INTO live_desk_usage_daily AS d (id, org_id, desk_id, day, sessions, credits, refreshed_at)
        SELECT v.id, v.org_id, v.desk_id, v.day, v.sessions::integer, v.credits::integer, now()
        FROM live_desk_usage v
        WHERE v.day >= since
        ON CONFLICT (id) DO UPDATE
            SET sessions = EXCLUDED.sessions, credits = EXCLUDED.credits, refreshed_at = EXCLUDED.refreshed_at
            WHERE (d.sessions, d.credits) IS DISTINCT FROM (EXCLUDED.sessions, EXCLUDED.credits)
        RETURNING 1
    )
    SELECT count(*) INTO changed FROM up;
    DELETE FROM live_desk_usage_daily d
    WHERE d.day >= since
      AND NOT EXISTS (SELECT 1 FROM live_desk_usage v WHERE v.id = d.id);
    RETURN changed;
END
$fn$;

ALTER TABLE live_desk_usage_daily ENABLE ROW LEVEL SECURITY;

DO $$
BEGIN
    IF current_setting('server_version_num')::int >= 150000 THEN
        ALTER VIEW live_desk_usage SET (security_invoker = true);
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON live_desk_usage FROM anon;
        REVOKE ALL ON live_desk_usage_daily FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON live_desk_usage FROM authenticated;
        REVOKE ALL ON live_desk_usage_daily FROM authenticated;
    END IF;
END
$$;

-- First fill: everything there is. Later runs only need the recent window.
SELECT refresh_live_desk_usage(36500);
