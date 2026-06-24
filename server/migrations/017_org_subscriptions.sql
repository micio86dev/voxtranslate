-- 017 — Org subscription state (spec 0106, Phase 2 PR-C).
--
-- Auto-renewing Stripe subscriptions for the Business tier. All columns are
-- additive and nullable / defaulted, so existing orgs and the consumer flow are
-- unaffected. Subscriptions are driven by Stripe Checkout (mode=subscription) and
-- the Billing Customer Portal; these columns mirror the live Stripe state, kept in
-- sync by the org webhook (`/api/business/stripe/webhook`).

ALTER TABLE organizations ADD COLUMN IF NOT EXISTS stripe_customer_id     TEXT;
ALTER TABLE organizations ADD COLUMN IF NOT EXISTS stripe_subscription_id TEXT;
ALTER TABLE organizations ADD COLUMN IF NOT EXISTS subscription_status    TEXT NOT NULL DEFAULT 'none';
                                            -- 'none' | 'active' | 'past_due' | 'canceled'
ALTER TABLE organizations ADD COLUMN IF NOT EXISTS subscription_interval  TEXT;
                                            -- 'month' | 'year'
ALTER TABLE organizations ADD COLUMN IF NOT EXISTS current_period_end     TIMESTAMPTZ;
ALTER TABLE organizations ADD COLUMN IF NOT EXISTS cancel_at_period_end   BOOLEAN NOT NULL DEFAULT FALSE;

CREATE INDEX IF NOT EXISTS idx_org_stripe_customer
    ON organizations (stripe_customer_id);
CREATE INDEX IF NOT EXISTS idx_org_stripe_subscription
    ON organizations (stripe_subscription_id);
