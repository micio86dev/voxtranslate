-- 050 — Make finished WEBINARS semantically searchable through the SAME
-- `transcript_embeddings` table the meets use (webinar-analytics epic, Phase C).
--
-- A webinar has no `call_sessions` row (its media is off-box; its history lives in
-- `webinar_transcripts`, migration 040). To let the existing Business search KNN
-- (`business::search`) surface webinar hits with zero schema divergence, we add a
-- nullable `webinar_id` owner column and reuse the denormalized `org_id`/`project_id`
-- scope columns already on the table — so the hot-path `(org_id, project_id)` scope
-- filter Just Works for webinar rows too.
--
-- Owner columns on `transcript_embeddings` today (after 030/034/036):
--   * call transcript rows   → transcript_id + session_id set
--   * voice-assistant rows   → interaction_id set; transcript_id + session_id NULL
--   * webinar rows (THIS)    → webinar_id set; session_id + transcript_id NULL
--
-- `session_id` is ALREADY nullable (migration 036 dropped its NOT NULL for the
-- voice-assistant path), so this migration only ADDS `webinar_id` + a guard.

ALTER TABLE transcript_embeddings
    ADD COLUMN IF NOT EXISTS webinar_id UUID
        REFERENCES webinars(id) ON DELETE CASCADE;

-- Mutual-exclusion guard: a row is NEVER owned by both a call session and a
-- webinar. We deliberately do NOT require "exactly one of (session_id, webinar_id)
-- is non-null" because voice-assistant rows legitimately have BOTH NULL (they are
-- owned by `interaction_id`, see 034/036). Enforcing exactly-one here would reject
-- every existing assistant embedding. This constraint captures the real invariant:
-- the call-owner and the webinar-owner are mutually exclusive.
ALTER TABLE transcript_embeddings
    ADD CONSTRAINT transcript_embeddings_owner_excl
        CHECK (NOT (session_id IS NOT NULL AND webinar_id IS NOT NULL));

-- Sparse index: only covers webinar embedding rows (webinar_id IS NOT NULL), used
-- for the idempotency skip/delete on re-finalize and cascade lookups. Mirrors the
-- interaction_id partial index added in 034.
CREATE INDEX IF NOT EXISTS idx_transcript_embeddings_webinar
    ON transcript_embeddings (webinar_id)
    WHERE webinar_id IS NOT NULL;
