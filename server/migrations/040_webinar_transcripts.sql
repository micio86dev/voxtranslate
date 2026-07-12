-- 040 — Webinar realtime subtitle transcripts (SPEC "Webinar Mode" Fase 2).
--
-- One row per FINALIZED host utterance while a webinar is live: the original
-- text + source language, plus the `{ lang: text }` translation map fanned out to
-- every viewer language present at that moment. Written best-effort (fire-and-
-- forget) from the STT processor ONLY when `webinars.record_transcript` is set —
-- it powers the post-webinar transcript/recap, never the live path. Realtime
-- subtitle delivery is in-memory over the presence WS; this table is history.
-- Conventions mirror 038_webinar_presence: FK cascade + RLS default-deny.

CREATE TABLE IF NOT EXISTS webinar_transcripts (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    webinar_id    UUID NOT NULL REFERENCES webinars(id) ON DELETE CASCADE,
    original_text TEXT NOT NULL,
    original_lang TEXT NOT NULL,
    -- `{ "<lang>": "<translated text>" }`, always incl. the source language.
    translations  JSONB NOT NULL DEFAULT '{}'::jsonb,
    spoken_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_webinar_transcripts_webinar_spoken
    ON webinar_transcripts (webinar_id, spoken_at);

-- ---- RLS: default-deny defense-in-depth (mirrors 038) -----------------------
ALTER TABLE webinar_transcripts ENABLE ROW LEVEL SECURITY;

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
    REVOKE ALL ON webinar_transcripts FROM anon;
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
    REVOKE ALL ON webinar_transcripts FROM authenticated;
  END IF;
END $$;
