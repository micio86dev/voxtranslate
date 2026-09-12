-- 059 — the column default must not contradict the product rule (spec 0112, 0111 R19).
--
-- `OrgSettings::default_for_new_org` states the rule in the team's own words: recording,
-- transcription and AI analysis stay OFF for an organisation that has not configured
-- them, because "a default that captures someone nobody asked is a different kind of
-- mistake from a default that refuses a call".
--
-- Migration 056 shipped `transcription_enabled BOOLEAN NOT NULL DEFAULT TRUE`, so three
-- places disagreed: the read path returned FALSE, the column said TRUE, and
-- `put_settings` defaulted an omitted field to TRUE. A row inserted by any path other
-- than that handler arrived with capture already permitted.
--
-- Only the DEFAULT changes. Existing rows are left exactly as they are: an organisation
-- that deliberately switched transcription on must keep it, and rewriting their choice
-- to match a new default would be its own kind of surprise.
--
-- `recording_enabled` already defaults to FALSE and is untouched. `allow_international`
-- defaults to TRUE in all three places and is therefore consistent, not a defect —
-- omitting it on a PUT restores the same value a new organisation gets.
-- Guarded, not bare. `sqlx::migrate!` runs in order, so 056 has created this table in
-- every database that reaches here honestly — but migration 044 taught this repo what an
-- unguarded ALTER does to a schema that has drifted ahead of the `_sqlx_migrations`
-- ledger: the server stops booting, and the fix needs a human with psql. A default is not
-- worth that, so a missing table is skipped rather than fatal.
DO $$
BEGIN
    IF to_regclass('public.voip_org_settings') IS NOT NULL THEN
        ALTER TABLE voip_org_settings
            ALTER COLUMN transcription_enabled SET DEFAULT FALSE;
    END IF;
END
$$;
