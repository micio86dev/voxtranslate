-- 060 — the organisation's address book (spec 0114).
--
-- Dialling meant typing a number and choosing the recipient's language out of eighty-four
-- EVERY time, for the same person. Nothing remembered that the number ending 8000 belongs
-- to Wei in Shenzhen and that Wei speaks Mandarin — and getting it wrong places a call,
-- bills it, and leaves two people unable to understand each other.

CREATE TABLE IF NOT EXISTS voip_contacts (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    -- Org-owned, not project-owned. A person does not belong to a project; they turn up
    -- in several, which is what voip_contact_projects is for.
    org_id      UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    company     TEXT,
    role        TEXT,
    notes       TEXT,
    tags        TEXT[] NOT NULL DEFAULT '{}',
    email       TEXT,
    -- Keep the contact if the colleague who added them leaves, the same way `projects`
    -- keeps a project when its creator's personal account is deleted.
    created_by  UUID REFERENCES users(id) ON DELETE SET NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_voip_contacts_org ON voip_contacts (org_id, name);

CREATE TABLE IF NOT EXISTS voip_contact_numbers (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    contact_id  UUID NOT NULL REFERENCES voip_contacts(id) ON DELETE CASCADE,
    -- Denormalised from the contact, and it earns its keep twice: it carries the UNIQUE
    -- constraint below, which Postgres will not enforce across a join, and it answers
    -- inbound's only question — "who is ringing this organisation?" — in one statement
    -- rather than a join, on the path where latency is a ringing telephone (spec 0116).
    org_id      UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    e164        TEXT NOT NULL,
    label       TEXT,
    -- The language THIS number speaks, not the person's. A colleague in Barcelona who
    -- takes work calls in English on the office line and Catalan on their mobile is not
    -- two people; putting the language on the contact forces a choice that is wrong half
    -- the time.
    language    TEXT,
    country     TEXT,
    is_primary  BOOLEAN NOT NULL DEFAULT FALSE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- One number, one person, per organisation. Inbound must not have to CHOOSE whose call
-- this is. Scoped to the org rather than global: two customers may legitimately both know
-- the same supplier.
CREATE UNIQUE INDEX IF NOT EXISTS idx_voip_contact_numbers_unique
    ON voip_contact_numbers (org_id, e164);
-- The reverse lookup inbound will make on every incoming call.
CREATE INDEX IF NOT EXISTS idx_voip_contact_numbers_contact
    ON voip_contact_numbers (contact_id);
-- At most one primary per contact. A partial unique index rather than application logic,
-- because two concurrent "make this the primary" requests would otherwise both win — the
-- same reasoning as `idx_voip_numbers_one_default`.
CREATE UNIQUE INDEX IF NOT EXISTS idx_voip_contact_numbers_one_primary
    ON voip_contact_numbers (contact_id) WHERE is_primary;

-- The first many-to-many involving `projects` in this schema. Every other reference is a
-- 1:N `project_id` column, because everything else so far has belonged to exactly one
-- project. A person does not.
CREATE TABLE IF NOT EXISTS voip_contact_projects (
    contact_id  UUID NOT NULL REFERENCES voip_contacts(id) ON DELETE CASCADE,
    project_id  UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (contact_id, project_id)
);
-- The composite PK already indexes (contact_id, project_id); this is the other direction,
-- for "show me everyone on this project".
CREATE INDEX IF NOT EXISTS idx_voip_contact_projects_project
    ON voip_contact_projects (project_id);

-- Which contact a call was placed to, when one was known.
--
-- ON DELETE SET NULL, not CASCADE: a call that happened cannot un-happen. Removing someone
-- from the address book must not remove the record of having called them or what it cost —
-- the rule `organization_credits_transactions` already follows for `session_id`.
ALTER TABLE voip_calls
    ADD COLUMN IF NOT EXISTS contact_id UUID REFERENCES voip_contacts(id) ON DELETE SET NULL;
CREATE INDEX IF NOT EXISTS idx_voip_calls_contact
    ON voip_calls (contact_id) WHERE contact_id IS NOT NULL;

ALTER TABLE voip_contacts          ENABLE ROW LEVEL SECURITY;
ALTER TABLE voip_contact_numbers   ENABLE ROW LEVEL SECURITY;
ALTER TABLE voip_contact_projects  ENABLE ROW LEVEL SECURITY;

-- Guarded REVOKE (anon/authenticated exist only on Supabase, not local/CI Postgres).
DO $$
DECLARE
    t TEXT;
BEGIN
    FOREACH t IN ARRAY ARRAY[
        'voip_contacts', 'voip_contact_numbers', 'voip_contact_projects'
    ] LOOP
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
            EXECUTE format('REVOKE ALL ON %I FROM anon', t);
        END IF;
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
            EXECUTE format('REVOKE ALL ON %I FROM authenticated', t);
        END IF;
    END LOOP;
END $$;
