-- 037 — Webinar Mode: broadcast (1-to-many) webinars. Phase 0-1 subset.
--
-- Live-translated webinars (SPEC "Webinar Mode"): a NEW asymmetric delivery path,
-- separate from the P2P mesh. The host publishes mic (+optional webcam) via WHIP;
-- guests watch via LL-HLS. This migration is the Phase 0-1 subset of SPEC §7 — the
-- `webinars` table only. Phase 2+ fields (record_*, voice_clone, project_id,
-- google_event_id) exist here but stay unused until their phase lands.
--
-- Conventions mirror 016_business_workspace.sql: FKs target the app's own tables,
-- authorization is enforced in the Rust API layer (require_role + org checks), and
-- RLS is ENABLE-without-policy default-deny defense-in-depth (the server connects as
-- the owning role, exempt from RLS; anon/authenticated get nothing). No triggers —
-- `updated_at` is bumped by app code. Unlike the org tables, the small closed enums
-- here (`status`, `tier`) are guarded by CHECK constraints, not just comments.

CREATE TABLE IF NOT EXISTS webinars (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id            UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- Org-owned: keep the webinar if the host's personal account is deleted.
    host_user_id      UUID REFERENCES users(id) ON DELETE SET NULL,
    -- Short, non-guessable join code (generated in F0-2). UNIQUE ⇒ backing index.
    code              TEXT UNIQUE NOT NULL,
    title             TEXT NOT NULL,
    description       TEXT,
    source_language   TEXT NOT NULL,
    tier              TEXT NOT NULL DEFAULT 'enhanced'
                          CHECK (tier IN ('standard', 'enhanced')),
    status            TEXT NOT NULL DEFAULT 'scheduled'
                          CHECK (status IN ('scheduled', 'live', 'ended', 'cancelled')),
    scheduled_start   TIMESTAMPTZ,
    scheduled_end     TIMESTAMPTZ,
    actual_start      TIMESTAMPTZ,
    actual_end        TIMESTAMPTZ,
    -- Phase 2+ pre-join toggles / associations. Present now (SPEC §7) but unused
    -- until their phase: recording (Fase 6), transcript-to-project (Fase 6),
    -- voice clone (Fase 3), Google Calendar (Fase 7).
    record_video      BOOLEAN NOT NULL DEFAULT FALSE,
    record_transcript BOOLEAN NOT NULL DEFAULT FALSE,
    voice_clone       BOOLEAN NOT NULL DEFAULT FALSE,
    project_id        UUID REFERENCES projects(id) ON DELETE SET NULL,
    google_event_id   TEXT,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- Host dashboard lists an org's webinars newest-first.
CREATE INDEX IF NOT EXISTS idx_webinars_org_created ON webinars (org_id, created_at DESC);

-- ---- RLS: default-deny defense-in-depth (mirrors 016_business_workspace.sql) ----
-- No permissive policy: the owning role the server uses is exempt; with RLS on and
-- no policy, PostgREST anon/authenticated can read/write NOTHING. Real requests are
-- authorized in the Rust API layer.
ALTER TABLE webinars ENABLE ROW LEVEL SECURITY;

-- Guarded REVOKE (anon/authenticated exist only on Supabase, not local/CI Postgres).
DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
    REVOKE ALL ON webinars FROM anon;
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
    REVOKE ALL ON webinars FROM authenticated;
  END IF;
END $$;
