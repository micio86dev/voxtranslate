# 0096 — Supabase RLS lockdown + security audit

| | |
|---|---|
| **Status** | In progress (migration written + verified; live re-audit pending apply) |
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

## 6. Remaining (needs Supabase access — #239/#240 live re-audit)

After the migration is applied to **production Supabase**, run the read-only
re-audit (Supabase MCP or SQL) to confirm on the live DB:
1. `SELECT relname, relrowsecurity FROM pg_class … WHERE relnamespace = 'public'::regnamespace` → every table `relrowsecurity = true`.
2. As `anon` / `authenticated` (PostgREST), every table returns 0 rows / permission denied.
3. No `SECURITY DEFINER` function grants privilege escalation; storage buckets are
   not publicly writable unless intended.
4. The app's critical flows (login, join call, payments, transcripts) still work
   end-to-end (regression).

This last step is a **read-only validation** — no further changes expected unless
the live audit surfaces a table created outside these migrations (e.g. by Directus)
that also needs RLS.
