-- AI transcript correction on export (spec 0068). A paid, cached cleanup of the
-- exported transcript text using the whole dialogue as context.
--
-- The cache key is (session_id, mode, lang): one paid result per export shape.
-- `mode` is what the chosen export shows — 'original' polishes source text only
-- (lang is ''), 'translated'/'both' also polish the translation into `lang`.
-- `result_json` is a JSON array of per-event corrected fields, aligned to the
-- export's event order: [{"i": <idx>, "original"?: "...", "translated"?: "..."}].
-- UNIQUE(session_id, mode, lang) is the cache contract: a repeat export of the
-- same shape is served without re-running Groq or re-charging.
CREATE TABLE IF NOT EXISTS session_corrections (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id  UUID NOT NULL REFERENCES call_sessions(id) ON DELETE CASCADE,
    user_id     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    mode        TEXT NOT NULL CHECK (mode IN ('original', 'translated', 'both')),
    lang        TEXT NOT NULL DEFAULT '',
    result_json JSONB NOT NULL,
    model       TEXT NOT NULL,
    cost        DECIMAL(10, 6) NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (session_id, mode, lang)
);
