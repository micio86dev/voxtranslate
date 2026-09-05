-- 053 — Make GDPR Art. 17 erasure able to reach Supabase Storage objects.
--
-- `SafetyService::delete_user` was `DELETE FROM users` relying on FK cascade. It
-- never removed the bytes a user had uploaded, because no table carrying a
-- storage artifact could be resolved to BOTH (a) the person who authored it and
-- (b) an object path, at the moment erasure runs:
--
--   * chat_files             — had no user column at all. `sender_peer_id` is a
--                              per-connection id and `sender_name` a display
--                              string, so an upload was unattributable.
--   * project_voice_messages — attributable via `created_by`, but that FK is
--                              ON DELETE SET NULL: the link is destroyed by the
--                              very statement that should have acted on it.
--
-- Neither could be deleted from `file_url` either: that column holds an EXPIRING
-- SIGNED URL, not an object path, so it is not a durable handle on the bytes.
--
-- Rows written BEFORE this migration keep NULL in the new columns. Those objects
-- are permanently unattributable and are NOT reachable by erasure — a known,
-- accepted limitation recorded in docs/gdpr-readiness-2026-09.md.
--
-- `user_id` cascades, matching the convention already documented on
-- session_participants.user_id and transcript_events.speaker_user_id (004): a
-- row naming a deleted account must not survive. Erasure deletes the storage
-- object BEFORE the row, so the cascade can never strand bytes.

ALTER TABLE chat_files
    ADD COLUMN IF NOT EXISTS user_id     UUID REFERENCES users(id) ON DELETE CASCADE,
    ADD COLUMN IF NOT EXISTS object_path TEXT;

CREATE INDEX IF NOT EXISTS idx_chat_files_user
    ON chat_files (user_id) WHERE user_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_pvm_created_by
    ON project_voice_messages (created_by) WHERE created_by IS NOT NULL;
