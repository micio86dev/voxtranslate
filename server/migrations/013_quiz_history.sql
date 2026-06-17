-- 013 — Quiz history persistence (spec 0098 / issue #221)
--
-- Quizzes (spec 0046/0067) are currently ephemeral — relayed as in-room game state
-- and lost when the call ends. Persist them per call so the session-detail page can
-- show what quizzes ran and how each participant scored.
--
-- Reuses the existing `call_sessions(id)` as the call key (no duplicate session
-- table). `created_by` / `user_id` are NULL for guests. RLS-enabled (spec 0096):
-- only the owner-role Rust API reads/writes these; never the Supabase anon API.

CREATE TABLE IF NOT EXISTS session_quizzes (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id UUID NOT NULL REFERENCES call_sessions(id) ON DELETE CASCADE,
    title      TEXT,
    -- [{ prompt, options: [text...], correct_index }]
    questions  JSONB NOT NULL DEFAULT '[]'::jsonb,
    created_by UUID REFERENCES users(id) ON DELETE SET NULL, -- host; NULL for guest host
    status     TEXT NOT NULL DEFAULT 'completed'
               CHECK (status IN ('active', 'completed', 'cancelled')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_quiz_session ON session_quizzes (session_id);

CREATE TABLE IF NOT EXISTS quiz_results (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    quiz_id      UUID NOT NULL REFERENCES session_quizzes(id) ON DELETE CASCADE,
    user_id      UUID REFERENCES users(id) ON DELETE CASCADE, -- NULL for guests
    peer_id      TEXT NOT NULL,    -- identity within the call (guests have no user_id)
    display_name TEXT NOT NULL,
    score        INTEGER NOT NULL DEFAULT 0,  -- correct answers
    total        INTEGER NOT NULL DEFAULT 0,  -- number of questions
    answers      JSONB NOT NULL DEFAULT '[]'::jsonb, -- chosen option indices
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_quiz_results_quiz ON quiz_results (quiz_id);
CREATE INDEX IF NOT EXISTS idx_quiz_results_user ON quiz_results (user_id);

ALTER TABLE session_quizzes ENABLE ROW LEVEL SECURITY;
ALTER TABLE quiz_results    ENABLE ROW LEVEL SECURITY;
