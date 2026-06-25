-- 020 — AI project storyboards (spec 0106, Phase-4).
--
-- An on-demand, AI-generated narrative of a project's trajectory: timeline +
-- history, per-member contribution, collaboration cadence, and — when the admin
-- supplies a target workflow — an analysis of how the observed activity aligns
-- with or diverges from it. One LATEST storyboard per project (UNIQUE project_id,
-- upserted on regenerate); the source data (sessions + participants) is never
-- deleted on team/membership changes, so a member joining/leaving loses nothing.

CREATE TABLE IF NOT EXISTS project_storyboards (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id      UUID NOT NULL UNIQUE REFERENCES projects(id) ON DELETE CASCADE,
    org_id          UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- Optional "ideal workflow" the admin wants the project measured against.
    target_workflow TEXT,
    markdown        TEXT NOT NULL,
    model           TEXT NOT NULL,
    -- Org-owned: keep the storyboard if the generator's account is deleted.
    generated_by    UUID REFERENCES users(id) ON DELETE SET NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_project_storyboards_org ON project_storyboards (org_id);

ALTER TABLE project_storyboards ENABLE ROW LEVEL SECURITY;

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
    REVOKE ALL ON ALL TABLES IN SCHEMA public FROM anon;
    REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM anon;
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
    REVOKE ALL ON ALL TABLES IN SCHEMA public FROM authenticated;
    REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM authenticated;
  END IF;
END $$;
