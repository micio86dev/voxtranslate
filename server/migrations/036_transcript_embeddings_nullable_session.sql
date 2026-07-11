-- 036 — Relax transcript_embeddings.session_id to nullable.
--
-- Migration 034 made `transcript_id` nullable and added `interaction_id` so the
-- Voice/Help Assistant re-index path could store embeddings that reference a
-- `voice_assistant_interactions` row instead of a call transcript. But it left
-- `session_id` (added NOT NULL in migration 030, FK to call_sessions) untouched.
--
-- Assistant interactions have NO call session, so the re-index INSERT omits
-- `session_id` → it defaults to NULL → the NOT NULL constraint from 030 rejects
-- every assistant embedding. Result: assistant Q&A was persisted but NEVER
-- indexed into the RAG knowledge base (silent warn on each session close).
--
-- Making it nullable is additive: all existing call-transcript rows keep their
-- non-NULL session_id; the FK (ON DELETE CASCADE) still applies when set.

ALTER TABLE transcript_embeddings
    ALTER COLUMN session_id DROP NOT NULL;
