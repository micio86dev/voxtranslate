-- 064 — self-service regulatory requirements for a purchased number (spec 0119).
--
-- A number bought with a regulatory requirement has stayed `pending_regulatory`
-- forever: nothing submits requirements, uploads documents, or re-reads order status.
-- This is the schema that self-service, and the reconcile sweep behind it, need.

ALTER TABLE voip_numbers
    -- Telnyx's own handles on the purchase, persisted at buy time so discovery,
    -- submission and the sweep can all address the same sub-order without re-deriving
    -- it from anything guessed.
    ADD COLUMN IF NOT EXISTS provider_order_id TEXT,
    ADD COLUMN IF NOT EXISTS provider_sub_order_id TEXT,
    -- What kind of number this is, in the vocabulary `NumberKind` already uses — needed
    -- to key a requirement group by country + number kind + action.
    ADD COLUMN IF NOT EXISTS number_kind TEXT,
    ADD COLUMN IF NOT EXISTS requirement_group_id UUID,
    -- When the sweep should next read this number's provider status. NULL means "not on
    -- the sweep's schedule" (nothing regulatory in flight, or the row predates this
    -- feature and has no `provider_sub_order_id` to poll).
    ADD COLUMN IF NOT EXISTS regulatory_next_check_at TIMESTAMPTZ,
    -- Consecutive sweep failures for this row, driving the backoff — never the number of
    -- rejections, which stays in `status_reason`.
    ADD COLUMN IF NOT EXISTS regulatory_failures INT NOT NULL DEFAULT 0;

-- One requirement group per org + country + number kind + action, so an approved group
-- is reused instead of asking the same paperwork twice for the same combination.
CREATE TABLE IF NOT EXISTS voip_requirement_groups (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id            UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    provider          TEXT NOT NULL,
    country           TEXT NOT NULL,
    number_kind       TEXT NOT NULL,
    action            TEXT NOT NULL DEFAULT 'ordering',
    -- NULL while a claim row is being created (D7) — the winner of the race fills this
    -- in once the carrier has actually answered.
    provider_group_id TEXT,
    -- 'creating' is our own claim state, never a Telnyx word; everything else mirrors
    -- what the carrier reports.
    status            TEXT NOT NULL DEFAULT 'creating',
    status_reason     TEXT,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The reuse key (D7/D8). Partial: an expired or declined group must not block a fresh
-- one for the same combination from being created.
CREATE UNIQUE INDEX IF NOT EXISTS idx_voip_requirement_groups_combo
    ON voip_requirement_groups (org_id, provider, country, number_kind, action)
    WHERE status NOT IN ('expired', 'no_longer_eligible');

-- The provider's own id, once it exists, must not be attached to two of our rows.
CREATE UNIQUE INDEX IF NOT EXISTS idx_voip_requirement_groups_provider_id
    ON voip_requirement_groups (provider, provider_group_id)
    WHERE provider_group_id IS NOT NULL;

-- Only what a document IS, never its bytes: `document_id` and `av_scan_status` are the
-- only things Telnyx returns that this product is allowed to keep (spec 0119, D12).
CREATE TABLE IF NOT EXISTS voip_requirement_documents (
    id                   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    group_id             UUID NOT NULL REFERENCES voip_requirement_groups(id) ON DELETE CASCADE,
    org_id               UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    requirement_id       TEXT NOT NULL,
    provider_document_id TEXT,
    av_scan_status       TEXT,
    content_type         TEXT,
    size_bytes           INT,
    -- Keep the record if the uploader's personal account is deleted later, the same rule
    -- `voip_contacts.created_by` already follows.
    uploaded_by          UUID REFERENCES users(id) ON DELETE SET NULL,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (group_id, requirement_id)
);

-- The sweep's query: everything due for a status re-read, oldest first. Scoped to rows
-- that actually have a sub-order to poll, so a legacy `pending_regulatory` row (no
-- `provider_sub_order_id`) is never picked up and mistaken for progress.
CREATE INDEX IF NOT EXISTS idx_voip_numbers_regulatory
    ON voip_numbers (regulatory_next_check_at)
    WHERE status IN ('pending_regulatory', 'regulatory_review', 'regulatory_rejected')
      AND provider_sub_order_id IS NOT NULL;

-- ---------------------------------------------------------------------------------------
-- Constraints, all guarded and NOT VALID (D3): every guard here protects the same thing —
-- an `IF NOT EXISTS` on a column add is silently satisfied by an already-migrated
-- database, and a constraint riding on that same silent no-op would never be created at
-- all. NOT VALID applies each constraint to every INSERT and UPDATE from this moment,
-- which is the protection that matters, without failing the migration — and the server's
-- boot with it — over a row that drifted in earlier.
-- ---------------------------------------------------------------------------------------

-- The D3 swap: two new resubmittable statuses join the vocabulary. The v1 constraint
-- already exists in every deployed database, so guarding the OLD name would silently
-- skip the new values and every INSERT of `regulatory_review`/`regulatory_rejected`
-- would fail forever.
DO $$
BEGIN
    IF to_regclass('public.voip_numbers') IS NOT NULL THEN
        ALTER TABLE voip_numbers DROP CONSTRAINT IF EXISTS voip_numbers_status_check;
        IF NOT EXISTS (
            SELECT 1 FROM pg_constraint
             WHERE conname = 'voip_numbers_status_check_v2'
               AND conrelid = to_regclass('public.voip_numbers')
        ) THEN
            ALTER TABLE voip_numbers
                ADD CONSTRAINT voip_numbers_status_check_v2
                CHECK (status IN ('ordering', 'pending_regulatory', 'regulatory_review',
                                  'regulatory_rejected', 'active', 'suspended',
                                  'releasing', 'released', 'failed')) NOT VALID;
        END IF;
    END IF;
END
$$;

DO $$
BEGIN
    IF to_regclass('public.voip_numbers') IS NOT NULL
       AND to_regclass('public.voip_requirement_groups') IS NOT NULL
       AND NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'voip_numbers_requirement_group_fk'
           AND conrelid = to_regclass('public.voip_numbers')
    ) THEN
        ALTER TABLE voip_numbers
            ADD CONSTRAINT voip_numbers_requirement_group_fk
            FOREIGN KEY (requirement_group_id) REFERENCES voip_requirement_groups(id)
            ON DELETE SET NULL NOT VALID;
    END IF;
