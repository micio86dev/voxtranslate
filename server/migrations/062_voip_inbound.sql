-- 062 — what happens when somebody calls you (spec 0116).
--
-- Spec 0111 named inbound a non-goal in its first milestone: "The abstraction admits them;
-- the orchestration does not yet." This is the orchestration's schema. Until now
-- `voip_numbers.inbound_enabled` had never been queried by anything, and
-- `TelephonyProvider::answer()` had zero callers anywhere in the server.

CREATE TABLE IF NOT EXISTS voip_number_routing (
    -- One row per number, and the number owns it: releasing a number takes its routing
    -- with it, because routing for a number you no longer have is a trap.
    number_id    UUID PRIMARY KEY REFERENCES voip_numbers(id) ON DELETE CASCADE,
    org_id       UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,

    -- Who to ring. `owners` is the default and the fallback: a call nobody is told about is
    -- worse than a call the wrong person takes, and an organisation that has not configured
    -- routing has still bought a number and given it to somebody.
    ring_mode    TEXT NOT NULL DEFAULT 'owners'
                   CHECK (ring_mode IN ('owners', 'users', 'team')),
    ring_user_ids UUID[] NOT NULL DEFAULT '{}',
    -- SET NULL rather than CASCADE: deleting a team must not delete the number's routing
    -- and leave the number silently unreachable. It falls back to the owners instead.
    ring_team_id UUID REFERENCES teams(id) ON DELETE SET NULL,

    -- Long enough not to clip somebody walking to their desk, short enough not to feel
    -- broken.
    ring_seconds INTEGER NOT NULL DEFAULT 25 CHECK (ring_seconds BETWEEN 5 AND 120),

    -- What happens when nobody answers. Never "keep ringing": a caller deserves an answer,
    -- and an unanswered call that rings for ever is the worst of the three.
    no_answer_action TEXT NOT NULL DEFAULT 'voicemail'
                   CHECK (no_answer_action IN ('voicemail', 'forward', 'refuse')),
    -- Where a forward goes. A second billable leg, priced and gated like any outbound call:
    -- a forward must not become a way around the organisation's spend caps.
    forward_to   TEXT,

    -- What language to answer a caller the address book does not know. A contact's own
    -- number carries its own language (spec 0114) and wins over this.
    stranger_language TEXT,

    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_voip_number_routing_org ON voip_number_routing (org_id);

ALTER TABLE voip_calls
    -- Which of our numbers was rung. Null for outbound, where the caller id is already
    -- recorded in `caller_e164`.
    ADD COLUMN IF NOT EXISTS inbound_number_id UUID REFERENCES voip_numbers(id) ON DELETE SET NULL,
    -- Who picked it up. Null while ringing, and null for ever on a missed call — which is
    -- what makes "missed" a fact rather than an inference.
    ADD COLUMN IF NOT EXISTS answered_by UUID REFERENCES users(id) ON DELETE SET NULL,
    -- Who was rung, so a missed call can say who it was missed by.
    ADD COLUMN IF NOT EXISTS rang_user_ids UUID[] NOT NULL DEFAULT '{}',
    -- When to stop waiting. Only a clock can notice an absence — the same reasoning as
    -- `fail_stalled_calls`.
    ADD COLUMN IF NOT EXISTS ring_deadline_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS missed BOOLEAN NOT NULL DEFAULT FALSE;

-- The ring-timeout sweep's query.
CREATE INDEX IF NOT EXISTS idx_voip_calls_ringing
    ON voip_calls (ring_deadline_at)
    WHERE direction = 'inbound' AND ring_deadline_at IS NOT NULL AND answered_by IS NULL;

ALTER TABLE voip_number_routing ENABLE ROW LEVEL SECURITY;
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON voip_number_routing FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON voip_number_routing FROM authenticated;
    END IF;
END $$;
