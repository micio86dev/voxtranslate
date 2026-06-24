-- 016 — VoxTranslate for Business: org workspace, projects, recordings, transcripts.
--
-- This is the persistence layer for the Business tier (spec 0106). It layers a
-- multi-tenant org model on top of the existing consumer schema WITHOUT touching
-- it: every consumer table, the credit ledger, and the call/auth flow are
-- untouched. New `call_sessions` columns are nullable / defaulted, so existing
-- rows and the consumer flow are unaffected (the columns simply stay NULL/'none'
-- for non-business calls).
--
-- Stack reconciliation (the spec was drafted Supabase-shaped; this repo is not):
--   * FKs target the app's own `users(id)` — there is no Supabase `auth.users`.
--   * Org credits are a SEPARATE INTEGER currency from the consumer DECIMAL(10,6)
--     ledger; the two never mix (see CLAUDE/spec: org vs consumer credits).
--   * There is no `rooms` table — rooms are ephemeral/in-memory. The persistent
--     call record is `call_sessions` (migration 004), so the spec's "rooms"
--     additions land there and the spec's `room_id` FK becomes `session_id`.
--   * Authorization is enforced in the Rust API layer (AuthUser + role checks),
--     NOT via per-user DB RLS. RLS here is default-deny defense-in-depth only,
--     mirroring 011_rls_lockdown.sql: the server connects as the table-owning role
--     (exempt from RLS), so ENABLE-without-policy locks out PostgREST anon/auth.
--   * `updated_at` is bumped by app code (`SET updated_at = now()`), as everywhere
--     else in this schema — there are no triggers.

