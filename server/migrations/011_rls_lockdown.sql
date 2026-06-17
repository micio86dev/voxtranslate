-- 011 — Row Level Security lockdown (spec 0096 / issue #239)
--
-- WHY: Supabase flagged `rls_disabled_in_public` + `sensitive_columns_exposed`.
-- Every table lives in the `public` schema, so Supabase's auto-generated REST API
-- (PostgREST) could expose them to the `anon` / `authenticated` roles if anyone
-- obtained the anon key.
--
-- SAFE BY CONSTRUCTION: this app NEVER uses the Supabase client/anon key — the
-- browser talks only to the Rust API, which connects as the role that OWNS these
-- tables (the role that runs these migrations). `ENABLE` (NOT `FORCE`) RLS exempts
-- the owner, so the server is completely unaffected. With RLS on and NO permissive
-- policy, the `anon`/`authenticated` PostgREST roles can read/write NOTHING.
--
-- Default-deny is correct here: no table is ever read directly from the client, so
-- no app-facing policy is needed. If a future feature uses the Supabase client
-- directly, add explicit least-privilege policies for that table at that time.
--
-- Runs in every environment via sqlx (prod Supabase, local Docker Postgres, CI).
-- The owner exemption keeps the server working everywhere; the REVOKE is guarded
-- because the `anon`/`authenticated` roles only exist on Supabase.

-- ---- Account / billing (HIGH sensitivity: PII + financial) -------------------
ALTER TABLE users               ENABLE ROW LEVEL SECURITY;
ALTER TABLE credit_transactions ENABLE ROW LEVEL SECURITY;
ALTER TABLE usage_sessions      ENABLE ROW LEVEL SECURITY;
ALTER TABLE stripe_events       ENABLE ROW LEVEL SECURITY;

-- ---- Trust & safety / admin (HIGH) ------------------------------------------
ALTER TABLE blocklist_terms     ENABLE ROW LEVEL SECURITY;
ALTER TABLE admin_audit         ENABLE ROW LEVEL SECURITY;
ALTER TABLE bug_reports         ENABLE ROW LEVEL SECURITY;

-- ---- Call content / transcripts (HIGH: user-generated, may contain PII) ------
ALTER TABLE call_sessions        ENABLE ROW LEVEL SECURITY;
ALTER TABLE session_participants ENABLE ROW LEVEL SECURITY;
ALTER TABLE transcript_events    ENABLE ROW LEVEL SECURITY;
ALTER TABLE transcript_bookmarks ENABLE ROW LEVEL SECURITY;
ALTER TABLE session_reports      ENABLE ROW LEVEL SECURITY;
ALTER TABLE reports              ENABLE ROW LEVEL SECURITY;
ALTER TABLE session_corrections  ENABLE ROW LEVEL SECURITY;
ALTER TABLE session_sentiments   ENABLE ROW LEVEL SECURITY;
ALTER TABLE session_emails       ENABLE ROW LEVEL SECURITY;
ALTER TABLE chat_files           ENABLE ROW LEVEL SECURITY;

-- ---- Room features (MED/LOW) -------------------------------------------------
ALTER TABLE room_glossaries  ENABLE ROW LEVEL SECURITY;
ALTER TABLE glossary_entries ENABLE ROW LEVEL SECURITY;

-- ---- Localized / public content (served via the Rust API, not PostgREST) -----
ALTER TABLE languages          ENABLE ROW LEVEL SECURITY;
ALTER TABLE i18n_strings       ENABLE ROW LEVEL SECURITY;
ALTER TABLE i18n_translations  ENABLE ROW LEVEL SECURITY;
ALTER TABLE legal_pages        ENABLE ROW LEVEL SECURITY;
ALTER TABLE legal_translations ENABLE ROW LEVEL SECURITY;

-- ---- Defense in depth: drop the PostgREST roles' table privileges entirely ----
-- (Guarded: these roles exist only on Supabase, not in local/CI Postgres.)
DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
    REVOKE ALL ON ALL TABLES IN SCHEMA public FROM anon;
    REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM anon;
  END IF;
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
    REVOKE ALL ON ALL TABLES IN SCHEMA public FROM authenticated;
    REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM authenticated;
  END IF;
END $$;
