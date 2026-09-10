-- 058 — Remember that the caller asked for an AI analysis of the call.
--
-- `DialOptions.ai_analysis` was collected from the dialer, used to decide what the
-- recipient's disclosure should say, and then dropped. Nothing on the row remembered it,
-- so nothing could act on it once the call ended — which is the only moment the analysis
-- can happen, because it reads the finished transcript.
--
-- Two columns rather than one, because "asked for" and "done" are different facts and a
-- sweep needs both: without the second, every pass would re-enqueue every call the
-- customer ever ticked the box for, and each enqueue spends their credits.
--
--   * `ai_analysis_requested`   — the caller ticked the box at dial time, having been
--                                 shown the price. That tick is the consent to charge;
--                                 there is no second confirmation later, and there should
--                                 not be one — the alternative is a report nobody asked
--                                 for twice.
--   * `ai_analysis_enqueued_at` — set when the job is claimed, so a call is considered
--                                 exactly once. `ai_jobs` has its own dedup on top of
--                                 this; the column is what keeps the SWEEP from asking.
--
-- The analysis itself is unchanged: same `ai_jobs` claim, same generation, same charge, as
-- the manual `POST …/report` route. This only removes the need for someone to click it.

ALTER TABLE voip_calls
    ADD COLUMN IF NOT EXISTS ai_analysis_requested  BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN IF NOT EXISTS ai_analysis_enqueued_at TIMESTAMPTZ;

-- The sweep's whole question: "which finished calls asked for an analysis and have not had
-- one?" Partial, because the overwhelming majority of calls never tick the box.
CREATE INDEX IF NOT EXISTS idx_voip_calls_ai_pending
    ON voip_calls (ended_at)
    WHERE ai_analysis_requested AND ai_analysis_enqueued_at IS NULL;
