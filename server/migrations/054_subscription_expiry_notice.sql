-- Expiring-subscription warning (org billing).
--
-- Marks WHICH period a warning was already sent for, rather than a plain
-- "notified" flag. A renewal moves `current_period_end` forward, so the stored
-- value stops matching and the next period is warned about on its own — no
-- write site has to remember to clear anything.
ALTER TABLE organizations
    ADD COLUMN IF NOT EXISTS subscription_expiry_notified_for TIMESTAMPTZ;

-- The sweep looks for live subscriptions inside the lead window; keep it off a
-- full scan as the org count grows.
CREATE INDEX IF NOT EXISTS idx_orgs_subscription_period_end
    ON organizations (current_period_end)
    WHERE subscription_status = 'active';