-- gen_random_bytes() (invite tokens) lives in pgcrypto. It's pre-installed on
-- Supabase and in the CI/Docker Postgres image, so IF NOT EXISTS is a no-op. (If a
-- restricted role ever can't create it, mint the token in Rust instead.)
CREATE EXTENSION IF NOT EXISTS pgcrypto;

-- ---- Organizations -----------------------------------------------------------
-- One row per workspace. `slug` is the future subdomain (acme.voxtranslate.app).
-- `credits_balance` is the org's INTEGER credit pool; `settings` holds retention /
-- compliance / domain-allowlist / recording-notice config with sane defaults.
CREATE TABLE IF NOT EXISTS organizations (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name            TEXT NOT NULL,
    slug            TEXT UNIQUE NOT NULL,
    plan            TEXT NOT NULL DEFAULT 'business',     -- 'business' | 'enterprise'
    credits_balance INTEGER NOT NULL DEFAULT 0,
    settings        JSONB NOT NULL DEFAULT
        '{"retention_days":90,"compliance_mode":false,"allowed_domains":[],"recording_notify_participants":true}'::jsonb,
    -- Deleting the owner's account CASCADE-deletes the org (and, transitively, all
    -- its data below). Phase 2 must require ownership transfer before account
    -- deletion, so this fires only as a GDPR last resort, never in normal use.
    owner_id        UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ---- Members -----------------------------------------------------------------
-- A user's role within an org. UNIQUE(org_id, user_id) = one membership per pair.
-- ON DELETE CASCADE on both FKs erases the row when the org or the account goes.
CREATE TABLE IF NOT EXISTS organization_members (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id     UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id    UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role       TEXT NOT NULL DEFAULT 'member',            -- 'owner'|'admin'|'member'|'guest'
    invited_by UUID REFERENCES users(id) ON DELETE SET NULL,
    joined_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (org_id, user_id)
);
CREATE INDEX IF NOT EXISTS idx_org_members_user ON organization_members (user_id);
CREATE INDEX IF NOT EXISTS idx_org_members_org  ON organization_members (org_id);

-- ---- Invites -----------------------------------------------------------------
-- Pending email invites. `token` is a secret used in /join?token=… links; it
-- defaults to 32 random bytes hex-encoded. `expires_at` defaults to 48h out;
-- `accepted_at` is set when redeemed.
CREATE TABLE IF NOT EXISTS organization_invites (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id      UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    email       TEXT NOT NULL,
    role        TEXT NOT NULL DEFAULT 'member',
    token       TEXT UNIQUE NOT NULL DEFAULT encode(gen_random_bytes(32), 'hex'),
    invited_by  UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at  TIMESTAMPTZ NOT NULL DEFAULT now() + interval '48 hours',
    accepted_at TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_org_invites_org   ON organization_invites (org_id);
CREATE INDEX IF NOT EXISTS idx_org_invites_email ON organization_invites (email);

-- ---- Projects ----------------------------------------------------------------
-- Groups calls within an org. Soft-deleted via `archived_at` (never hard-deleted
-- here, so historic calls keep a valid project FK).
CREATE TABLE IF NOT EXISTS projects (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id            UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    name              TEXT NOT NULL,
    description       TEXT,
    default_languages TEXT[] NOT NULL DEFAULT '{}',
    -- Org-owned: keep the project if the creator's personal account is deleted.
    created_by        UUID REFERENCES users(id) ON DELETE SET NULL,
    archived_at       TIMESTAMPTZ,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_projects_org ON projects (org_id);

-- ---- Business associations on the persistent call record ---------------------
-- The spec's "ALTER TABLE rooms …" lands on `call_sessions` (the only persistent
-- call entity). All columns default so consumer calls are unaffected. ON DELETE
-- SET NULL: deleting an org/project reverts its calls to unaffiliated (consumer)
-- records rather than destroying shared call history.
ALTER TABLE call_sessions ADD COLUMN IF NOT EXISTS org_id                  UUID REFERENCES organizations(id) ON DELETE SET NULL;
ALTER TABLE call_sessions ADD COLUMN IF NOT EXISTS project_id              UUID REFERENCES projects(id) ON DELETE SET NULL;
ALTER TABLE call_sessions ADD COLUMN IF NOT EXISTS cloud_recording_enabled BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE call_sessions ADD COLUMN IF NOT EXISTS recording_storage_path  TEXT;
ALTER TABLE call_sessions ADD COLUMN IF NOT EXISTS transcript_status       TEXT NOT NULL DEFAULT 'none';
                                            -- 'none' | 'processing' | 'ready' | 'failed'
CREATE INDEX IF NOT EXISTS idx_call_sessions_org_started ON call_sessions (org_id, started_at DESC);
CREATE INDEX IF NOT EXISTS idx_call_sessions_project     ON call_sessions (project_id);

-- ---- Pre-call binding --------------------------------------------------------
-- Maps a room code to an org/project + recording intent BEFORE a call exists.
-- RoomManager reads this when it materializes a `call_session` so the call inherits
-- the org/project/recording flag. Mirrors `room_glossaries` (room TEXT PK).
CREATE TABLE IF NOT EXISTS room_business_bindings (
    room                    TEXT PRIMARY KEY,
    org_id                  UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    project_id              UUID REFERENCES projects(id) ON DELETE SET NULL,
    cloud_recording_enabled BOOLEAN NOT NULL DEFAULT FALSE,
    created_by              UUID REFERENCES users(id) ON DELETE SET NULL,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ---- Transcripts -------------------------------------------------------------
-- Post-call diarized transcript produced from the cloud recording (Deepgram
-- diarize=true). Distinct from realtime `transcript_events` (migration 004).
-- `session_id` is the spec's "room_id" → the persistent call record.
-- `segments` is the diarized text; `translations` is an on-demand language cache.
CREATE TABLE IF NOT EXISTS transcripts (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id       UUID NOT NULL REFERENCES call_sessions(id) ON DELETE CASCADE,
    org_id           UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    source_language  TEXT NOT NULL,
    segments         JSONB NOT NULL DEFAULT '[]'::jsonb,    -- [{speaker_id,speaker_name,text,start_ms,end_ms}]
    translations     JSONB NOT NULL DEFAULT '{}'::jsonb,    -- {"it":"…","de":"…"}  cache
    duration_seconds INTEGER,
    word_count       INTEGER,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    processed_at     TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS idx_transcripts_session ON transcripts (session_id);
CREATE INDEX IF NOT EXISTS idx_transcripts_org     ON transcripts (org_id);

-- ---- Compliance audit trail --------------------------------------------------
-- Written only when an org has compliance_mode = true. Inserted exclusively by
-- the backend (service role); admins/owners read it via the API.
CREATE TABLE IF NOT EXISTS audit_logs (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id        UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- Keep the compliance event but drop the PII pointer on GDPR account erasure.
    actor_id      UUID REFERENCES users(id) ON DELETE SET NULL,
    action        TEXT NOT NULL,        -- 'transcript.view'|'transcript.export'|'recording.play'|'member.invite'|…
    resource_type TEXT NOT NULL,
    resource_id   UUID NOT NULL,
    metadata      JSONB NOT NULL DEFAULT '{}'::jsonb,
    ip_address    INET,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_audit_logs_org_created ON audit_logs (org_id, created_at DESC);

-- ---- Org credit ledger -------------------------------------------------------
-- Immutable ledger for the org's INTEGER credit pool (separate currency from the
-- consumer DECIMAL ledger). `amount` is signed (+purchase, -consumption).
-- `session_id` is the spec's "room_id".
CREATE TABLE IF NOT EXISTS organization_credits_transactions (
    id                       UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id                   UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    amount                   INTEGER NOT NULL,    -- +purchase, -consumption
    type                     TEXT NOT NULL,       -- 'purchase'|'recording'|'transcription'|'translation'|'subscription_grant'
    description              TEXT,
    -- Preserve the financial record when a call is purged; just null the link.
    session_id               UUID REFERENCES call_sessions(id) ON DELETE SET NULL,
    stripe_payment_intent_id TEXT,
    created_at               TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_org_credits_tx_org_created
    ON organization_credits_transactions (org_id, created_at DESC);

-- ---- Role helper -------------------------------------------------------------
-- Single source of truth for "what role does this user have in this org" — called
-- by the Rust authorization guards. Returns NULL when the user is not a member.
-- STABLE (not SECURITY DEFINER): the server already runs as the owning role.
CREATE OR REPLACE FUNCTION get_user_org_role(p_org_id UUID, p_user_id UUID)
RETURNS TEXT AS $$
  SELECT role FROM organization_members WHERE org_id = p_org_id AND user_id = p_user_id
$$ LANGUAGE sql STABLE;

-- ---- RLS: default-deny defense-in-depth (mirrors 011_rls_lockdown.sql) --------
-- No permissive policy: the owner role the server uses is exempt; with RLS on and
-- no policy, PostgREST anon/authenticated can read/write NOTHING. Authorization
-- for real requests is enforced in the Rust API layer.
ALTER TABLE organizations                     ENABLE ROW LEVEL SECURITY;
ALTER TABLE organization_members              ENABLE ROW LEVEL SECURITY;
ALTER TABLE organization_invites              ENABLE ROW LEVEL SECURITY;
ALTER TABLE projects                          ENABLE ROW LEVEL SECURITY;
ALTER TABLE room_business_bindings            ENABLE ROW LEVEL SECURITY;
ALTER TABLE transcripts                       ENABLE ROW LEVEL SECURITY;
ALTER TABLE audit_logs                        ENABLE ROW LEVEL SECURITY;
ALTER TABLE organization_credits_transactions ENABLE ROW LEVEL SECURITY;

-- Re-apply the guarded REVOKE for these new tables (011 ran before they existed).
-- Guarded because anon/authenticated only exist on Supabase, not local/CI Postgres.
DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
    REVOKE ALL ON ALL TABLES IN SCHEMA public FROM anon;
    REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM anon;
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
    REVOKE ALL ON ALL TABLES IN SCHEMA public FROM authenticated;
    REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM authenticated;
  END IF;
END $$;
