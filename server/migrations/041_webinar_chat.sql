-- 041 — Webinar auto-translated chat (SPEC "Webinar Mode" Feature ⑤).
--
-- An optional per-webinar chat panel. EVERYONE can send — the host (JWT) and
-- unauthenticated viewers (the `guest_id` cookie), each with a cosmetic display
-- name. Everyone reads messages auto-translated into their own language. When the
-- host enables chat at creation time (`webinars.chat_enabled`), every message is
-- PERSISTED here (recorded) — the gate is `chat_enabled` itself, independent of
-- `record_transcript`. Delivery is realtime over the presence WS; this table is
-- history for the post-webinar record + the initial history fetch.
--
-- Conventions mirror 040_webinar_transcripts: FK cascade + RLS default-deny.

ALTER TABLE webinars ADD COLUMN IF NOT EXISTS chat_enabled BOOLEAN NOT NULL DEFAULT FALSE;

CREATE TABLE IF NOT EXISTS webinar_chat_messages (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    webinar_id    UUID NOT NULL REFERENCES webinars(id) ON DELETE CASCADE,
    -- 'host' = an org member sending with a valid JWT; 'guest' = the guest cookie.
    sender_kind   TEXT NOT NULL CHECK (sender_kind IN ('host', 'guest')),
    -- The host's user id or the guest's server-side cookie UUID. NEVER exposed to
    -- other participants (the public views carry only the cosmetic display name).
    sender_id     UUID NOT NULL,
    -- Cosmetic only; a guest can't spoof identity (the real anchor is sender_id).
    display_name  TEXT NOT NULL,
    original_text TEXT NOT NULL,
    -- The sender's UI language label; the "auto" fan-out drives the real keys.
    original_lang TEXT NOT NULL,
    -- `{ "<lang>": "<translated text>" }`, one key per live viewer language.
    translations  JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_webinar_chat_messages_webinar_created
    ON webinar_chat_messages (webinar_id, created_at);

-- ---- RLS: default-deny defense-in-depth (mirrors 040) -----------------------
ALTER TABLE webinar_chat_messages ENABLE ROW LEVEL SECURITY;

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
    REVOKE ALL ON webinar_chat_messages FROM anon;
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
    REVOKE ALL ON webinar_chat_messages FROM authenticated;
  END IF;
END $$;
