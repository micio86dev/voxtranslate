-- Live Connect (spec 0121): one host, up to three guests on the SAME invitation.
--
-- A conversation used to be exactly two seats. The invitation was single-use and a partial unique
-- index on `live_participants` allowed one guest row per session (`idx_live_participants_guest_side`,
-- migration 065). Now the host picks how many guests the link admits (1-3; three because a room
-- holds four peers) and the limit is enforced IN CODE, under a row lock on the session, because a
-- unique index cannot express "at most N".
--
--   live_sessions.max_guests   how many guests this session admits. Existing rows keep 1, which is
--                              exactly what they were: nothing about a conversation already in
--                              progress changes.
--   idx_live_participants_guest_side   no longer UNIQUE (see below). The owner-side unique index
--                              stays: a conversation still has exactly one host (or one staff member).
--
-- Additive and idempotent: `ADD COLUMN IF NOT EXISTS`, a guarded NOT VALID constraint in its own
-- DO block, an index dropped only while it is still the unique one; every ALTER is guarded with
-- `to_regclass`. Other checkouts share a development database, so nothing here may fail when run
-- twice or when run by a build that does not know about the column.
DO $$
BEGIN
    IF to_regclass('public.live_sessions') IS NOT NULL THEN
        ALTER TABLE live_sessions ADD COLUMN IF NOT EXISTS max_guests SMALLINT NOT NULL DEFAULT 1;
    END IF;
END
$$;

DO $$
BEGIN
    IF to_regclass('public.live_sessions') IS NOT NULL
       AND NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'live_sessions_max_guests_check') THEN
        ALTER TABLE live_sessions ADD CONSTRAINT live_sessions_max_guests_check
            CHECK (max_guests BETWEEN 1 AND 3) NOT VALID;
    END IF;
END
$$;

-- The guest-side UNIQUE index goes. It is replaced by a plain index under the SAME name, on purpose:
-- `065_live_connect.sql` says `CREATE UNIQUE INDEX IF NOT EXISTS idx_live_participants_guest_side`,
-- and `IF NOT EXISTS` matches by name, so re-running 065 (the schema suite does, to prove it is
-- idempotent; a drifted database may) finds the name taken and does nothing, instead of rebuilding
-- the unique index and failing on a conversation that has two guests. The plain index also serves
-- the capacity count (`guests in this session`).
DO $$
BEGIN
    IF to_regclass('public.live_participants') IS NOT NULL THEN
        IF EXISTS (
            SELECT 1
              FROM pg_class c
              JOIN pg_index i ON i.indexrelid = c.oid
             WHERE c.relname = 'idx_live_participants_guest_side'
               AND c.relnamespace = 'public'::regnamespace
               AND i.indisunique
        ) THEN
            DROP INDEX public.idx_live_participants_guest_side;
        END IF;
        CREATE INDEX IF NOT EXISTS idx_live_participants_guest_side
            ON live_participants (session_id) WHERE role = 'guest';
    END IF;
END
$$;
