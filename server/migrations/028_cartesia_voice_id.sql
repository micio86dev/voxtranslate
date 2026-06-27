-- Per-user Cartesia cloned-voice id (spec 0108, Enhanced tier). Captured once at the
-- pre-join "voice preparation" step via Instant Voice Cloning, then reused for every
-- Enhanced call so the user is heard, translated, in their own voice. Propagated to peers
-- in the room presence payload. NULL = no clone yet → a default Cartesia voice is used.
-- Additive and backward-compatible.
ALTER TABLE users ADD COLUMN IF NOT EXISTS cartesia_voice_id TEXT;
