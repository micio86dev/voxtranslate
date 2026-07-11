-- 035 — Add interaction_kind discriminator to voice_assistant_interactions.
--
-- This column lets us distinguish between the B2B Voice Assistant (project RAG Q&A)
-- and the new Dashboard Help Assistant (static product knowledge) in the same table.
-- Existing rows keep the default 'project_qa', which is accurate — they came from
-- the RAG-grounded voice assistant. New help-assistant sessions will write 'help_assistant'
-- explicitly at INSERT time.
--
-- Safe rollback: the column has a DEFAULT, so removing the feature flag and
-- not registering the help-assistant route is sufficient; old rows are unaffected.

-- ---- Add interaction_kind column --------------------------------------------
ALTER TABLE voice_assistant_interactions
    ADD COLUMN IF NOT EXISTS interaction_kind TEXT NOT NULL DEFAULT 'project_qa';

-- ---- Backfill: all existing rows already match the default ------------------
-- (No UPDATE needed — ADD COLUMN with DEFAULT applies the value to existing rows.)

-- ---- Constraint: only known kinds accepted -----------------------------------
-- Prevents future insert bugs from silently writing garbage values.
ALTER TABLE voice_assistant_interactions
    ADD CONSTRAINT chk_interaction_kind
        CHECK (interaction_kind IN ('project_qa', 'help_assistant'));

-- ---- Index: scoped queries by kind (analytics, history filtering) -----------
CREATE INDEX IF NOT EXISTS idx_voice_assistant_interactions_kind
    ON voice_assistant_interactions (org_id, interaction_kind, created_at DESC);