END
$$;

DO $$
BEGIN
    IF to_regclass('public.voip_requirement_groups') IS NOT NULL AND NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'voip_requirement_groups_status_check'
           AND conrelid = to_regclass('public.voip_requirement_groups')
    ) THEN
        ALTER TABLE voip_requirement_groups
            ADD CONSTRAINT voip_requirement_groups_status_check
            CHECK (status IN ('creating', 'unapproved', 'pending_approval', 'approved',
                              'declined', 'expired', 'no_longer_eligible', 'unknown'))
            NOT VALID;
    END IF;
END
$$;

DO $$
BEGIN
    IF to_regclass('public.voip_requirement_groups') IS NOT NULL AND NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'voip_requirement_groups_action_check'
           AND conrelid = to_regclass('public.voip_requirement_groups')
    ) THEN
        ALTER TABLE voip_requirement_groups
            ADD CONSTRAINT voip_requirement_groups_action_check
            CHECK (action IN ('ordering')) NOT VALID;
    END IF;
END
$$;

DO $$
BEGIN
    IF to_regclass('public.voip_requirement_groups') IS NOT NULL AND NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'voip_requirement_groups_kind_check'
           AND conrelid = to_regclass('public.voip_requirement_groups')
    ) THEN
        ALTER TABLE voip_requirement_groups
            ADD CONSTRAINT voip_requirement_groups_kind_check
            CHECK (number_kind IN ('local', 'national', 'toll_free', 'mobile')) NOT VALID;
    END IF;
END
$$;

DO $$
BEGIN
    IF to_regclass('public.voip_requirement_documents') IS NOT NULL AND NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'voip_requirement_documents_scan_check'
           AND conrelid = to_regclass('public.voip_requirement_documents')
    ) THEN
        ALTER TABLE voip_requirement_documents
            ADD CONSTRAINT voip_requirement_documents_scan_check
            CHECK (av_scan_status IS NULL
                   OR av_scan_status IN ('pending', 'passed', 'failed')) NOT VALID;
    END IF;
END
$$;

ALTER TABLE voip_requirement_groups    ENABLE ROW LEVEL SECURITY;
ALTER TABLE voip_requirement_documents ENABLE ROW LEVEL SECURITY;
DO $$
DECLARE
    t TEXT;
BEGIN
    FOREACH t IN ARRAY ARRAY[
        'voip_requirement_groups', 'voip_requirement_documents'
    ] LOOP
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
            EXECUTE format('REVOKE ALL ON %I FROM anon', t);
        END IF;
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
            EXECUTE format('REVOKE ALL ON %I FROM authenticated', t);
        END IF;
    END LOOP;
END $$;
