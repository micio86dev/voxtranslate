-- Live Connect (spec 0121): keep the conversation's billing ledger when an account is deleted.
--
-- `live_participants.user_id` was ON DELETE CASCADE (migration 065). Deleting the account of a
-- member of staff (or of a host) therefore deleted their participant row, and `live_usage_ticks`
-- hangs off `live_participants(peer_id)` — so the whole tick ledger of every conversation they
-- answered went with it, and the org's wallet debits could no longer be attributed to a project or
-- a desk. The person's NAME is still erased (the application scrubs it before the account goes);
-- what must survive is the billing fact.
--
-- Idempotent: only acts while the foreign key still cascades.
DO $$
BEGIN
    IF to_regclass('public.live_participants') IS NOT NULL
       AND EXISTS (SELECT 1 FROM pg_constraint
                   WHERE conrelid = 'public.live_participants'::regclass
                     AND conname = 'live_participants_user_id_fkey'
                     AND confdeltype = 'c') THEN
        ALTER TABLE live_participants DROP CONSTRAINT live_participants_user_id_fkey;
        ALTER TABLE live_participants
            ADD CONSTRAINT live_participants_user_id_fkey
            FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE SET NULL;
    END IF;
END
$$;
