-- 015 — allow the `sentiment` feature in ai_jobs.
--
-- Sentiment analysis fans out into many Groq calls like report/correction/email
-- draft, so it joins the same async job model (POST returns a job handle, work
-- runs in the background, client polls). Migration 014 only whitelisted the
-- first three features; relax the CHECK to include `sentiment`.
ALTER TABLE ai_jobs DROP CONSTRAINT IF EXISTS ai_jobs_feature_check;
ALTER TABLE ai_jobs ADD CONSTRAINT ai_jobs_feature_check
    CHECK (feature IN ('report', 'correction', 'email_draft', 'sentiment'));
