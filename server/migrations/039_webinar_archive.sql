-- 039 — Webinar soft-archive (③). A nullable timestamp: archived webinars are
-- hidden from the host's active list but NEVER deleted — the data (participants,
-- events, transcripts, recordings) is preserved and stays visible in the dashboard.

ALTER TABLE webinars ADD COLUMN IF NOT EXISTS archived_at TIMESTAMPTZ;

-- Partial index for the common "active (non-archived) webinars of an org, newest
-- first" list query.
CREATE INDEX IF NOT EXISTS idx_webinars_org_active
    ON webinars (org_id, created_at DESC)
    WHERE archived_at IS NULL;
