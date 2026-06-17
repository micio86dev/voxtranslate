-- Comprehensive RLS lockdown for the Supabase `public` schema (spec 0096, #239/#240).
--
-- Applied to production via the Supabase MCP as migration
-- `rls_lockdown_all_public_tables` on 2026-06-17. Kept here for reproducibility
-- because it covers tables the Rust sqlx migration (011) cannot: the Directus CMS
-- tables (`directus_*`, incl. directus_users/sessions/shares with password/token)
-- and `_sqlx_migrations`. The Rust server owns only the 24 app tables, so it must
-- not ALTER the Directus tables; this script runs as a superuser/`postgres` role.
--
-- SAFE: every public table is owned by `postgres`, which has BYPASSRLS, so the
-- Rust API and Directus (both connect via that role) are unaffected by ENABLE
-- (not FORCE) RLS. With RLS on and NO policy, only the PostgREST anon/authenticated
-- roles are denied — the app never uses the Supabase client, so default-deny is
-- correct. Verified post-apply: 0 rls_disabled / 0 sensitive_columns advisors;
-- live /api/content endpoints (which read locked tables) returned 200.
--
-- RE-RUN this after Directus creates new collections (new tables land RLS-off).

DO $$
DECLARE r RECORD;
BEGIN
  FOR r IN SELECT tablename FROM pg_tables WHERE schemaname = 'public' LOOP
    EXECUTE format('ALTER TABLE public.%I ENABLE ROW LEVEL SECURITY;', r.tablename);
  END LOOP;
END $$;

REVOKE ALL ON ALL TABLES IN SCHEMA public FROM anon, authenticated;
REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM anon, authenticated;
ALTER DEFAULT PRIVILEGES IN SCHEMA public REVOKE ALL ON TABLES FROM anon, authenticated;
ALTER DEFAULT PRIVILEGES IN SCHEMA public REVOKE ALL ON SEQUENCES FROM anon, authenticated;
