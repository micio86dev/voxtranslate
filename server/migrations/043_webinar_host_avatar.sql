-- Snapshot the host's avatar at webinar creation time so participants see a face
-- in the waiting overlay even before the stream starts.
-- NULL for webinars created before this migration (backward compatible).
ALTER TABLE webinars ADD COLUMN IF NOT EXISTS host_avatar_url TEXT;
