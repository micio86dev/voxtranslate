-- 057 — Give GDPR erasure a durable handle on a telephone recording.
--
-- `voip_calls.recording_status` said a recording existed; nothing said WHERE. The
-- `call.recording.saved` webhook carried a URL and the code dropped it with a comment
-- deferring to "the recording service, which owns object storage" — but a phone recording
-- is not in our object storage. It is on the carrier's, and nothing in this system could
-- name it, play it or delete it.
--
-- That matters for one reason above the others: `voip_calls.user_id` is ON DELETE SET
-- NULL, deliberately, so an org's billing history survives an employee leaving. So
-- `DELETE FROM users` — Art. 17 erasure — does NOT cascade these rows away. Without a
-- handle, the erased person's recorded voice stays on the carrier's disk forever, and the
-- only pointer to it is destroyed by the very statement that was supposed to erase them.
-- Exactly the failure migration 053 fixed for chat uploads, one system over.
--
-- Two columns, two jobs:
--
--   * `provider_recording_id`  — the DURABLE handle. This is what deletion uses. Same
--                                principle as 053: an expiring signed URL is not a handle
--                                on the bytes, an id is.
--   * `provider_recording_url` — where the carrier put it. Recorded for operations and
--                                for reconciliation, and deliberately NOT exposed by any
--                                API: some carriers make these publicly fetchable, so
--                                serving one to a browser would put the recording behind a
--                                guessable link instead of behind authorisation.
--
-- Rows written before this migration keep NULL in both. Those recordings are permanently
-- unattributable and NOT reachable by erasure — a known limitation, stated here for the
-- same reason 053 stated its own.

ALTER TABLE voip_calls
    ADD COLUMN IF NOT EXISTS provider_recording_id  TEXT,
    ADD COLUMN IF NOT EXISTS provider_recording_url TEXT;

-- Erasure asks one question: "what recordings did this person's calls produce?" Partial,
-- because the overwhelming majority of calls have no recording at all and an index over
-- them would be mostly empty pages.
CREATE INDEX IF NOT EXISTS idx_voip_calls_recording_handle
    ON voip_calls (user_id)
    WHERE provider_recording_id IS NOT NULL;
