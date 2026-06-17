# 0096 — Supabase RLS lockdown + security audit

| | |
|---|---|
| **Status** | ✅ Applied to prod + verified (live audit clean) |
| **Owner** | micio86dev |
| **Created** | 2026-06-17 |
| **Shipped** | — |
| **Version** | — |
| **Commits** | _(pin on merge)_ |
| **Depends on** | 001–010 (schema) |

## 1. Context & Problem

Supabase advisors flagged `rls_disabled_in_public` and `sensitive_columns_exposed`
(issues #239, #240). All 24 application tables live in the `public` schema with
**Row Level Security disabled**, so Supabase's auto-generated PostgREST API could
serve them to the `anon` / `authenticated` roles to anyone holding the anon key.

**Crucial architectural fact (verified):** the browser **never** uses the Supabase
client or anon key. Every read/write goes through the **Rust API**
(`api.voxtranslate.app`), which connects as the role that **owns** these tables
(it runs the migrations). i18n + legal "public" content is also served via the Rust
API (`/api/content/...`), not PostgREST. So **no table is ever read directly from
the client.**

## 2. Goals / Non-Goals

**Goals**
- Enable RLS on every `public` table so the PostgREST `anon`/`authenticated` roles
  can read/write nothing (default-deny).
- Zero impact on the Rust API (and therefore the app).
- Run safely in every environment the migration touches (prod Supabase, local
  Docker Postgres, CI).

**Non-Goals**
- Per-user "own rows" policies — unnecessary, since authenticated users have **no
  direct DB access**; the Rust API enforces per-user authorization in code. (Add
  policies only if a future feature uses the Supabase client directly.)
- Moving tables out of `public`, or column-level encryption.

## 3. Design & Key Decision

`migrations/011_rls_lockdown.sql`:
- `ALTER TABLE … ENABLE ROW LEVEL SECURITY` on all 24 tables.
  - **`ENABLE`, not `FORCE`** → the **table owner is exempt**. The Rust API
    connects as the owner, so it is completely unaffected; `anon`/`authenticated`
    (not owners) get default-deny.
- Defense in depth: `REVOKE ALL … FROM anon, authenticated`, **guarded** by
  `pg_roles` existence (those roles exist only on Supabase, not in local/CI
  Postgres), so the same migration runs everywhere.

**Why default-deny with no policies is correct:** the app's only DB client is the
owner-role Rust server; PostgREST is not a legitimate access path here, so the
right posture is "expose nothing through it."

## 4. Table audit (all → RLS ENABLED, 0 policies = deny for anon/authenticated)

| Table | Purpose | Sensitivity | Reached by app via |
|---|---|---|---|
| users | accounts: email, name, balance | **HIGH** (PII + $) | Rust API |
| credit_transactions | credit ledger | **HIGH** ($) | Rust API |
| usage_sessions | per-call billing | MED | Rust API |
| stripe_events | Stripe webhook idempotency | **HIGH** ($) | Rust API |
| blocklist_terms | moderation word list | MED | Rust API |
| admin_audit | backoffice action log | **HIGH** | Rust API |
| bug_reports | user messages + email | MED (PII) | Rust API |
| call_sessions | call lifecycle | MED | Rust API |
| session_participants | who was in a call | **HIGH** (PII) | Rust API |
| transcript_events | spoken/chat transcript | **HIGH** (PII content) | Rust API |
| transcript_bookmarks | pinned moments | MED | Rust API |
| session_reports | AI reports | MED | Rust API |
| reports | participant abuse reports | **HIGH** | Rust API |
| session_corrections | AI-corrected text cache | MED | Rust API |
| session_sentiments | AI sentiment | MED | Rust API |
| session_emails | AI email drafts | MED (PII) | Rust API |
| chat_files | uploaded documents | **HIGH** | Rust API |
| room_glossaries | per-room glossary | LOW | Rust API |
| glossary_entries | glossary terms | LOW | Rust API |
| languages | language list | PUBLIC | Rust API (`/api/content`) |
| i18n_strings | UI string keys | PUBLIC | Rust API (`/api/content`) |
| i18n_translations | UI translations | PUBLIC | Rust API (`/api/content`) |
| legal_pages | legal page keys | PUBLIC | Rust API (`/api/content`) |
| legal_translations | legal page bodies | PUBLIC | Rust API (`/api/content`) |

## 5. Verification

- **Applied locally** against Docker Postgres: migration runs clean; the owner
  round-trip (`db::tests::migrate_and_round_trip_user`) still passes.
- **Full `--test billing` suite (15 tests) green post-RLS**, including
  `content_api::i18n_and_legal_served_from_db` (proves the locked-down i18n/legal
  tables still serve through the Rust API) and the account/usage/transcript paths.

## 6. Live audit on production (#240) — DONE

Ran via the Supabase MCP on the production project.

**Critical finding static analysis missed:** the prod `public` schema had **54**
tables, not 24 — it also contains the **Directus CMS** tables (`directus_*`) and
`_sqlx_migrations`. Worse, `sensitive_columns_exposed` flagged **`directus_users`
(password, token)**, **`directus_sessions` (token)** and **`directus_shares`
(password)** — admin credential hashes + session tokens reachable via PostgREST if
RLS stayed off. The sqlx migration (011) only covers the 24 app tables (the Rust
role owns those); the Directus tables needed a separate lockdown.

**Verified before applying:** all 54 public tables are owned by `postgres`, which
has `BYPASSRLS = true` (as does `service_role`); `anon`/`authenticated` do not. So
`ENABLE` (not `FORCE`) RLS cannot affect the Rust API or Directus (both connect via
`postgres` → owner + BYPASSRLS) — only the PostgREST roles are denied.

**Applied** (Supabase migration `rls_lockdown_all_public_tables`, mirrored in the
repo at `infra/supabase/rls-lockdown-all-public.sql`): a dynamic loop enabling RLS
on **all 54** public tables + `REVOKE` of anon/authenticated table/sequence grants
+ `ALTER DEFAULT PRIVILEGES` for future objects.

**Re-audit (after):**
- `rls_disabled_in_public`: **0** (was 54, ERROR) ✅
- `sensitive_columns_exposed`: **0** (was 12, ERROR — incl. directus password/token) ✅
- Remaining: only `rls_enabled_no_policy` at **INFO** — the *intended* default-deny
  state (no policies, because the app never uses the Supabase client).
- `pg_tables`: 54/54 `rowsecurity = true`; `anon`/`authenticated` hold 0 table grants.

**Regression (live, post-lockdown):** `GET /health` → 200; `GET /api/content/i18n`
→ 200 (28 KB, reads the now-locked `i18n_strings`/`i18n_translations`); `GET
/api/content/legal/privacy` → 200 (reads locked `legal_translations`). The Rust API
reads locked tables fine — confirms no breakage.

## 7. Residual notes
- New Directus collections created later land RLS-disabled again → re-run
  `infra/supabase/rls-lockdown-all-public.sql` after schema changes (or schedule it).
- sqlx migration 011 (24 app tables) still ships and auto-applies on deploy —
  idempotent against the already-locked prod state.
