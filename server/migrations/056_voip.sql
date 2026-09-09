-- 056 — Real-time translated telephone calls (spec 0111).
--
-- A VoIP call IS a `call_sessions` row with `kind = 'phone'`. That is the whole
-- integration strategy: projects, transcripts, recordings, AI reports, semantic search,
-- retention sweeps and GDPR erasure already key off `call_sessions`, so a phone call
-- inherits every one of them without a line of new code. `voip_calls` carries only what
-- is telephony-specific and points back.
--
-- Conventions follow 016/037: idempotent DDL throughout (CREATE … IF NOT EXISTS, ADD
-- COLUMN IF NOT EXISTS), closed enums guarded by CHECK constraints, authorization
-- enforced in the Rust API layer, and RLS enabled-without-policy as default-deny
-- defence-in-depth (the server connects as the owning role and is exempt; anon and
-- authenticated get nothing through PostgREST).
--
-- Money is INTEGER credits (1 credit = 1 US cent, as `organizations.credits_balance`) or
-- NUMERIC for USD. Never floating point — see `server/src/voip/pricing.rs`.

-- `kind` already exists (032) as 'call' | 'voice_message'; 'phone' joins them. There is no
-- CHECK on that column to widen, by design — it was introduced as a free-form discriminator.
CREATE INDEX IF NOT EXISTS idx_call_sessions_phone
    ON call_sessions (org_id, started_at DESC)
    WHERE kind = 'phone';

-- ---------------------------------------------------------------------------
-- Per-org VoIP policy
-- ---------------------------------------------------------------------------

