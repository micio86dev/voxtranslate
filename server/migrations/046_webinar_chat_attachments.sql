-- 046 — Webinar chat: per-message avatar URL + file attachment metadata.
ALTER TABLE webinar_chat_messages
    ADD COLUMN IF NOT EXISTS avatar_url TEXT,
    ADD COLUMN IF NOT EXISTS attachment_url TEXT,
    ADD COLUMN IF NOT EXISTS attachment_name TEXT,
    ADD COLUMN IF NOT EXISTS attachment_content_type TEXT,
    ADD COLUMN IF NOT EXISTS attachment_size BIGINT;
