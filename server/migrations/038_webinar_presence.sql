-- 038 — Webinar presence + timed event history (SPEC "Webinar Mode" Fase 4).
--
-- `webinar_participants`: one row per (webinar, guest) — the approximate audience,
-- with watch time + conversion attribution fields (later phases). `webinar_events`:
-- the timed join/leave/lang_change/heartbeat log that powers presence counts now and
-- analytics/reports later (Fase 9). Realtime presence itself is tracked in-memory
-- (Axum WS); these tables persist the history. Conventions mirror 037_webinars:
-- FKs to the app's own tables, CHECK on the small event enum, RLS default-deny.

CREATE TABLE IF NOT EXISTS webinar_participants (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    webinar_id          UUID NOT NULL REFERENCES webinars(id) ON DELETE CASCADE,
    -- Anonymous guest identity (cookie/localStorage, F0-4). Not a users FK.
    guest_id            UUID NOT NULL,
    -- Set if the guest was (or later) a signed-in user.
    user_id             UUID REFERENCES users(id) ON DELETE SET NULL,
    language_code       TEXT,
    first_seen          TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen           TIMESTAMPTZ NOT NULL DEFAULT now(),
    joined_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    total_watch_seconds INTEGER NOT NULL DEFAULT 0,
    approx_country      TEXT,
    user_agent         TEXT,
    -- CTA conversion attribution (Fase 8), unused until then.
    converted_signup    BOOLEAN NOT NULL DEFAULT FALSE,
    converted_user_id   UUID REFERENCES users(id) ON DELETE SET NULL,
    -- One participant row per guest per webinar (re-joins update it).
    UNIQUE (webinar_id, guest_id)
);
CREATE INDEX IF NOT EXISTS idx_webinar_participants_webinar ON webinar_participants (webinar_id);

CREATE TABLE IF NOT EXISTS webinar_events (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    webinar_id     UUID NOT NULL REFERENCES webinars(id) ON DELETE CASCADE,
    participant_id UUID REFERENCES webinar_participants(id) ON DELETE SET NULL,
    type           TEXT NOT NULL CHECK (type IN (
                       'join', 'leave', 'lang_change', 'heartbeat',
                       'lang_activated', 'lang_deactivated')),
    language_code  TEXT,
    at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    meta           JSONB NOT NULL DEFAULT '{}'::jsonb
);
CREATE INDEX IF NOT EXISTS idx_webinar_events_webinar_at ON webinar_events (webinar_id, at);

-- ---- RLS: default-deny defense-in-depth (mirrors 037) ------------------------
ALTER TABLE webinar_participants ENABLE ROW LEVEL SECURITY;
ALTER TABLE webinar_events ENABLE ROW LEVEL SECURITY;

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
    REVOKE ALL ON webinar_participants FROM anon;
    REVOKE ALL ON webinar_events FROM anon;
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
    REVOKE ALL ON webinar_participants FROM authenticated;
    REVOKE ALL ON webinar_events FROM authenticated;
  END IF;
END $$;