-- One row per organization. Absent row = VoIP disabled for that org, so enabling is
-- always an explicit administrative act that lands in `audit_logs`.
CREATE TABLE IF NOT EXISTS voip_org_settings (
    org_id                  UUID PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    enabled                 BOOLEAN NOT NULL DEFAULT FALSE,
    -- ISO 3166-1 alpha-2. Empty allow-list = "no country allow-list", NOT "no countries".
    -- The distinction matters: an accidentally-empty array must not silently ban the world
    -- (nor open it), so the Rust layer treats empty as absent and the default below is the
    -- conservative one for the block-list.
    allowed_countries       TEXT[] NOT NULL DEFAULT '{}',
    blocked_countries       TEXT[] NOT NULL DEFAULT '{}',
    allow_international     BOOLEAN NOT NULL DEFAULT TRUE,
    -- 'notice_only' | 'press_key' | 'verbal' | 'disabled' (spec 0111 R20). The default is
    -- the conservative one: if this org records or transcribes, the telephone participant
    -- is asked, not merely told.
    consent_policy          TEXT NOT NULL DEFAULT 'press_key'
                                CHECK (consent_policy IN ('notice_only', 'press_key', 'verbal', 'disabled')),
    -- What happens when consent is refused or times out: end the call, or continue it
    -- without recording/transcription. Continuing is the friendlier default and is legal
    -- in more places; an org in a regulated market sets 'end'.
    consent_refused_action  TEXT NOT NULL DEFAULT 'continue_unrecorded'
                                CHECK (consent_refused_action IN ('continue_unrecorded', 'end')),
    recording_enabled       BOOLEAN NOT NULL DEFAULT FALSE,
    transcription_enabled   BOOLEAN NOT NULL DEFAULT TRUE,
    ai_analysis_enabled     BOOLEAN NOT NULL DEFAULT FALSE,
    -- Days to keep a recording. NULL = follow the org's general retention setting.
    recording_retention_days INTEGER,
    max_call_minutes        INTEGER NOT NULL DEFAULT 60 CHECK (max_call_minutes > 0),
    max_concurrent_per_user INTEGER NOT NULL DEFAULT 2  CHECK (max_concurrent_per_user > 0),
    max_concurrent_per_org  INTEGER NOT NULL DEFAULT 10 CHECK (max_concurrent_per_org > 0),
    -- Credits. NULL = no org-level ceiling beyond the global one.
    monthly_spend_limit_credits INTEGER,
    -- Engine id (`engine::*_ID`), not a tier label. NULL = the deployment default.
    default_engine_id       TEXT,
    require_project         BOOLEAN NOT NULL DEFAULT FALSE,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- Numbers the org can present or receive on
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS voip_numbers (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id            UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    provider          TEXT NOT NULL,
    -- The provider's id for this number, so releasing it does not need a lookup.
    provider_number_id TEXT,
    -- Canonical E.164, `+` included. UNIQUE across the install: one number belongs to one
    -- org, and a number appearing twice would make inbound routing ambiguous.
    e164              TEXT NOT NULL UNIQUE,
    country           TEXT NOT NULL,
    label             TEXT,
    inbound_enabled   BOOLEAN NOT NULL DEFAULT FALSE,
    outbound_enabled  BOOLEAN NOT NULL DEFAULT TRUE,
    -- Default caller id for the org. Partial-unique below enforces at most one.
    is_default        BOOLEAN NOT NULL DEFAULT FALSE,
    project_id        UUID REFERENCES projects(id) ON DELETE SET NULL,
    team_id           UUID,
    -- Provider/regulator verification state. An unverified number must not be presented
    -- as caller id — arbitrary caller-id spoofing is illegal in most of our markets.
    verification_status TEXT NOT NULL DEFAULT 'pending'
                          CHECK (verification_status IN ('pending', 'verified', 'rejected')),
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_voip_numbers_org ON voip_numbers (org_id, created_at DESC);
-- At most one default caller id per org. A partial unique index rather than app logic,
-- because two concurrent "make this the default" requests would otherwise both win.
CREATE UNIQUE INDEX IF NOT EXISTS idx_voip_numbers_one_default
    ON voip_numbers (org_id) WHERE is_default;

-- ---------------------------------------------------------------------------
-- Destination rate deck
-- ---------------------------------------------------------------------------

-- Synced from the provider's public pricing. `fetched_at` is a correctness property, not
-- metadata: past `VOIP_RATE_MAX_AGE_SECS` a row is stale and the call is REFUSED rather
-- than dialed on a guessed price (spec 0111 R5, D11).
CREATE TABLE IF NOT EXISTS voip_rates (
    provider        TEXT NOT NULL,
    -- E.164 digits WITHOUT the leading '+'. Longest match wins.
    prefix          TEXT NOT NULL,
    description     TEXT NOT NULL DEFAULT '',
    -- What the provider charges US, per minute, in `currency`.
    cost_per_minute NUMERIC(12, 6) NOT NULL CHECK (cost_per_minute >= 0),
    currency        TEXT NOT NULL DEFAULT 'USD',
    -- 'mobile' | 'landline' | 'other'. Advisory: the price comes from the prefix, this is
    -- for the dialer to show and for reporting.
    number_type     TEXT NOT NULL DEFAULT 'other'
                        CHECK (number_type IN ('mobile', 'landline', 'other')),
    fetched_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (provider, prefix)
);

-- Longest-prefix lookup scans candidates for one provider ordered by prefix length.
CREATE INDEX IF NOT EXISTS idx_voip_rates_lookup
    ON voip_rates (provider, length(prefix) DESC, prefix);

-- ---------------------------------------------------------------------------
-- Calls
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS voip_calls (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    -- The shared session row. ON DELETE CASCADE so GDPR erasure of a session takes the
    -- telephony record with it rather than leaving an orphan holding a phone number.
    session_id        UUID NOT NULL UNIQUE REFERENCES call_sessions(id) ON DELETE CASCADE,
    org_id            UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- Who placed it. SET NULL, not CASCADE: the org's billing history must survive an
    -- employee leaving, and the personal data in this row is the phone number, not the id.
    user_id           UUID REFERENCES users(id) ON DELETE SET NULL,
    project_id        UUID REFERENCES projects(id) ON DELETE SET NULL,

    provider          TEXT NOT NULL,
    -- Provider anchorsite/region actually used, recorded so a residency claim can be
    -- audited after the fact instead of asserted from config that has since changed.
    provider_region   TEXT,
    -- Every leg the provider created for this call. Array rather than a child table: it is
    -- at most two or three entries, always read whole, and never joined on.
    provider_leg_ids  TEXT[] NOT NULL DEFAULT '{}',

    direction         TEXT NOT NULL CHECK (direction IN ('outbound', 'inbound', 'bridge')),
    -- Presented caller id and destination, canonical E.164.
    --
    -- The destination is stored in full ON PURPOSE. A sales rep has to be able to see who
    -- they called and the provider CDR is keyed on it; "no PII" (R23) is a rule about LOGS
    -- and observability, not about the record the customer paid for. `recipient_pseudonym`
    -- is the keyed HMAC that goes to logs, metrics and analytics instead.
    caller_e164       TEXT,
    recipient_e164    TEXT NOT NULL,
    recipient_pseudonym TEXT NOT NULL,
    recipient_country TEXT NOT NULL,

    source_language   TEXT NOT NULL,
    target_language   TEXT NOT NULL,
    -- Engine id, matching `usage_sessions.engine_id`.
    engine_id         TEXT NOT NULL,
    -- Whether EU-only processing was DEMANDED for this call. Recorded per call because the
    -- flag can change between calls and an audit asks about a specific one.
    eu_processing_required BOOLEAN NOT NULL DEFAULT FALSE,

    status            TEXT NOT NULL DEFAULT 'created'
                          CHECK (status IN ('created', 'dialing', 'ringing', 'answered',
                                            'bridged', 'ending', 'completed', 'failed')),
    failure_reason    TEXT,

    started_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    answered_at       TIMESTAMPTZ,
    ended_at          TIMESTAMPTZ,
    -- Our own measure. The provider's billable seconds live on the CDR columns below and
    -- need not agree; when they disagree the provider wins for cost and we win for the
    -- customer-facing duration, and both are kept so the difference is visible.
    duration_seconds  INTEGER,

    recording_status  TEXT NOT NULL DEFAULT 'none'
                          CHECK (recording_status IN ('none', 'pending', 'recording', 'saved', 'failed', 'deleted')),
    transcription_status TEXT NOT NULL DEFAULT 'none'
                          CHECK (transcription_status IN ('none', 'live', 'ready', 'failed')),

    -- Consent and disclosure (spec 0111 R19-R21). Every one of these is audit evidence.
    consent_policy       TEXT NOT NULL DEFAULT 'notice_only'
                          CHECK (consent_policy IN ('notice_only', 'press_key', 'verbal', 'disabled')),
    consent_status       TEXT NOT NULL DEFAULT 'not_required'
                          CHECK (consent_status IN ('not_required', 'pending', 'granted', 'denied', 'timeout')),
    disclosure_played_at TIMESTAMPTZ,
    disclosure_language  TEXT,
    consent_received_at  TIMESTAMPTZ,
    recording_started_at TIMESTAMPTZ,
    transcription_started_at TIMESTAMPTZ,

    -- Money. USD in NUMERIC, customer charge also mirrored in whole credits so the
    -- dashboard never re-derives it and drifts.
    estimated_cost_usd     NUMERIC(12, 6),
    quoted_price_per_min   NUMERIC(12, 6),
    reserved_credits       INTEGER NOT NULL DEFAULT 0,
    actual_provider_cost_usd NUMERIC(12, 6),
    customer_charge_usd    NUMERIC(12, 6),
    credits_consumed       INTEGER NOT NULL DEFAULT 0,
    -- Realised gross margin as a fraction. NULL until the CDR lands. A value below the
    -- configured floor is an alarm, never something to absorb quietly (R12).
    gross_margin           NUMERIC(6, 4),

    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Org call history, newest first — the dashboard's default view.
CREATE INDEX IF NOT EXISTS idx_voip_calls_org_started ON voip_calls (org_id, started_at DESC);
-- A member's own history (non-admins are scoped to calls they placed).
CREATE INDEX IF NOT EXISTS idx_voip_calls_user_started ON voip_calls (user_id, started_at DESC);
CREATE INDEX IF NOT EXISTS idx_voip_calls_project ON voip_calls (project_id, started_at DESC);
-- Webhook arrival: leg id -> call, on every single event, so it must not scan.
CREATE INDEX IF NOT EXISTS idx_voip_calls_legs ON voip_calls USING GIN (provider_leg_ids);
-- Billing reports and margin audits over a period.
CREATE INDEX IF NOT EXISTS idx_voip_calls_billing ON voip_calls (org_id, ended_at DESC)
    WHERE status = 'completed';
-- Concurrency caps count live calls; a partial index keeps that count off the whole table.
CREATE INDEX IF NOT EXISTS idx_voip_calls_live ON voip_calls (org_id, user_id)
    WHERE status IN ('created', 'dialing', 'ringing', 'answered', 'bridged', 'ending');

-- ---------------------------------------------------------------------------
-- Call quality / diagnostics
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS voip_call_quality (
    call_id             UUID PRIMARY KEY REFERENCES voip_calls(id) ON DELETE CASCADE,
    codec               TEXT,
    carrier             TEXT,
    setup_ms            INTEGER,
    disconnect_cause    TEXT,
    media_interruptions INTEGER NOT NULL DEFAULT 0,
    websocket_reconnects INTEGER NOT NULL DEFAULT 0,
    -- End-to-end translation latency, milliseconds. Measured from stamped pipeline events,
    -- never estimated (spec 0111 §P).
    latency_p50_ms      INTEGER,
    latency_p95_ms      INTEGER,
    latency_p99_ms      INTEGER,
    stt_confidence_avg  NUMERIC(5, 4),
    jitter_ms           INTEGER,
    packet_loss_pct     NUMERIC(5, 2),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- Provider event ledger — the idempotency mechanism
-- ---------------------------------------------------------------------------

-- Spec 0111 R25. Idempotency is a UNIQUE CONSTRAINT, not application logic: two workers
-- processing the same redelivered webhook concurrently would both pass an application-level
-- "have we seen this?" check, and one of them would double-charge.
CREATE TABLE IF NOT EXISTS voip_provider_events (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    provider     TEXT NOT NULL,
    provider_event_id TEXT NOT NULL,
    call_id      UUID REFERENCES voip_calls(id) ON DELETE CASCADE,
    leg_id       TEXT NOT NULL,
    event_type   TEXT NOT NULL,
    occurred_at  TIMESTAMPTZ NOT NULL,
    received_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- State before and after, so a lifecycle can be replayed and audited without guessing
    -- which events were no-ops.
    state_before TEXT,
    state_after  TEXT,
    UNIQUE (provider, provider_event_id)
);

CREATE INDEX IF NOT EXISTS idx_voip_events_call ON voip_provider_events (call_id, occurred_at);

-- ---------------------------------------------------------------------------
-- Credit reservations
-- ---------------------------------------------------------------------------

-- Spec 0111 R9/R10, decision D6. The existing ledger is atomic per deduction but holds
-- nothing for a call that has not happened yet, so N simultaneous dials can each see the
-- same balance and each start a call the org cannot pay for.
--
-- A hold is a REAL deduction from `organizations.credits_balance` at dial time, with its
-- own ledger row. The money leaves the pool immediately, so the race cannot happen at all
-- — there is nothing left for the second dial to see. Settlement refunds the unused part.
--
-- Invariant, asserted in tests: held_credits = settled_credits + released_credits
--                                            + shortfall_credits.
CREATE TABLE IF NOT EXISTS voip_credit_reservations (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    call_id           UUID NOT NULL REFERENCES voip_calls(id) ON DELETE CASCADE,
    org_id            UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    state             TEXT NOT NULL DEFAULT 'held'
                          CHECK (state IN ('held', 'settled', 'released')),
    held_credits      INTEGER NOT NULL CHECK (held_credits >= 0),
    settled_credits   INTEGER NOT NULL DEFAULT 0 CHECK (settled_credits >= 0),
    released_credits  INTEGER NOT NULL DEFAULT 0 CHECK (released_credits >= 0),
    -- What the call consumed beyond the hold and beyond what the pool could then cover.
    -- The call already happened; it cannot be un-happened, so the gap is recorded rather
    -- than hidden. A non-zero value here is a signal that the estimate is too low.
    shortfall_credits INTEGER NOT NULL DEFAULT 0 CHECK (shortfall_credits >= 0),
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    closed_at         TIMESTAMPTZ
);

-- One open hold per call. A second hold would double-charge the org for one call.
CREATE UNIQUE INDEX IF NOT EXISTS idx_voip_reservations_one_open
    ON voip_credit_reservations (call_id) WHERE state = 'held';
CREATE INDEX IF NOT EXISTS idx_voip_reservations_org ON voip_credit_reservations (org_id, created_at DESC);

-- ---------------------------------------------------------------------------
-- Route validation (China gate)
-- ---------------------------------------------------------------------------

-- Spec 0111 D10. `VOIP_CHINA_REQUIRE_VALIDATED_ROUTE` reads this table. A row exists only
-- when a real call was placed to a real handset in that country and the results were
-- recorded. A VPN test does not qualify and `tested_from_country` is what makes that
-- checkable rather than a promise in a runbook.
CREATE TABLE IF NOT EXISTS voip_route_validations (
    id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    provider           TEXT NOT NULL,
    country            TEXT NOT NULL,
    -- 'mobile' | 'landline'
    number_type        TEXT NOT NULL CHECK (number_type IN ('mobile', 'landline')),
    carrier            TEXT,
    -- Where the RECIPIENT physically was. Must equal `country` for the validation to count.
    tested_from_country TEXT NOT NULL,
    tested_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    tested_by          UUID REFERENCES users(id) ON DELETE SET NULL,
    call_setup_success BOOLEAN NOT NULL,
    post_dial_delay_ms INTEGER,
    one_way_audio      BOOLEAN NOT NULL DEFAULT FALSE,
    audio_quality_mos  NUMERIC(3, 2),
    translation_latency_ms INTEGER,
    dtmf_ok            BOOLEAN,
    caller_id_presented TEXT,
    hangup_detected    BOOLEAN,
    notes              TEXT,
    -- Only a validation that passed everything opens the gate.
    valid              BOOLEAN NOT NULL DEFAULT FALSE,
    expires_at         TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_voip_route_validations_lookup
    ON voip_route_validations (provider, country, number_type, tested_at DESC)
    WHERE valid;

-- ---------------------------------------------------------------------------
-- RLS: enabled, no policy. PostgREST anon/authenticated read and write NOTHING.
-- Real requests are authorized in the Rust API layer (require_role + org checks).
-- ---------------------------------------------------------------------------

ALTER TABLE voip_org_settings         ENABLE ROW LEVEL SECURITY;
ALTER TABLE voip_numbers              ENABLE ROW LEVEL SECURITY;
ALTER TABLE voip_rates                ENABLE ROW LEVEL SECURITY;
ALTER TABLE voip_calls                ENABLE ROW LEVEL SECURITY;
ALTER TABLE voip_call_quality         ENABLE ROW LEVEL SECURITY;
ALTER TABLE voip_provider_events      ENABLE ROW LEVEL SECURITY;
ALTER TABLE voip_credit_reservations  ENABLE ROW LEVEL SECURITY;
ALTER TABLE voip_route_validations    ENABLE ROW LEVEL SECURITY;

-- Guarded REVOKE (anon/authenticated exist only on Supabase, not local/CI Postgres).
DO $$
DECLARE
    t TEXT;
BEGIN
    FOREACH t IN ARRAY ARRAY[
        'voip_org_settings', 'voip_numbers', 'voip_rates', 'voip_calls',
        'voip_call_quality', 'voip_provider_events', 'voip_credit_reservations',
        'voip_route_validations'
    ] LOOP
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
            EXECUTE format('REVOKE ALL ON %I FROM anon', t);
        END IF;
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
            EXECUTE format('REVOKE ALL ON %I FROM authenticated', t);
        END IF;
    END LOOP;
END $$;
