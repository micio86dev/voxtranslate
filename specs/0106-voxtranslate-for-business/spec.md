# 0106 — VoxTranslate for Business (org workspace, projects, cloud recording, transcripts)

| | |
|---|---|
| **Status** | In progress (Phase 1 — schema) |
| **Owner** | micio86dev |
| **Created** | 2026-06-24 |
| **Shipped** | — |
| **Version** | — |
| **Commits** | _pending_ |
| **Depends on** | [0005](../0005-accounts-credits-billing/spec.md), [0009 transcripts](../0009-call-history-transcripts/spec.md), [0018 storage](../0018-chat-file-uploads/spec.md), [0096 RLS](../0096-rls-lockdown/spec.md) |

## 1. Context & Problem

VoxTranslate is a per-user consumer product: a person signs in with Google, gets a
DECIMAL credit balance, joins ephemeral rooms, and is metered as they speak. There
is no notion of a **team**: no shared workspace, no shared credit pool, no way to
organize calls by project, and no durable, queryable **call history** with
**recordings** and **diarized transcripts**.

"VoxTranslate for Business" adds a multi-tenant layer — **organizations, members,
projects, cloud recording, transcripts, and call history** — on top of the existing
consumer app, **without touching** the call room logic, the consumer credit system,
or the individual auth flow. A user can keep using VoxTranslate normally *and* belong
to a business workspace; the two are independent.

**Stack reconciliation (important).** The original brief was written assuming a
**Supabase** stack (Supabase Auth, `auth.users`, client-side RLS, a `rooms` table,
`integer` credits, pgTAP). This repository is **not** Supabase-shaped:

| Brief assumed | This repo |
|---|---|
| Supabase Auth, `auth.users(id)` | Google OAuth + JWT; own `users(id)` (`migrations/001_init.sql`) |
| Client-side per-user RLS | Browser never touches Postgres; Rust API connects as the **RLS-exempt owner role**. `011_rls_lockdown.sql` uses RLS as **default-deny defense-in-depth only** |
| `supabase/migrations/<ts>.sql` | sqlx `server/migrations/NNN_*.sql` (next: `016`) |
| `ALTER TABLE rooms`, `room_id → rooms(id)` | **No `rooms` table** — rooms are ephemeral/in-memory. Persistent call record is **`call_sessions`** |
| `credits_balance integer`, pgTAP | Money is `DECIMAL(10,6)`; DB tests are **Rust integration tests** |
| `src/business/`, component-based Astro | Rust in `server/src/`; client is a monolithic `index.astro` + DOM-driven `app.ts` |

Supabase **Storage** (REST) *is* already wired (`server/src/storage.rs`, chat files),
so a private `recordings` bucket fits cleanly.

## 2. Goals / Non-Goals

**Goals**
- **Organizations** with members (`owner`/`admin`/`member`/`guest`), email invites,
  and projects to group calls.
- **Org credit pool** (INTEGER), a **separate currency** from consumer DECIMAL
  credits — business calls deduct org credits, never personal ones, and vice versa.
- **Cloud recording** of business calls → private Supabase Storage, with a
  GDPR recording notice to all participants.
- **Diarized transcripts** (Deepgram `diarize=true`) per recorded call, with
  on-demand translation cached per language.
- **Queryable call history** by org/project, with recording + transcript status.
- **Compliance mode**: audit log, configurable retention, EU storage.
- **Zero regressions** on the consumer flow (calls, metering, auth).

**Non-Goals (this spec)**
- Per-user DB-level (Supabase) RLS enforcement — authorization stays in the Rust layer.
- Replacing the ephemeral room model with a persistent `rooms` table.
- SSO/SAML, SCIM provisioning (future enterprise work).
- Real-time transcript editing UI (transcripts are post-call artifacts here).

## 3. Requirements

> Phase 1 (this milestone) pins **R1–R3**. R4–R12 are tracked for Phases 2–5.

- **R1 — Schema exists & is additive.** *Given* the migrations run, *when* the server
  boots, *then* all business tables exist with the documented columns/types, and
  `call_sessions` gains 5 nullable/defaulted business columns so existing consumer
  INSERTs and rows are unaffected.
