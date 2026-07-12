-- 042 — Webinar public/private visibility.
--
-- A webinar is PRIVATE by default (reachable only via its direct `/w/{code}`
-- link) or PUBLIC (also discoverable via the public list endpoint, rendered on
-- the home page next to the public rooms). Mirrors the idempotent-ALTER style of
-- 041_webinar_chat: a plain column add on an existing table needs no RLS change.

ALTER TABLE webinars ADD COLUMN IF NOT EXISTS visibility TEXT NOT NULL DEFAULT 'private'
    CHECK (visibility IN ('public', 'private'));

-- Partial index backing the public list query (visibility = 'public' rows only).
CREATE INDEX IF NOT EXISTS idx_webinars_public
    ON webinars (visibility, status)
    WHERE visibility = 'public';
