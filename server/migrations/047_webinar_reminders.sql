-- 047 — Webinar "starting soon" / "live now" reminders for the host's friends.
--
-- Mirrors the scheduled-meeting reminder shape (scheduled_meetings.reminder_*):
--   * reminder_minutes_before — lead time for a SCHEDULED public webinar (default 10).
--   * reminder_sent_at        — dedup marker (NULL = not yet notified; set once fired).
--
-- Both the time-based scheduler (public + scheduled) and the go-live hook (public +
-- unscheduled) claim rows by flipping reminder_sent_at from NULL to now(), so each
-- webinar notifies the host's accepted friends at most once.
ALTER TABLE webinars
    ADD COLUMN IF NOT EXISTS reminder_minutes_before INTEGER NOT NULL DEFAULT 10,
    ADD COLUMN IF NOT EXISTS reminder_sent_at TIMESTAMPTZ;
