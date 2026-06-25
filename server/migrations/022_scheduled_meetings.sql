-- Scheduled meetings (spec: scheduled meetings, Phase 1b).
--
-- A scheduled meeting is a VoxTranslate room (room_code → join_url) mirrored as a
-- Google Calendar event (the source of truth for time/attendees/RSVP). B2B meetings
-- carry org_id/project_id so that when the room is opened the call auto-links to the
-- project (via a pre-inserted room_business_bindings row) exactly like a live meet.
-- B2C personal meetings leave org_id/project_id NULL.
CREATE TABLE IF NOT EXISTS scheduled_meetings (
    id                       UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    creator_user_id          UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    org_id                   UUID REFERENCES organizations(id) ON DELETE CASCADE,
    project_id               UUID REFERENCES projects(id) ON DELETE SET NULL,
    title                    TEXT NOT NULL,
    description              TEXT,
    scheduled_at             TIMESTAMPTZ NOT NULL,
    end_at                   TIMESTAMPTZ NOT NULL,
    timezone                 TEXT NOT NULL DEFAULT 'UTC',
    room_code                TEXT NOT NULL,
    join_url                 TEXT NOT NULL,
    google_calendar_event_id TEXT,
    google_calendar_id       TEXT NOT NULL DEFAULT 'primary',
    status                   TEXT NOT NULL DEFAULT 'scheduled', -- scheduled | cancelled | completed
    reminder_minutes_before  INTEGER NOT NULL DEFAULT 10,
    reminder_sent_at         TIMESTAMPTZ,
    created_at               TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at               TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_sched_meetings_org     ON scheduled_meetings (org_id, scheduled_at);
CREATE INDEX IF NOT EXISTS idx_sched_meetings_creator ON scheduled_meetings (creator_user_id, scheduled_at);
-- Reminder scheduler hot path: due-but-unsent, still scheduled.
CREATE INDEX IF NOT EXISTS idx_sched_meetings_reminder
    ON scheduled_meetings (scheduled_at)
    WHERE reminder_sent_at IS NULL AND status = 'scheduled';

CREATE TABLE IF NOT EXISTS scheduled_meeting_invitees (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    meeting_id       UUID NOT NULL REFERENCES scheduled_meetings(id) ON DELETE CASCADE,
    user_id          UUID REFERENCES users(id) ON DELETE SET NULL, -- NULL = external email only
    email            TEXT NOT NULL,
    role             TEXT NOT NULL DEFAULT 'attendee',     -- extension point (organizer | attendee)
    rsvp_status      TEXT NOT NULL DEFAULT 'needsAction',  -- mirrors Google: needsAction|accepted|declined|tentative
    reminder_sent_at TIMESTAMPTZ,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (meeting_id, email)
);
CREATE INDEX IF NOT EXISTS idx_meeting_invitees_meeting ON scheduled_meeting_invitees (meeting_id);

ALTER TABLE scheduled_meetings         ENABLE ROW LEVEL SECURITY;
ALTER TABLE scheduled_meeting_invitees ENABLE ROW LEVEL SECURITY;

-- Re-apply the guarded REVOKE (anon/authenticated only exist on Supabase) — same
-- pattern as migration 016.
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
