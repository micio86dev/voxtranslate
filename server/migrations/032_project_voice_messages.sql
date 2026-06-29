-- Project-attached voice messages (spec: B2B project voice notes). A Business/
-- Enterprise member records a voice note straight onto a PROJECT (no video call);
-- it's transcribed + translated and lands in the project's data history so the
-- existing semantic search + insights RAG consume it.
--
-- Implementation reuses the post-call transcript pipeline: each note materializes a
-- lightweight `call_sessions` row tagged `kind = 'voice_message'` plus a `transcripts`
-- row, then is embedded into `transcript_embeddings` exactly like a real call — so the
-- retrieval/scope SQL needs no changes. The `kind` discriminator lets the COLD
-- analytics/history aggregations exclude voice notes from call/minute KPIs.

-- Distinguish real calls from project voice notes. DEFAULT 'call' means every
-- existing row + the consumer/call flow is unaffected with no backfill.
ALTER TABLE call_sessions
    ADD COLUMN IF NOT EXISTS kind TEXT NOT NULL DEFAULT 'call';   -- 'call' | 'voice_message'
CREATE INDEX IF NOT EXISTS idx_call_sessions_kind
    ON call_sessions (org_id, kind, started_at DESC);

-- Audio artifact + dashboard/app list metadata for a project voice note. The
-- transcript text, translations and embeddings live in transcripts /
-- transcript_embeddings (reused); this row stores the storage object_path
-- (re-signed on read, never an expiring URL) and the fields the list views need.
CREATE TABLE IF NOT EXISTS project_voice_messages (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id           UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    project_id       UUID NOT NULL REFERENCES projects(id)      ON DELETE CASCADE,
    session_id       UUID NOT NULL REFERENCES call_sessions(id) ON DELETE CASCADE,
    transcript_id    UUID REFERENCES transcripts(id)            ON DELETE SET NULL,
    created_by       UUID REFERENCES users(id)                  ON DELETE SET NULL,
    created_by_name  TEXT NOT NULL,
    object_path      TEXT NOT NULL,
    file_name        TEXT NOT NULL,
    content_type     TEXT NOT NULL,
    size_bytes       BIGINT NOT NULL,
    duration_seconds INTEGER,
    source_language  TEXT NOT NULL,
    word_count       INTEGER,
    translated       BOOLEAN NOT NULL DEFAULT FALSE,
    status           TEXT NOT NULL DEFAULT 'ready',  -- 'ready' | 'failed'
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_pvm_project ON project_voice_messages (org_id, project_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_pvm_session ON project_voice_messages (session_id);

-- Mirror the project's Supabase hardening (migrations 011/016/029/030): default-deny
-- RLS with no policy, and direct grants revoked from anon/authenticated. The server
-- connects as the owning role (RLS-exempt); authorization lives in the Rust API.
ALTER TABLE project_voice_messages ENABLE ROW LEVEL SECURITY;

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
    REVOKE ALL ON project_voice_messages FROM anon;
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
    REVOKE ALL ON project_voice_messages FROM authenticated;
  END IF;
END $$;
