-- 049 — Webinar AI recap: report + multilingual participant emails (webinar
-- analytics epic, Phase B). Mirrors the MEET AI system (migration 005:
-- `session_reports` / `session_emails`) and its async job pattern (014:
-- `ai_jobs`), but keyed on `webinars(id)` instead of `call_sessions(id)`.
--
-- WHY NEW TABLES (not reusing session_reports / session_emails / ai_jobs):
-- all three are `session_id UUID NOT NULL REFERENCES call_sessions(id)`. A
-- webinar is NOT a `call_sessions` row (it has its own `webinar_sessions`),
-- so the FK can't be satisfied. Relaxing the FK + widening every CHECK/UNIQUE
-- on the shared meet tables is more invasive than clean, webinar-scoped tables.
-- The generation code (ai/report.rs, ai/email_draft.rs) is reused as-is; only
-- the storage + job-claim rows differ.
--
-- Conventions mirror 040_webinar_transcripts: FK cascade + RLS default-deny.

-- ---- ai_jobs, webinar-flavored --------------------------------------------
-- One live job slot per (webinar, feature, params_key) so a re-click joins the
-- running job instead of starting a second (double-charge) generation. Same
-- lifecycle (pending → done/failed, heartbeated `updated_at`) as `ai_jobs`.
CREATE TABLE IF NOT EXISTS webinar_ai_jobs (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    webinar_id  UUID NOT NULL REFERENCES webinars(id) ON DELETE CASCADE,
    -- The host who requested it (host-only endpoints).
    user_id     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    feature     TEXT NOT NULL CHECK (feature IN ('report', 'email')),
    params_key  TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'pending'
                CHECK (status IN ('pending', 'done', 'failed')),
    error       TEXT,
    result_json JSONB,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (webinar_id, feature, params_key)
);
CREATE INDEX IF NOT EXISTS webinar_ai_jobs_updated_idx ON webinar_ai_jobs (updated_at);

-- ---- Recap reports ---------------------------------------------------------
-- One report per (webinar, lang): re-requesting the same language upserts (the
-- idempotency key), a new language adds a row. Charged to the host's org.
CREATE TABLE IF NOT EXISTS webinar_reports (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    webinar_id UUID NOT NULL REFERENCES webinars(id) ON DELETE CASCADE,
    -- The host who requested it.
    user_id    UUID REFERENCES users(id) ON DELETE SET NULL,
    format     TEXT NOT NULL DEFAULT 'structured'
               CHECK (format IN ('structured', 'freeform')),
    lang       TEXT NOT NULL,
    markdown   TEXT NOT NULL,
    model      TEXT NOT NULL,
    -- Credits charged to the host org for this report (INTEGER org-credit pool).
    cost_credits INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- One report per (webinar, lang): re-run upserts, never a duplicate charge.
    UNIQUE (webinar_id, lang)
);
CREATE INDEX IF NOT EXISTS idx_webinar_reports_webinar
    ON webinar_reports (webinar_id, created_at DESC);

-- ---- Participant recap emails ---------------------------------------------
-- One row per (webinar, participant) email send. The dedup key ensures a
-- re-run never double-sends to the same participant. `participant_id` FKs to
-- `webinar_participants`; `lang` is the language the email was written in.
-- No recipient address is stored here (privacy) — it is resolved server-side at
-- send time via the participant's `user_id`.
CREATE TABLE IF NOT EXISTS webinar_emails (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    webinar_id     UUID NOT NULL REFERENCES webinars(id) ON DELETE CASCADE,
    participant_id UUID NOT NULL REFERENCES webinar_participants(id) ON DELETE CASCADE,
    lang           TEXT NOT NULL,
    subject        TEXT NOT NULL,
    status         TEXT NOT NULL DEFAULT 'sent'
                   CHECK (status IN ('sent', 'failed')),
    resend_id      TEXT,
    error          TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- One email per (webinar, participant): re-run skips already-sent rows.
    UNIQUE (webinar_id, participant_id)
);
CREATE INDEX IF NOT EXISTS idx_webinar_emails_webinar
    ON webinar_emails (webinar_id);

-- ---- RLS: default-deny defense-in-depth (mirrors 040) ----------------------
ALTER TABLE webinar_ai_jobs ENABLE ROW LEVEL SECURITY;
ALTER TABLE webinar_reports ENABLE ROW LEVEL SECURITY;
ALTER TABLE webinar_emails  ENABLE ROW LEVEL SECURITY;

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
    REVOKE ALL ON webinar_ai_jobs FROM anon;
    REVOKE ALL ON webinar_reports FROM anon;
    REVOKE ALL ON webinar_emails  FROM anon;
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
    REVOKE ALL ON webinar_ai_jobs FROM authenticated;
    REVOKE ALL ON webinar_reports FROM authenticated;
    REVOKE ALL ON webinar_emails  FROM authenticated;
  END IF;
END $$;