- **R2 — Tenant isolation by membership.** *Given* users in different orgs, *when*
  the membership-scoped query / `get_user_org_role()` runs, *then* a user only ever
  resolves orgs/roles they belong to (non-members resolve to `NULL` → API 403).
- **R3 — Defense-in-depth RLS + app-managed `updated_at`.** *Given* every new table,
  *when* inspected, *then* RLS is enabled (no permissive policy, mirroring `011`);
  and `updated_at` advances on UPDATE via app code (no triggers).
- **R4 — Org CRUD & membership.** Create org (creator becomes `owner`); list my orgs
  with role; read/patch org (owner/admin); list members.
- **R5 — Invite flow.** Owner/admin invites by email → secret-token link; invitee
  views invite (public-by-token) and accepts (authenticated) → becomes a member.
- **R6 — Projects.** CRUD projects (read/create = member; update/soft-delete = admin/owner).
- **R7 — Associate a call.** Bind a room code to org/project + recording intent before
  the call; the materialized `call_session` inherits it.
- **R8 — Cloud recording.** On call end with recording on, upload audio to the private
  `recordings` bucket; deduct org credits (1/min, round up); enqueue transcription.
- **R9 — Transcription.** Async job downloads the recording, runs Deepgram
  `diarize=true`, writes `transcripts.segments`, sets `transcript_status`; 5 credits/hour.
- **R10 — Transcript translate (cached).** Translate to a target language; cache hit = 0
  credits; otherwise translate, cache, and deduct 2 credits/1000 words.
- **R11 — Signed access.** Recording playback signed URL TTL = 1h; export TTL = 15m;
  members only.
- **R12 — Compliance.** When `compliance_mode`, log `audit_logs` for
  transcript.view/export and recording.play; reads restricted to admin/owner.

## 4. Design & Architecture

**Data model (Phase 1 — `migrations/016_business_workspace.sql`)**
- `organizations(id, name, slug UNIQUE, plan, credits_balance INT, settings JSONB, owner_id→users, …)`
- `organization_members(org_id→orgs, user_id→users, role, invited_by, UNIQUE(org_id,user_id))`
- `organization_invites(org_id, email, role, token UNIQUE, invited_by, expires_at, accepted_at)`
- `projects(org_id, name, description, default_languages TEXT[], created_by, archived_at)`
- `call_sessions` **+** `org_id, project_id, cloud_recording_enabled, recording_storage_path, transcript_status`
- `room_business_bindings(room TEXT PK, org_id, project_id, cloud_recording_enabled, created_by)` — pre-call binding read by `RoomManager`
- `transcripts(session_id→call_sessions, org_id, source_language, segments JSONB, translations JSONB, duration_seconds, word_count)`
- `audit_logs(org_id, actor_id, action, resource_type, resource_id, metadata, ip_address)`
- `organization_credits_transactions(org_id, amount INT, type, description, session_id, stripe_payment_intent_id)`
- `get_user_org_role(p_org_id, p_user_id) → role` (SQL, STABLE) — backs Rust authz.

**Key decisions**
- *Authorization in Rust, not DB RLS* → the server runs as the owning role (RLS-exempt);
  per-user RLS would be dead code. RLS is `ENABLE`d with no policy purely to keep
  PostgREST `anon`/`authenticated` locked out (consistent with `011`). Rejected:
  re-architecting connections to pass per-user identity to Postgres.
- *`call_sessions` + `room_business_bindings` instead of a `rooms` table* → rooms are
  ephemeral; the only persistent call entity is `call_sessions`. The binding table holds
  the association before a call exists. Rejected: a new persistent `rooms` table
  (duplicates the call model, bigger blast radius).
- *INTEGER org credits, separate ledger* → org credits are an explicitly separate
  currency; never mixed with consumer DECIMAL credits.
- *`updated_at` in app code, no triggers* → matches the entire existing schema.
- *Spec's `room_id` → `session_id`* in `transcripts` / `organization_credits_transactions`.

**Deletion lifecycle (FK `ON DELETE`)** — chosen so both org deletion and consumer
GDPR account deletion succeed, and financial/compliance history survives:
- *Org deleted* → CASCADE its members, invites, projects, room bindings, transcripts,
  audit logs, and credit ledger; its `call_sessions` revert to consumer rows (`org_id`/
  `project_id` SET NULL) rather than being destroyed.
