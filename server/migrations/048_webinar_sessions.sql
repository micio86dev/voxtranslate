-- 048 — Webinar session record + host cost (Webinar Analytics, Phase A).
--
-- When a live webinar FINALIZES (publish-stopped / status→ended), we materialize a
-- one-row-per-run `webinar_sessions` record — the meet-like SESSION RECORD for a
-- broadcast — and bill the HOST's org for it. The raw timeline lives in
-- `webinar_events` (038: join/leave, each join carries the participant's language);
-- this table is the finalized rollup so the dashboard/analytics never re-sweep the
-- event log.
--
-- Cost mirrors the meet per-tick meter (`usage.rs`): per broadcast-minute we bill
-- `ratePerMin × K`, where K is the number of DISTINCT participant languages that
-- differ from the webinar's `source_language` during that minute (dynamic as viewers
-- join/leave/change language). K=0 (no foreign-language viewers) ⇒ no charge, exactly
-- like the meet meter skipping a tick. The per-tier `ratePerMin` already bundles
-- STT+translation+TTS per stream (Enhanced uses the higher Cartesia rate).
--
-- Conventions mirror 037/038: FKs to the app's own tables, RLS default-deny (the
-- server connects as the owning role, exempt; anon/authenticated get nothing).
-- `cost_credits` is the INTEGER org-credit currency (100 credits = $1), same pool as
-- `organization_credits_transactions`.

CREATE TABLE IF NOT EXISTS webinar_sessions (
    id                       UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    -- One finalized run per webinar. UNIQUE ⇒ the finalization batch is idempotent:
    -- a second publish-stopped can't insert a duplicate and double-charge the org.
    webinar_id               UUID NOT NULL UNIQUE REFERENCES webinars(id) ON DELETE CASCADE,
    org_id                   UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- Snapshot of who hosted; keep the record if the host's account is deleted.
    host_user_id             UUID REFERENCES users(id) ON DELETE SET NULL,
    actual_start             TIMESTAMPTZ NOT NULL,
    actual_end               TIMESTAMPTZ NOT NULL,
    -- Wall-clock broadcast length (actual_end − actual_start), the billed span.
    duration_seconds         INTEGER NOT NULL DEFAULT 0,
    -- Copied from the webinar at finalization for the analytics rollup.
    scheduled_start          TIMESTAMPTZ,
    scheduled_end            TIMESTAMPTZ,
    -- Reserved for a future host-presence signal; equals duration_seconds today
    -- (no separate host on/off-air tracking yet).
    host_online_seconds      INTEGER NOT NULL DEFAULT 0,
    -- Largest concurrent audience observed while live (from the event sweep).
    peak_viewers             INTEGER NOT NULL DEFAULT 0,
    -- Count of DISTINCT non-source languages that were EVER active during the run —
    -- the analytics "how many languages did we translate into" number.
    translated_language_count INTEGER NOT NULL DEFAULT 0,
    -- Credits deducted from the host's org for this run (INTEGER pool, 100 = $1).
    cost_credits             INTEGER NOT NULL DEFAULT 0,
    created_at               TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_webinar_sessions_org_created
    ON webinar_sessions (org_id, created_at DESC);

-- ---- RLS: default-deny defense-in-depth (mirrors 037/038) --------------------
ALTER TABLE webinar_sessions ENABLE ROW LEVEL SECURITY;

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
    REVOKE ALL ON webinar_sessions FROM anon;
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
    REVOKE ALL ON webinar_sessions FROM authenticated;
  END IF;
END $$;
