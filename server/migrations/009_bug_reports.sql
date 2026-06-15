-- User bug/error reports (spec 0071). Submitted from the app by any user (guest or
-- signed-in); emailed to the admins and triaged in the Directus backoffice via the
-- `status` lifecycle. `gen_random_uuid()` is built into Postgres 13+.
CREATE TABLE IF NOT EXISTS bug_reports (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    message     TEXT NOT NULL,
    -- received (new) → cancelled | resolved. Drives the backoffice triage.
    status      TEXT NOT NULL DEFAULT 'received'
                CHECK (status IN ('received', 'cancelled', 'resolved')),
    -- Attribution + triage context. user_id is set only for signed-in reporters and
    -- survives account deletion as NULL; email/page_url/user_agent aid reproduction.
    user_id     UUID REFERENCES users(id) ON DELETE SET NULL,
    email       TEXT,
    page_url    TEXT,
    user_agent  TEXT
);

-- Backoffice list: newest open reports first.
CREATE INDEX IF NOT EXISTS bug_reports_status_created_idx
    ON bug_reports (status, created_at DESC);
