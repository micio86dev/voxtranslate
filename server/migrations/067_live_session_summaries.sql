-- Live Connect (spec 0121, S12): one AI-written summary per (conversation, language).
--
-- A summary is CONTENT, exactly like the speech it was written from: it quotes and paraphrases
-- what was said. It therefore hangs off `live_sessions` with ON DELETE CASCADE and is also
-- deleted by `live::erase::erase_content`, so it goes with the conversation (an admin deleting
-- it, retention, account deletion) while the billing facts (the session row, the metered ticks
-- and the `organization_credits_transactions` entry of type 'live_summary') stay.
--
-- The row doubles as the idempotency and concurrency guard of the paid call. A request claims
-- the pair by inserting a `pending` row and charging the organisation in the same transaction;
-- a second request for the same pair finds the row and is charged nothing. On a provider failure
-- the pending row is deleted and the charge refunded together. `created_at` of a pending row is
-- when it was claimed, so a claim left behind by a crashed process can be taken over later
-- without charging twice.
--
-- Idempotent (CLAUDE.md): CREATE ... IF NOT EXISTS, every CHECK its own guarded NOT VALID
-- constraint, RLS default-deny, guarded REVOKE (anon/authenticated exist only on Supabase).
CREATE TABLE IF NOT EXISTS live_session_summaries (
    session_id UUID        NOT NULL REFERENCES live_sessions(id) ON DELETE CASCADE,
    lang       TEXT        NOT NULL,
    status     TEXT        NOT NULL DEFAULT 'ready',            -- 'pending' | 'ready'
    summary    TEXT        NOT NULL DEFAULT '',
    credits    INTEGER     NOT NULL DEFAULT 0,                  -- what the organisation was charged
    created_by UUID        REFERENCES users(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (session_id, lang)
);

DO $$
BEGIN
    IF to_regclass('public.live_session_summaries') IS NOT NULL
       AND NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'live_session_summaries_status_check') THEN
        ALTER TABLE live_session_summaries ADD CONSTRAINT live_session_summaries_status_check
            CHECK (status IN ('pending', 'ready')) NOT VALID;
    END IF;
    IF to_regclass('public.live_session_summaries') IS NOT NULL
       AND NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'live_session_summaries_lang_check') THEN
        ALTER TABLE live_session_summaries ADD CONSTRAINT live_session_summaries_lang_check
            CHECK (lang = lower(lang) AND length(lang) BETWEEN 2 AND 16) NOT VALID;
    END IF;
    IF to_regclass('public.live_session_summaries') IS NOT NULL
       AND NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'live_session_summaries_credits_check') THEN
        ALTER TABLE live_session_summaries ADD CONSTRAINT live_session_summaries_credits_check
            CHECK (credits >= 0) NOT VALID;
    END IF;
    IF to_regclass('public.live_session_summaries') IS NOT NULL
       AND NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'live_session_summaries_ready_check') THEN
        ALTER TABLE live_session_summaries ADD CONSTRAINT live_session_summaries_ready_check
            CHECK (status = 'pending' OR summary <> '') NOT VALID;
    END IF;
END
$$;

ALTER TABLE live_session_summaries ENABLE ROW LEVEL SECURITY;

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON live_session_summaries FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON live_session_summaries FROM authenticated;
    END IF;
END
$$;
