-- Live Connect (spec 0121, S5): the host's cloned voice for an Enhanced conversation.
--
--   live_sessions.use_voice_clone   the host opted, for THIS conversation, into having the other
--                                   phones speak the translation of the host's words with the
--                                   host's own cloned voice (`users.cartesia_voice_id`, migration
--                                   028). Off by default: a clone is a biometric-adjacent
--                                   identifier, so it is never used unless the host asked for it
--                                   on this conversation. Existing rows keep `false`, which is
--                                   exactly what they were. The clone id itself is NOT stored
--                                   here: it is read from the user's row when a phone needs it, so
--                                   nothing here is voice material and erasing a conversation has
--                                   nothing extra to delete.
--
-- Additive and idempotent: `ADD COLUMN IF NOT EXISTS`, guarded with `to_regclass`. Other
-- checkouts share a development database, so nothing here may fail when run twice or by a build
-- that does not know about the column.
DO $$
BEGIN
    IF to_regclass('public.live_sessions') IS NOT NULL THEN
        ALTER TABLE live_sessions
            ADD COLUMN IF NOT EXISTS use_voice_clone BOOLEAN NOT NULL DEFAULT false;
    END IF;
END
$$;
