-- 061 — a number's life, and what it cost (spec 0115).
--
-- `voip_numbers` has existed since 056 and nothing in the product ever wrote to it: rows
-- arrived over psql, by hand, and `verification_status` was only ever read. This is the
-- schema the buying, verifying and keeping of a number needs.

ALTER TABLE voip_numbers
    -- Where the number is in its life, in the terms a customer needs rather than a
    -- carrier's internal vocabulary. `pending_regulatory` is its own state because saying
    -- "active" while a regulator is the blocker is a lie with a fine attached.
    -- The CHECK is added separately, below. An inline one rides on `IF NOT EXISTS`: if
    -- the column is already there the whole clause is a no-op, the constraint is never
    -- created, and the column is left accepting anything — silently, which is the worst
    -- way for a constraint to be missing.
    ADD COLUMN IF NOT EXISTS status TEXT NOT NULL DEFAULT 'active',
    -- Why, in the provider's words, when the status alone does not explain itself.
    ADD COLUMN IF NOT EXISTS status_reason TEXT,

    -- The pricing snapshot. Spec 0111 R27 requires this for calls; a number that renews
    -- for years needs it more — an invoice from eighteen months ago must never be
    -- recomputed at today's markup.
    ADD COLUMN IF NOT EXISTS provider_monthly_usd NUMERIC(12,6),
    ADD COLUMN IF NOT EXISTS provider_setup_usd NUMERIC(12,6),
    ADD COLUMN IF NOT EXISTS markup_rate NUMERIC(6,4),
    ADD COLUMN IF NOT EXISTS customer_monthly_usd NUMERIC(12,6),
    ADD COLUMN IF NOT EXISTS customer_setup_usd NUMERIC(12,6),
    ADD COLUMN IF NOT EXISTS currency TEXT NOT NULL DEFAULT 'USD',

    -- OUR idempotency key for the purchase that created this row. A retry after a timeout
    -- must not buy a second number: the customer would pay for it monthly, for ever,
    -- without ever having asked. The uniqueness is enforced below, in the database, not in
    -- a handler that could forget — the same reasoning as `voip_provider_events`.
    ADD COLUMN IF NOT EXISTS purchase_key TEXT,

    ADD COLUMN IF NOT EXISTS next_renewal_at TIMESTAMPTZ,
    -- When the wallet could not cover a renewal. A suspended number is still costing US:
    -- that is a deliberate, bounded loss, taken so a business does not lose its telephone
    -- number over a card that expired.
    ADD COLUMN IF NOT EXISTS suspended_at TIMESTAMPTZ,
    -- The provider's handle on an in-flight caller-id verification.
    ADD COLUMN IF NOT EXISTS verification_id TEXT,
    ADD COLUMN IF NOT EXISTS regulatory_requirement TEXT;

-- The status vocabulary, added on its own so it exists whether or not the column did.
-- `pending_regulatory` is its own state because saying "active" while a regulator is the
-- blocker is a lie with a fine attached.
--
-- NOT VALID on purpose: it applies to every INSERT and UPDATE from this moment, which is
-- the protection that matters, while a row that drifted in before this migration cannot
-- fail the ALTER and take the server's boot down with it. Guarding the constraint against
-- an unbootable database, then leaving it able to cause one, would be a wasted guard.
DO $$
BEGIN
    -- `to_regclass`, not `::regclass`: the cast RAISES on a missing table, which would
    -- reintroduce inside the guard exactly the unbootable-database failure the guard is
    -- here to prevent.
    IF to_regclass('public.voip_numbers') IS NOT NULL AND NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'voip_numbers_status_check'
           AND conrelid = to_regclass('public.voip_numbers')
    ) THEN
        ALTER TABLE voip_numbers
            ADD CONSTRAINT voip_numbers_status_check
            CHECK (status IN ('ordering', 'pending_regulatory', 'active', 'suspended',
                              'releasing', 'released', 'failed')) NOT VALID;
    END IF;
END
$$;

-- Scoped to the org: two organisations generating the same key is a coincidence, not a
-- duplicate purchase. Partial, because rows inserted before this migration have none.
CREATE UNIQUE INDEX IF NOT EXISTS idx_voip_numbers_purchase_key
    ON voip_numbers (org_id, purchase_key) WHERE purchase_key IS NOT NULL;

-- The renewal sweep's query: everything due, oldest first.
CREATE INDEX IF NOT EXISTS idx_voip_numbers_renewal
    ON voip_numbers (next_renewal_at)
    WHERE status IN ('active', 'suspended') AND next_renewal_at IS NOT NULL;
