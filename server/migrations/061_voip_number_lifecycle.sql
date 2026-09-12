-- 061 — a number's life, and what it cost (spec 0115).
--
-- `voip_numbers` has existed since 056 and nothing in the product ever wrote to it: rows
-- arrived over psql, by hand, and `verification_status` was only ever read. This is the
-- schema the buying, verifying and keeping of a number needs.

ALTER TABLE voip_numbers
    -- Where the number is in its life, in the terms a customer needs rather than a
    -- carrier's internal vocabulary. `pending_regulatory` is its own state because saying
    -- "active" while a regulator is the blocker is a lie with a fine attached.
    ADD COLUMN IF NOT EXISTS status TEXT NOT NULL DEFAULT 'active'
        CHECK (status IN ('ordering', 'pending_regulatory', 'active', 'suspended',
                          'releasing', 'released', 'failed')),
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

-- Scoped to the org: two organisations generating the same key is a coincidence, not a
-- duplicate purchase. Partial, because rows inserted before this migration have none.
CREATE UNIQUE INDEX IF NOT EXISTS idx_voip_numbers_purchase_key
    ON voip_numbers (org_id, purchase_key) WHERE purchase_key IS NOT NULL;

-- The renewal sweep's query: everything due, oldest first.
CREATE INDEX IF NOT EXISTS idx_voip_numbers_renewal
    ON voip_numbers (next_renewal_at)
    WHERE status IN ('active', 'suspended') AND next_renewal_at IS NOT NULL;
