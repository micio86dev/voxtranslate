-- 034 — Voice Assistant interactions (B2B). Stores per-session exchanges between
-- a Business/Enterprise user and the GPT-Realtime voice assistant, grounded by RAG
-- over the org's transcript embeddings.
--
-- Two schema changes to transcript_embeddings:
--   1. transcript_id is made nullable so assistant-question embeddings can be stored
--      without a transcript row (they reference a voice_assistant_interactions row
--      instead, via the new interaction_id FK).
--   2. A nullable interaction_id FK is added for the re-index path.
--
-- voice_assistant_interactions must be created BEFORE the FK is added.

-- ---- Voice assistant interactions -------------------------------------------
-- One row per completed assistant exchange. session_id omitted here — each page-
-- scoped session may produce multiple exchanges, identified by created_at order.
CREATE TABLE IF NOT EXISTS voice_assistant_interactions (
    id                UUID    PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id            UUID    NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    project_id        UUID    REFERENCES projects(id)               ON DELETE SET NULL,
    member_id         UUID    REFERENCES users(id)                  ON DELETE SET NULL,
    user_id           UUID    NOT NULL REFERENCES users(id)         ON DELETE CASCADE,
    user_transcript   TEXT    NOT NULL DEFAULT '',
    ai_response       TEXT    NOT NULL DEFAULT '',
    duration_seconds  INTEGER NOT NULL DEFAULT 0,
    credits_used      INTEGER NOT NULL DEFAULT 0,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Scope filter: org + created_at (the primary list view — reverse-chron per org).
CREATE INDEX IF NOT EXISTS idx_voice_assistant_interactions_org_created
    ON voice_assistant_interactions (org_id, created_at DESC);

-- Secondary scope filter for project-scoped queries.
CREATE INDEX IF NOT EXISTS idx_voice_assistant_interactions_org_project
    ON voice_assistant_interactions (org_id, project_id);

-- ---- Relax transcript_embeddings.transcript_id ------------------------------
-- The column is NOT NULL today (migration 030). Assistant re-index rows have no
-- transcript row — they link to voice_assistant_interactions instead. Making it
-- nullable is additive: all existing rows keep their non-NULL values.
ALTER TABLE transcript_embeddings
    ALTER COLUMN transcript_id DROP NOT NULL;

-- ---- Add interaction_id FK to transcript_embeddings -------------------------
-- nullable: only set for assistant re-index embeddings (transcript_id = NULL).
-- existing call-transcript embeddings keep transcript_id set and interaction_id NULL.
ALTER TABLE transcript_embeddings
    ADD COLUMN IF NOT EXISTS interaction_id UUID
        REFERENCES voice_assistant_interactions(id) ON DELETE CASCADE;

-- Sparse index: only covers the assistant embedding rows (interaction_id IS NOT NULL).
CREATE INDEX IF NOT EXISTS idx_transcript_embeddings_interaction
    ON transcript_embeddings (interaction_id)
    WHERE interaction_id IS NOT NULL;

-- ---- RLS: default-deny defense-in-depth (mirrors 011_rls_lockdown.sql) ------
ALTER TABLE voice_assistant_interactions ENABLE ROW LEVEL SECURITY;

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
    REVOKE ALL ON voice_assistant_interactions FROM anon;
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
    REVOKE ALL ON voice_assistant_interactions FROM authenticated;
  END IF;
END $$;
