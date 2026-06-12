-- Chat file upload (spec 0018): metadata for files attached to the room chat.
-- The bytes live in Supabase Storage (bucket `chat-files`); this row records the
-- public URL plus the descriptive metadata the chat bubble and any later file
-- history need. `session_id` ties the file to the call lifetime so it is purged
-- with the session (and with the account, transitively) — same GDPR lifecycle
-- as transcript_events.
CREATE TABLE IF NOT EXISTS chat_files (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id     UUID NOT NULL REFERENCES call_sessions(id) ON DELETE CASCADE,
    room           TEXT NOT NULL,
    sender_peer_id TEXT NOT NULL,
    sender_name    TEXT NOT NULL,
    file_url       TEXT NOT NULL,
    file_name      TEXT NOT NULL,
    file_type      TEXT NOT NULL,   -- MIME content type, e.g. audio/mpeg
    size_bytes     BIGINT NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_chat_files_session
    ON chat_files (session_id, created_at);
