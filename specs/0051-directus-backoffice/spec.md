# 0051 — Directus backoffice: KPI dashboards, Stripe movements, acquisition source

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-14 |
| **Shipped** | 2026-06-14 |
| **Version** | — |
| **Commits** | `627325b` |
| **Depends on** | [0007 backoffice](../0007-backoffice-directus/spec.md), [0008 i18n](../0008-managed-content-i18n/spec.md), [0019 admin bonus](../0019-admin-bonus-credits/spec.md) |

## 1. Context & Problem

The Directus backoffice so far only edits content and runs the privileged Flow
buttons (ban / credit / bonus / resolve / delete). There was **no at-a-glance view
of how the product is doing** — no KPIs, no revenue view, no feature-usage stats —
and the app tables show up as raw, ungrouped SQL tables. The operator also asked
for two specific things:

- **Stripe movements** — see earnings (completed purchases) and the promotional
  credit we give away, so revenue and cost of acquisition are legible.
- **Marketing attribution** — a `?source` query param so we know *where registered
  users came from* (which campaign), reported in the backoffice.

## 2. Goals / Non-Goals

**Goals**
- A reproducible, idempotent script that provisions the whole backoffice over the
  Directus REST API (like `setup-bonus-flow.mjs`) — no click-ops, repeatable.
- Register every app table as a managed collection, grouped into folders with
  icons, colours and display templates.
- Four Insights dashboards of KPI panels: **Overview**, **Billing & Stripe**,
  **Moderation**, **Acquisition & Features**.
- **First-touch acquisition source** captured at sign-up and surfaced as a
  "users by source" KPI.

**Non-Goals**
- Refunds / chargebacks as Stripe movements — the webhook only handles
  `checkout.session.completed`; refunds aren't recorded, so "Revenue" = purchases.
- Our infra costs (Deepgram / Groq / Railway / Resend) — those are external to the
  DB; "spese" in the dashboard means promotional credit granted, not infra spend.
- Scripting Directus **permissions** (role-id specific) — the super-admin role
  bypasses them; non-admin roles still need read grants set by hand (README §6).

## 3. Requirements

- **R1 — Acquisition source (server).** `users.source TEXT` (migration `007`).
  `POST /api/auth/google` accepts an optional `source`; it is stamped **only on the
  INSERT** (first login) and never overwritten — first-touch attribution. Trimmed,
  capped at 64 chars, empty → NULL.
- **R2 — Acquisition source (client).** On every page load, capture `?source`
  (fallback `utm_source`, then `ref`) into `localStorage` (`vox.src`) — first touch
  wins — and send it with the Google login. Best-effort; no-op where storage/URL
  is unavailable.
- **R3 — Collections.** Idempotent registration of all 22 app tables as collections
  in 5 folders (accounts / sessions / moderation / ai_features / content) with
  icon, colour, note, display template and sort.
- **R4 — Dashboards.** Idempotent Insights dashboards with metric / time-series /
  list / bar-chart panels (re-running rebuilds panels). Billing dashboard shows
  Revenue (sum of `purchase`), promo credits (`free_credit`+`bonus`), credits spent
  (`usage`), outstanding balance, AI charges, a revenue/day series and the Stripe
  events log. Acquisition dashboard shows attributed vs organic users and a
  users-by-`source` bar chart.

## 4. Design & Architecture

- **Server:** `migrations/007_source.sql` (+ index); `db::User.source`;
  `auth::GoogleAuthRequest.source` + `clean_source()`; `upsert_google_user(..,
  source)` binds it on INSERT only; `auth_google` passes the cleaned value.
- **Client:** `auth.ts` gains `captureAcquisitionSource()` / `getAcquisitionSource()`
  (`vox.src`); `loginWithGoogle` includes `source`; `app.ts boot()` calls capture
  first, before the URL is tidied.
- **Directus:** `directus/setup-backoffice.mjs` — REST provisioner. Collections are
  registered PATCH-first (already-managed tables), falling back to
  `POST /collections {schema:null}` (registers metadata against an existing table
  without a CREATE TABLE). Dashboards/panels are upserted; each panel POST is
  independently try/caught so one bad panel can't abort the run.
- **Key decisions:**
  - *First-touch, server-stamped source.* Attribution belongs to the account's
    origin; capturing client-side but persisting server-side (INSERT only) keeps it
    tamper-light and immune to later visits with a different `utm_source`.
  - *Provision via API, not the UI.* The prod Directus is org-blocked in a browser;
    a script is reproducible and reviewable, and Insights/Flows live in Directus's
    own tables (not the app schema) so they can't be app migrations.
  - *Resilient idempotency.* Re-running refreshes metadata and rebuilds panels, so
    the script is also the way we iterate the dashboards.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `users.source` column + index | `migrations/007_source.sql` |
| S1 | Server: request field, `clean_source`, INSERT-only stamp, tests | `auth.rs`, `db.rs` |
| S2 | Client: capture + send `source`, boot hook, tests | `auth.ts`, `app.ts`, `auth.test.ts` |
| S3 | Directus provisioner: folders, collections, 4 dashboards | `directus/setup-backoffice.mjs` |
| S4 | Runbook | `directus/README.md` |

## 6. Testing & Verification

- `cargo build` + clippy clean; `auth::clean_source` unit test; the DB-gated
  `upsert_grants_free_credit_once` test extended to assert first-touch source.
- `astro check` clean; `auth.test.ts` extended (capture / first-touch / fallback /
  omit-when-absent) — 28 client auth tests pass.
- The Directus script is `node --check`-clean. It runs against the live Directus
  with admin creds (cannot be CI-tested) — first-run-and-iterate, like
  `setup-bonus-flow.mjs`.

## 7. Deployment & Operations

- **Server** change (migration + auth) → `railway up`. The migration runs on boot.
- **Client** change → Vercel autodeploys on merge to `main`.
- **Directus** → operator runs `node directus/setup-backoffice.mjs` with
  `DIRECTUS_URL` + admin creds (README §9). Re-runnable to refresh.
- Mark a campaign by linking with `?source=<campaign>` (e.g.
  `https://voxtranslate.app/?source=reddit-launch`); the value is attributed to any
  account created in that session.

## 8. Risks / Open Items

- Panel `options` schemas are version-sensitive; a panel that fails to render is
  logged and skipped (the rest still provision) — iterate from the warnings.
- `POST /collections {schema:null}` to register an existing table is the fallback
  path; if a Directus version rejects it, the warning names the table and we switch
  that table to manual enable (README §4).

## 9. References

- Files: `server/migrations/007_source.sql`, `server/src/auth.rs`,
  `client/src/scripts/auth.ts`, `directus/setup-backoffice.mjs`, `directus/README.md`.
- Related: [0007](../0007-backoffice-directus/spec.md), [0019](../0019-admin-bonus-credits/spec.md).
