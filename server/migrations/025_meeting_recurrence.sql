-- Recurring meetings: store the RRULE for a recurring series (Google Calendar is
-- the source of truth and sends native invites/reminders for each occurrence). NULL
-- = a one-off meeting. scheduled_at remains the first occurrence.
ALTER TABLE scheduled_meetings ADD COLUMN IF NOT EXISTS recurrence TEXT;