- *Call (`call_sessions`) deleted* → CASCADE its transcript; the credit ledger row
  survives with `session_id` SET NULL (immutable accounting).
- *User account deleted* → their membership CASCADEs; creator/inviter/actor pointers
  (`projects.created_by`, `audit_logs.actor_id`, `organization_members.invited_by`,
  `room_business_bindings.created_by`) SET NULL so org/compliance records persist; an
  owner's deletion CASCADEs the org (Phase 2 must require ownership transfer first, so
  this is a GDPR last resort, not the normal path).

**API (Phase 2 — `server/src/business/`, routes under `/api/business/…`)**
Organizations, members/invites, projects, rooms/history, recording, transcripts,
org Stripe — per the brief, adapted to the patterns in `api.rs`/`billing.rs`/`storage.rs`.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| **S0** | **Schema + RLS + helper** (this milestone) | `server/migrations/016_business_workspace.sql` |
| **S1** | **Schema verification tests** (this milestone) | `server/tests/business_schema.rs` |
| S2 | Org + member + invite CRUD & authz guards | `server/src/business/{organizations,members}.rs`, `routes.rs`, `lib.rs` |
| S3 | Projects + room association + history listing | `server/src/business/projects.rs` |
| S4 | Org credits + Stripe (purchase, subscription, webhook) | `server/src/business/credits.rs`, `stripe_handler.rs` |
| S5 | Cloud recording upload → private bucket | `server/src/business/recording.rs`, `storage.rs` |
| S6 | Async transcription (Deepgram `diarize=true`) + translate cache | `server/src/business/transcripts.rs` |
| S7 | Audit log (async, non-blocking) | `server/src/business/audit.rs` |
| S8 | Frontend: business dashboard routes + call-room hooks + i18n | `client/src/pages/business/*`, `app.ts` |
| S9 | Marketing site: `/business` page, homepage, blog | `website/` (submodule) |

## 6. Testing & Verification

**Phase 1 (`server/tests/business_schema.rs`, DB-gated)** pins R1–R3:
- `tables_and_columns_have_expected_types` — every table + key column types (R1).
- `call_sessions_gained_business_columns_only` — 5 additive cols + a legacy
  two-column INSERT still works with safe defaults (R1, regression guard).
- `rls_enabled_on_every_new_table` — RLS on all 8 tables (R3).
- `cross_org_reads_are_isolated` — membership query + `get_user_org_role` isolation (R2).
- `updated_at_advances_on_update` — app-code `updated_at` convention (R3).
- `business_rows_round_trip_and_cascade` — full org→project→call→transcript→audit→ledger
  round-trip and FK cascades (R1).

Run: `DATABASE_URL=… cargo test --test business_schema`; full suite `cargo test --locked`
must stay green (zero regressions). Coverage gate ≥85% applies from Phase 2 onward.

## 7. Deployment & Operations

- **Migration**: `016_business_workspace.sql` auto-applies on boot via `db::migrate`
  (idempotent; `IF NOT EXISTS` throughout). Requires `pgcrypto` for invite tokens
  (pre-installed on Supabase/CI).
- **Storage (Phase 2)**: create a private `recordings` bucket (service_role + signed
  URLs only); env mirrors the existing `SUPABASE_*` config in `storage.rs`.
- **Env (Phase 2)**: org Stripe price/plan ids; recording credit rates.
- **Rollout**: schema ships dark (no endpoints) — no consumer-visible change until Phase 2.

## 8. Risks / Open Items

- **pgcrypto** availability for `gen_random_bytes` (fallback: mint invite token in Rust).
- **Frontend architecture** (Phase 3): the app is a monolithic `index.astro` +
  DOM-driven `app.ts`, not component-based — the business dashboard will be new Astro
  routes; final structure to confirm at Phase 3.
- **84-locale i18n** for new UI strings (per project policy) — Phase 3.
- **EU storage / retention enforcement** (compliance) — design in Phase 2 (S5/S7).

## 9. References

- Files: `server/migrations/016_business_workspace.sql`, `server/tests/business_schema.rs`
- Patterns reused: `migrations/001_init.sql`, `004_transcripts.sql`, `011_rls_lockdown.sql`,
  `server/src/db.rs`, `server/src/storage.rs`, `server/tests/billing.rs`
- Brief: `prompts/voxtranslate-business.md`
