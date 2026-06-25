-- Configurable notifications (spec: scheduled meetings, Phase 1d).
--
-- Per-user web-push subscriptions, per-type×channel preferences, quiet-hours
-- settings, and a persistent in-app notification log (the bell/history).
CREATE TABLE IF NOT EXISTS user_push_subscriptions (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    endpoint    TEXT NOT NULL,
    p256dh      TEXT NOT NULL,
    auth        TEXT NOT NULL,
    user_agent  TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (user_id, endpoint)
);
CREATE INDEX IF NOT EXISTS idx_push_subs_user ON user_push_subscriptions (user_id);

-- Per-type, per-channel toggle. Absence of a row = channel ON by default.
CREATE TABLE IF NOT EXISTS notification_preferences (
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    type    TEXT NOT NULL,   -- meeting_invited | meeting_reminder | meeting_updated | meeting_cancelled
    channel TEXT NOT NULL,   -- push | email | in_app
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    PRIMARY KEY (user_id, type, channel)
);

-- One sparse row per user for quiet hours / timezone (defaults applied in app).
CREATE TABLE IF NOT EXISTS notification_settings (
    user_id           UUID PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    quiet_hours_start SMALLINT,           -- 0..23 local, NULL = off
    quiet_hours_end   SMALLINT,
    timezone          TEXT NOT NULL DEFAULT 'UTC',
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- In-app notification center (bell + history).
CREATE TABLE IF NOT EXISTS notifications (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id    UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    type       TEXT NOT NULL,
    title      TEXT NOT NULL,
    body       TEXT NOT NULL,
    data       JSONB NOT NULL DEFAULT '{}'::jsonb,   -- {meeting_id, room_code, join_url, ...}
    read_at    TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_notifications_user_unread
    ON notifications (user_id, created_at DESC)
    WHERE read_at IS NULL;

ALTER TABLE user_push_subscriptions   ENABLE ROW LEVEL SECURITY;
ALTER TABLE notification_preferences  ENABLE ROW LEVEL SECURITY;
ALTER TABLE notification_settings     ENABLE ROW LEVEL SECURITY;
ALTER TABLE notifications             ENABLE ROW LEVEL SECURITY;

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
    REVOKE ALL ON ALL TABLES IN SCHEMA public FROM anon;
    REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM anon;
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
    REVOKE ALL ON ALL TABLES IN SCHEMA public FROM authenticated;
    REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM authenticated;
  END IF;
END $$;
