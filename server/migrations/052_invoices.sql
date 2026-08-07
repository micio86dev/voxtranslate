-- 052 — Invoices (spec 0109)
--
-- WHY: consumer credit purchases went through Stripe Checkout in `mode=payment`
-- without `invoice_creation`, so Stripe only ever issued a payment *receipt* —
-- no Invoice object, no number, no PDF. A receipt is not a fiscal document. Org
-- subscriptions already produce real Stripe Invoices, but nothing on our side
-- recorded them, so the Dashboard could not list them without a live Stripe call
-- on every page load.
--
-- This table is a local INDEX of Stripe invoices, not the source of truth. Stripe
-- owns numbering, VAT lines and the PDF; we keep just enough to list and
-- authorise, and re-resolve the (short-lived) PDF URL from Stripe at download
-- time. `stripe_invoice_id` is UNIQUE so webhook replays upsert instead of
-- duplicating.
--
-- Exactly one of `user_id` / `org_id` is set: consumer invoices belong to a user,
-- organization invoices to an org. The CHECK makes that structural rather than a
-- convention someone has to remember.

CREATE TABLE IF NOT EXISTS invoices (
    id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id            UUID REFERENCES users(id) ON DELETE CASCADE,
    org_id             UUID REFERENCES organizations(id) ON DELETE CASCADE,
    stripe_invoice_id  TEXT NOT NULL UNIQUE,
    number             TEXT,
    issued_at          TIMESTAMPTZ NOT NULL,
    period_start       TIMESTAMPTZ,
    period_end         TIMESTAMPTZ,
    subtotal_cents     BIGINT NOT NULL DEFAULT 0,
    tax_cents          BIGINT NOT NULL DEFAULT 0,
    total_cents        BIGINT NOT NULL DEFAULT 0,
    currency           TEXT NOT NULL DEFAULT 'usd',
    status             TEXT NOT NULL DEFAULT 'open',
    hosted_invoice_url TEXT,
    invoice_pdf_url    TEXT,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT invoices_owner_exactly_one
        CHECK ((user_id IS NULL) <> (org_id IS NULL))
);

-- Both list endpoints read "everything for this owner, newest first" — the index
-- carries `issued_at DESC` so the monthly grouping never sorts in memory.
CREATE INDEX IF NOT EXISTS idx_invoices_user ON invoices (user_id, issued_at DESC);
CREATE INDEX IF NOT EXISTS idx_invoices_org  ON invoices (org_id,  issued_at DESC);

-- Consumers had no Stripe customer at all (Checkout created a guest customer and
-- threw it away). Without one there is no invoice history to attach, so persist it
-- the first time we see it.
ALTER TABLE users ADD COLUMN IF NOT EXISTS stripe_customer_id TEXT;
CREATE INDEX IF NOT EXISTS idx_users_stripe_customer ON users (stripe_customer_id);

-- Financial + PII: default-deny for the PostgREST roles, same as every other
-- billing table (migration 011). The API connects as the table owner, which
-- `ENABLE` (not `FORCE`) exempts, so the server is unaffected.
ALTER TABLE invoices ENABLE ROW LEVEL SECURITY;
