-- Per-user UI locale, captured at Google login from the frontend (the language the
-- user is actually using the product in). Used to localize outbound notifications
-- (meeting invites/reminders/updates) in the *recipient's* language rather than the
-- sender's or a hardcoded English string. NULL = unknown → English fallback.
ALTER TABLE users ADD COLUMN IF NOT EXISTS locale TEXT;
