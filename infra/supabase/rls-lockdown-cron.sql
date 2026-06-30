-- Durable RLS lockdown for the Supabase `public` schema — follow-up to
-- rls-lockdown-all-public.sql. Schedules a nightly pg_cron job that re-enables
-- RLS on any public table still missing it, so Directus collections (and any
-- other table) created after the last manual lockdown can't sit RLS-off and
-- re-trigger the `rls_disabled_in_public` advisor (the recurring root cause).
--
-- Run ONCE per project in the Supabase SQL editor — staging
-- `sdhhgrinwfhxdicjlzwe`, then prod `sqqzopinejlqnletloyl`. Idempotent:
-- re-pasting is safe (the function is CREATE OR REPLACE and the cron job is
-- upserted by name). The setup call at the bottom also performs the lockdown
-- immediately, so this both FIXES NOW and prevents future drift.
--
-- SAFE: runs as `postgres` (BYPASSRLS), so the Rust API and Directus are
-- unaffected; ENABLE (not FORCE) RLS exempts the owner. Only the PostgREST
-- anon/authenticated roles are denied — and the app never uses the Supabase
-- client, so default-deny is correct.

-- 1. pg_cron (Supabase installs it into the `cron` schema).
create extension if not exists pg_cron;

-- 2. The lockdown routine. Only touches tables that still need it, so it takes
--    no locks on already-secured tables. SECURITY DEFINER + empty search_path
--    so it always runs with the owner's privileges and can't be search-path
--    hijacked (also clears the `function_search_path_mutable` advisor).
create or replace function public.enforce_public_rls()
  returns void
  language plpgsql
  security definer
  set search_path = ''
as $$
declare
  r record;
begin
  -- Enable RLS on every public table that doesn't have it yet.
  for r in
    select c.relname
    from pg_class c
    join pg_namespace n on n.oid = c.relnamespace
    where n.nspname = 'public'
      and c.relkind in ('r', 'p')   -- ordinary + partitioned tables
      and c.relrowsecurity = false  -- skip tables already locked
  loop
    execute format('alter table public.%I enable row level security;', r.relname);
  end loop;

  -- Defense in depth: strip the PostgREST roles' privileges (guarded — these
  -- roles exist only on Supabase, not in local/CI Postgres).
  if exists (select 1 from pg_roles where rolname = 'anon') then
    revoke all on all tables    in schema public from anon;
    revoke all on all sequences in schema public from anon;
  end if;
  if exists (select 1 from pg_roles where rolname = 'authenticated') then
    revoke all on all tables    in schema public from authenticated;
    revoke all on all sequences in schema public from authenticated;
  end if;

  -- Future tables inherit no anon/authenticated grants.
  alter default privileges in schema public revoke all on tables    from anon, authenticated;
  alter default privileges in schema public revoke all on sequences from anon, authenticated;
end;
$$;

-- The function only ever tightens security, but keep it off the public REST
-- surface so it isn't callable as an anon/authenticated RPC.
revoke all on function public.enforce_public_rls() from public;

-- 3. Lock down NOW, then nightly at 03:17 UTC. The named cron.schedule overload
--    upserts by job name, so re-running this just refreshes the schedule.
select public.enforce_public_rls();

select cron.schedule(
  'enforce-public-rls',
  '17 3 * * *',
  $$select public.enforce_public_rls();$$
);
