-- Premium translation engine (spec 0093). Tag each usage session with the engine
-- the speaker chose so billing and analytics can attribute cost per engine. It's a
-- TEXT id, not an enum, so future engines need no schema change; existing rows and
-- guests both default to 'standard'. Idempotent: re-running migrations is a no-op.
ALTER TABLE usage_sessions
    ADD COLUMN IF NOT EXISTS engine_id TEXT NOT NULL DEFAULT 'standard';
