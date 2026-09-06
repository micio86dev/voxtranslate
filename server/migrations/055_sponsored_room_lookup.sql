-- Org-sponsored rooms: every WebSocket join resolves `room_code` -> the org that
-- booked the meeting, so the lookup must not scan. Only rows that can sponsor a
-- room are worth indexing — a cancelled meeting or a personal one never does.
CREATE INDEX IF NOT EXISTS idx_sched_meetings_room_code_org
    ON scheduled_meetings (room_code)
    WHERE org_id IS NOT NULL AND status = 'scheduled';
