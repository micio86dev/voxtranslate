-- Semantic transcript search (spec: per-project semantic search in the Business
-- dashboard). Stores OpenAI `text-embedding-3-small` (1536-dim) vectors for chunks
-- of each diarized post-call transcript, so the Business API can run a pgvector
-- cosine KNN scoped to the projects a caller is allowed to see.
--
-- `org_id` and `project_id` are denormalized off the transcript's session so the
-- role-based scope filter (own/participated projects for members, all for admins)
-- runs without a join on the hot search path. `project_id` is nullable: a call can
-- be a business call without being bound to a project.

-- Requires the pgvector extension. Supabase (prod/local DATABASE_URL) ships it; CI
-- uses the pgvector/pgvector:pg16 image (see .github/workflows/ci.yml).
CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE IF NOT EXISTS transcript_embeddings (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    transcript_id UUID NOT NULL REFERENCES transcripts(id) ON DELETE CASCADE,
    session_id    UUID NOT NULL REFERENCES call_sessions(id) ON DELETE CASCADE,
    org_id        UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    project_id    UUID REFERENCES projects(id) ON DELETE SET NULL,
    chunk_index   INTEGER NOT NULL,
    speaker_name  TEXT,
    start_ms      BIGINT,
    end_ms        BIGINT,
    content       TEXT NOT NULL,              -- chunk text, returned as the result snippet
    embedding     VECTOR(1536) NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Approximate-nearest-neighbour index for cosine distance (`<=>`). HNSW gives
-- better recall/latency than IVFFlat and needs no training step.
CREATE INDEX IF NOT EXISTS idx_transcript_embeddings_hnsw
    ON transcript_embeddings USING hnsw (embedding vector_cosine_ops);
-- Scope filter: org + project (and the project-only / org-only variants share the
-- leading column). The ANN search is pre-filtered to the caller's visible set.
CREATE INDEX IF NOT EXISTS idx_transcript_embeddings_scope
    ON transcript_embeddings (org_id, project_id);
-- Re-index / cascade lookups by transcript (delete-then-insert on re-process).
CREATE INDEX IF NOT EXISTS idx_transcript_embeddings_transcript
    ON transcript_embeddings (transcript_id);

-- Mirror the project's Supabase hardening (migrations 011/016/029): default-deny
-- RLS with no policy, and direct grants revoked from anon/authenticated. The server
-- connects as the owning role (RLS-exempt); authorization lives in the Rust API.
ALTER TABLE transcript_embeddings ENABLE ROW LEVEL SECURITY;

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
    REVOKE ALL ON transcript_embeddings FROM anon;
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
    REVOKE ALL ON transcript_embeddings FROM authenticated;
  END IF;
END $$;
