-- 014 — Async AI job tracking (report, transcript correction, email draft).
--
-- WHY: these paid features fan out into many Groq calls. On a multi-hour
-- transcript the synchronous request runs longer than the edge proxy's request
-- ceiling (~100s) and the client sees a 502. The POST endpoints now CLAIM a job
-- here, return immediately (202 + job id), and run generation in a background
-- task; the client polls `GET /api/sessions/{id}/ai-job/{job_id}` until the job
-- is `done` (with `result_json`) or `failed` (with `error`). No HTTP request is
-- ever held open long enough to time out.
--
-- The feature RESULT itself still lands in its own table (session_reports /
-- session_corrections / session_emails) exactly as before — this table only
-- carries liveness + the JSON body the POST used to return inline, so the client
-- can render it without re-fetching.
CREATE TABLE IF NOT EXISTS ai_jobs (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id  UUID NOT NULL REFERENCES call_sessions(id) ON DELETE CASCADE,
    user_id     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    feature     TEXT NOT NULL CHECK (feature IN ('report', 'correction', 'email_draft')),
    -- Stable digest of the request params that determine the result. A duplicate
    -- in-flight request (double-click) joins the running job via the UNIQUE key
    -- below instead of starting a second — and double-charging — generation.
    params_key  TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'pending'
                CHECK (status IN ('pending', 'done', 'failed')),
    -- Failure reason for the client ('groq', 'insufficient_credits', 'db',
    -- 'unavailable'); NULL while pending and on success.
    error       TEXT,
    -- The JSON body the synchronous endpoint used to return (cost, balance,
    -- markdown/email/correction meta). Present on `done`; on an
    -- `insufficient_credits` failure it carries the standard 402 body.
    result_json JSONB,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- One live slot per (session, feature, params): the claim upsert keys on it.
    UNIQUE (session_id, feature, params_key)
);

-- Reclaim (stale pending) + prune (old terminal) scans filter on updated_at.
CREATE INDEX IF NOT EXISTS ai_jobs_updated_idx ON ai_jobs (updated_at);

-- RLS lockdown (mirrors migration 011): the server OWNS this table and is exempt
-- from RLS; the Supabase anon/authenticated PostgREST roles get nothing.
ALTER TABLE ai_jobs ENABLE ROW LEVEL SECURITY;
