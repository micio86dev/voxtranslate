-- 019 — Attribute org credit spend to the member who triggered it (spec 0106,
-- Phase-3 per-member analytics).
--
-- `actor_id` is the user who caused a ledger row (started a recording, ran a
-- transcript translation, …). Nullable: system/background charges and all
-- historical rows have no actor (backfilled NULL). ON DELETE SET NULL keeps the
-- financial record when the member's account is erased (GDPR), dropping only the
-- PII pointer — same policy as audit_logs.actor_id.

ALTER TABLE organization_credits_transactions
    ADD COLUMN IF NOT EXISTS actor_id UUID REFERENCES users(id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS idx_org_credits_tx_actor
    ON organization_credits_transactions (org_id, actor_id);
