-- Live Connect (spec 0121): two phones, two mics, two independent translation streams.
--
-- A live session is a thin lifecycle/billing row (`live_sessions`) linked to an ordinary
-- `call_sessions` row tagged `kind = 'live_connect'`. Everything conversational (speech
-- events, participants, transcripts, embeddings, search, insights) therefore reuses the
-- existing meeting tables untouched — this migration adds only what meetings lack:
--
--   live_desks           permanent QR entry points (one per service desk / location)
--   live_sessions        lifecycle, ownership (user XOR org), project + desk binding
--   live_participants    who is on the call and how they authenticate (max 2 per session)
--   live_usage_ticks     idempotency ledger: one debit per (session, peer, tick)
--   live_session_events  bounded diagnostics for support / Directus
--
-- Every statement is idempotent (CLAUDE.md): CREATE ... IF NOT EXISTS, and each CHECK is
-- its own guarded, NOT VALID constraint so a drifted row can never fail the boot.
-- `call_sessions.kind` already exists (032, no CHECK) so 'live_connect' needs no change.

-- ---------------------------------------------------------------------------
-- Permanent QR entry points. The QR itself carries only `public_id` plus an HMAC of
-- (desk id, token_version); the secret key lives in env, nothing secret is stored here.
-- Revoke = status flag. Regenerate = token_version + 1 (the old printed QR stops
-- verifying). Neither depends on any session, so a QR never "is" a conversation.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS live_desks (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id         UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    project_id     UUID REFERENCES projects(id) ON DELETE SET NULL,
    name           TEXT NOT NULL,
    location_label TEXT,
    default_lang   TEXT NOT NULL DEFAULT 'en',
    languages      TEXT[] NOT NULL DEFAULT '{}',
    public_id      TEXT NOT NULL,
    token_version  INTEGER NOT NULL DEFAULT 1,
    status         TEXT NOT NULL DEFAULT 'active',            -- 'active' | 'revoked'
    created_by     UUID REFERENCES users(id) ON DELETE SET NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at     TIMESTAMPTZ
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_live_desks_public_id ON live_desks (public_id);
CREATE INDEX IF NOT EXISTS idx_live_desks_org ON live_desks (org_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_live_desks_project ON live_desks (project_id) WHERE project_id IS NOT NULL;

-- ---------------------------------------------------------------------------
-- One row per Live Connect conversation. Ownership is a user (B2C) or an org (B2B);
-- the org pays through its existing wallet, the user through the personal balance.
-- `call_session_id` links the meeting-side row; SET NULL so retention can purge the
-- conversation without erasing the billing/usage record.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS live_sessions (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    call_session_id UUID REFERENCES call_sessions(id) ON DELETE SET NULL,
    owner_user_id   UUID REFERENCES users(id) ON DELETE CASCADE,
    org_id          UUID REFERENCES organizations(id) ON DELETE CASCADE,
    project_id      UUID REFERENCES projects(id) ON DELETE SET NULL,
    desk_id         UUID REFERENCES live_desks(id) ON DELETE SET NULL,
    status          TEXT NOT NULL DEFAULT 'waiting',          -- 'waiting' | 'active' | 'ended' | 'expired'
    invite_hash     BYTEA,                                    -- SHA-256 of the invite code (B2C); NULL for desk scans
    host_lang       TEXT NOT NULL,
    save_transcript BOOLEAN NOT NULL DEFAULT FALSE,           -- B2C opt-in; B2B follows org retention
    engine_id       TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at      TIMESTAMPTZ NOT NULL,                     -- join/answer window while 'waiting'
    started_at      TIMESTAMPTZ,
    ended_at        TIMESTAMPTZ,
    end_reason      TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_live_sessions_invite_hash
    ON live_sessions (invite_hash) WHERE invite_hash IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_live_sessions_org_created
    ON live_sessions (org_id, created_at DESC) WHERE org_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_live_sessions_owner_created
    ON live_sessions (owner_user_id, created_at DESC) WHERE owner_user_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_live_sessions_desk_status
    ON live_sessions (desk_id, status) WHERE desk_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_live_sessions_open
    ON live_sessions (status, expires_at) WHERE status IN ('waiting', 'active');
CREATE INDEX IF NOT EXISTS idx_live_sessions_call ON live_sessions (call_session_id)
    WHERE call_session_id IS NOT NULL;

-- ---------------------------------------------------------------------------
-- Participants. `peer_id` is server-minted and is what the room uses as the peer id,
-- so a client can never claim (and evict) someone else's id. Roles: the "owner side" is
-- exactly one of host (B2C owner) / staff (B2B desk answerer); the other side is the
-- guest. The two partial unique indexes make "max 2 per session" a database fact.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS live_participants (
    peer_id      UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id   UUID NOT NULL REFERENCES live_sessions(id) ON DELETE CASCADE,
    role         TEXT NOT NULL,                               -- 'host' | 'staff' | 'guest'
    user_id      UUID REFERENCES users(id) ON DELETE CASCADE, -- NULL for guests; name is PII, erased with the account
    display_name TEXT NOT NULL,
    lang         TEXT NOT NULL,
    token_hash   BYTEA,                                       -- SHA-256 of the guest token; NULL for signed-in sides
    consent_at   TIMESTAMPTZ,
    joined_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    left_at      TIMESTAMPTZ
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_live_participants_owner_side
    ON live_participants (session_id) WHERE role IN ('host', 'staff');
CREATE UNIQUE INDEX IF NOT EXISTS idx_live_participants_guest_side
    ON live_participants (session_id) WHERE role = 'guest';

-- ---------------------------------------------------------------------------
-- Idempotency ledger. A debit and its tick row are written in ONE transaction with
-- ON CONFLICT DO NOTHING on this primary key; a replayed or duplicated meter tick hits
-- the conflict and charges nothing. `amount` is in the payer's own unit (USD for a user,
-- credits for an org) — the matching ledger row stays the source of truth for money.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS live_usage_ticks (
    session_id UUID NOT NULL REFERENCES live_sessions(id)     ON DELETE CASCADE,
    peer_id    UUID NOT NULL REFERENCES live_participants(peer_id) ON DELETE CASCADE,
    tick_seq   BIGINT NOT NULL,
    payer_kind TEXT NOT NULL,                                 -- 'user' | 'org'
    amount     NUMERIC(14, 8) NOT NULL,
    ts         TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (session_id, peer_id, tick_seq)
);

-- Bounded diagnostics (join, leave, reconnect, error, capacity, echo_loop, exhausted,
-- mismatch). Never holds tokens or transcript text.
CREATE TABLE IF NOT EXISTS live_session_events (
    id         BIGSERIAL PRIMARY KEY,
    session_id UUID NOT NULL REFERENCES live_sessions(id) ON DELETE CASCADE,
    ts         TIMESTAMPTZ NOT NULL DEFAULT now(),
    kind       TEXT NOT NULL,
    detail     JSONB NOT NULL DEFAULT '{}'::jsonb
);
CREATE INDEX IF NOT EXISTS idx_live_session_events_session ON live_session_events (session_id, ts);

-- ---------------------------------------------------------------------------
-- Constraints: each guarded on pg_constraint and NOT VALID (see CLAUDE.md — an
-- `IF NOT EXISTS` on the table silently skips a CHECK on an already-migrated database).
-- NOT VALID still enforces every INSERT/UPDATE from now on.
-- ---------------------------------------------------------------------------
DO $$
BEGIN
    IF to_regclass('public.live_desks') IS NOT NULL
       AND NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'live_desks_status_check') THEN
        ALTER TABLE live_desks ADD CONSTRAINT live_desks_status_check
            CHECK (status IN ('active', 'revoked')) NOT VALID;
    END IF;
    IF to_regclass('public.live_desks') IS NOT NULL
       AND NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'live_desks_token_version_check') THEN
        ALTER TABLE live_desks ADD CONSTRAINT live_desks_token_version_check
            CHECK (token_version >= 1) NOT VALID;
    END IF;
    IF to_regclass('public.live_sessions') IS NOT NULL
       AND NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'live_sessions_status_check') THEN
        ALTER TABLE live_sessions ADD CONSTRAINT live_sessions_status_check
            CHECK (status IN ('waiting', 'active', 'ended', 'expired')) NOT VALID;
    END IF;
    IF to_regclass('public.live_sessions') IS NOT NULL
       AND NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'live_sessions_owner_check') THEN
        ALTER TABLE live_sessions ADD CONSTRAINT live_sessions_owner_check
            CHECK (owner_user_id IS NOT NULL OR org_id IS NOT NULL) NOT VALID;
    END IF;
    IF to_regclass('public.live_participants') IS NOT NULL
       AND NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'live_participants_role_check') THEN
        ALTER TABLE live_participants ADD CONSTRAINT live_participants_role_check
            CHECK (role IN ('host', 'staff', 'guest')) NOT VALID;
    END IF;
    IF to_regclass('public.live_usage_ticks') IS NOT NULL
       AND NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'live_usage_ticks_payer_check') THEN
        ALTER TABLE live_usage_ticks ADD CONSTRAINT live_usage_ticks_payer_check
            CHECK (payer_kind IN ('user', 'org')) NOT VALID;
    END IF;
END
$$;

-- ---------------------------------------------------------------------------
-- RLS: default-deny with no policy, matching every other table (spec 0096). The server
-- connects as the owner role and enforces tenancy in Rust; Directus connects the same way.
-- ---------------------------------------------------------------------------
ALTER TABLE live_desks           ENABLE ROW LEVEL SECURITY;
ALTER TABLE live_sessions        ENABLE ROW LEVEL SECURITY;
ALTER TABLE live_participants    ENABLE ROW LEVEL SECURITY;
ALTER TABLE live_usage_ticks     ENABLE ROW LEVEL SECURITY;
ALTER TABLE live_session_events  ENABLE ROW LEVEL SECURITY;

-- Guarded REVOKE (anon/authenticated exist only on Supabase, not on local/CI Postgres).
DO $$
DECLARE
    t TEXT;
BEGIN
    FOREACH t IN ARRAY ARRAY[
        'live_desks', 'live_sessions', 'live_participants',
        'live_usage_ticks', 'live_session_events'
    ] LOOP
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
            EXECUTE format('REVOKE ALL ON %I FROM anon', t);
        END IF;
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
            EXECUTE format('REVOKE ALL ON %I FROM authenticated', t);
        END IF;
    END LOOP;
END
$$;
