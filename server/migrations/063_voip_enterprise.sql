-- 063 — office hours and a menu (spec 0118).
--
-- 0116 made a number ring somebody. Every Enterprise customer's next two questions are
-- what happens outside office hours, and whether a caller can choose a department.

CREATE TABLE IF NOT EXISTS voip_business_hours (
    number_id  UUID PRIMARY KEY REFERENCES voip_numbers(id) ON DELETE CASCADE,
    org_id     UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- IANA, e.g. 'Europe/Rome'. The organisation's, never the server's and never the
    -- caller's: a call at 09:00 in Milan is inside Milan office hours wherever our
    -- process happens to be running.
    timezone   TEXT NOT NULL DEFAULT 'UTC',
    -- Seven windows, Monday first, as MINUTES FROM MIDNIGHT. Minutes rather than `TIME`
    -- because every bug this kind of feature has is arithmetic across a boundary, and
    -- integers do not have the rest of `TIME`'s opinions. NULL = closed all day.
    opens_at   INTEGER[] NOT NULL DEFAULT '{}',
    closes_at  INTEGER[] NOT NULL DEFAULT '{}',
    -- What happens when the office is shut. Same vocabulary as the no-answer action, so
    -- an admin learns one set of words rather than two.
    closed_action TEXT NOT NULL DEFAULT 'voicemail'
                    CHECK (closed_action IN ('voicemail', 'forward', 'refuse')),
    closed_forward_to TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_voip_business_hours_org ON voip_business_hours (org_id);

CREATE TABLE IF NOT EXISTS voip_ivr_options (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    number_id  UUID NOT NULL REFERENCES voip_numbers(id) ON DELETE CASCADE,
    org_id     UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- A single key. One level only: nested menus are where IVRs become the thing
    -- customers hate, and the second level can wait until somebody asks for it.
    digit      TEXT NOT NULL CHECK (digit ~ '^[0-9*#]$'),
    label      TEXT NOT NULL,
    -- The same ring-target shape `voip_number_routing` uses, so an option routes by
    -- exactly the rules a number does rather than by a second implementation that drifts.
    ring_mode  TEXT NOT NULL DEFAULT 'owners'
                 CHECK (ring_mode IN ('owners', 'users', 'team')),
    ring_user_ids UUID[] NOT NULL DEFAULT '{}',
    ring_team_id  UUID REFERENCES teams(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- One meaning per key. Two options on "1" is a menu whose behaviour depends on row order.
CREATE UNIQUE INDEX IF NOT EXISTS idx_voip_ivr_options_key
    ON voip_ivr_options (number_id, digit);

ALTER TABLE voip_business_hours ENABLE ROW LEVEL SECURITY;
ALTER TABLE voip_ivr_options    ENABLE ROW LEVEL SECURITY;
DO $$
DECLARE
    t TEXT;
BEGIN
    FOREACH t IN ARRAY ARRAY['voip_business_hours', 'voip_ivr_options'] LOOP
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
            EXECUTE format('REVOKE ALL ON %I FROM anon', t);
        END IF;
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
            EXECUTE format('REVOKE ALL ON %I FROM authenticated', t);
        END IF;
    END LOOP;
END $$;
