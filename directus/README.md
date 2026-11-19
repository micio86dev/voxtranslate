# VoxTranslate Backoffice (Directus)

A [Directus](https://directus.io) Data Studio over the **same Supabase Postgres**
the app uses. It's the admin UI for **moderation** (reports, bans), **users &
billing** (read), and **multilingual content** (UI strings + legal pages) with
Directus's native Translations interface.

**Design rule:** money and ban logic stay behind the VoxTranslate server. Directus
*reads* the data and *edits content* directly, but privileged actions (ban,
credit, resolve report, GDPR delete) run by pressing a Flow button that calls the
server's `/api/admin/*` endpoints with the shared `ADMIN_API_SECRET`. So a Directus
misconfiguration can never silently rewrite a balance.

---

## 0. Prerequisites

- The app's migration `003_backoffice.sql` has run on the database (it runs
  automatically when the server boots). This creates `languages`, `i18n_strings`,
  `i18n_translations`, `legal_pages`, `legal_translations`, `blocklist_terms`,
  the `reports` lifecycle columns, and `admin_audit`.
- A strong shared secret. Generate one: `openssl rand -hex 32`.

## 1. Set the shared admin secret on the server

On the **voxtranslate-server** (Railway) service, set:

```
ADMIN_API_SECRET = <the openssl value>
```

Redeploy the server so it picks it up. Until this is set, every `/api/admin/*`
endpoint returns `403` (admin disabled) — which is the safe default.

## 2. Seed the database

Run these in the **Supabase SQL editor** (or psql), in this order:

1. `seed-content.sql` — the 8 UI languages and the 3 legal pages (English).
2. `seed-i18n.sql` — every bundled UI string in all 8 languages.

Both are idempotent (re-running only upserts). Regenerate them from source if the
strings change:

```bash
node directus/gen-content-seed.mjs > directus/seed-content.sql
node directus/gen-i18n-seed.mjs    > directus/seed-i18n.sql
```

> The client treats DB rows as **overrides over its bundled defaults**, so the
> app keeps working even with an empty content table — seeding just makes the
> strings editable in Directus.

## 3. Run Directus

### Local
```bash
cd directus
cp .env.example .env      # fill in the values
docker compose up
```
Open <http://localhost:8055>, sign in with `ADMIN_EMAIL` / `ADMIN_PASSWORD`.

### Railway (production)
Create a new service from the Docker image **`directus/directus:11`** and set the
env vars from `.env.example` (use the **same** `DATABASE_URL` Session-pooler string
as the server, and the **same** `ADMIN_API_SECRET`). Also set:

```
FLOWS_ENV_ALLOW_LIST = ADMIN_API_SECRET,VOX_API_URL
DB_SSL__REJECT_UNAUTHORIZED = false
PUBLIC_URL = https://<your-directus-domain>
```

Directus creates its own `directus_*` tables on first boot (alongside the app
tables — they don't conflict).

## 4. Add the app tables as collections

In **Settings → Data Model**, Directus lists the existing database tables. Add
(enable) these as collections:

- **Content:** `languages`, `i18n_strings`, `i18n_translations`, `legal_pages`,
  `legal_translations`, `blocklist_terms`
- **Moderation/ops (read):** `reports`, `users`, `credit_transactions`,
  `usage_sessions`, `admin_audit`
- **Bug reports (spec 0071):** `bug_reports` — user-submitted problem reports.
  **`setup-backoffice.mjs` registers this collection (under the *moderation*
  folder) and configures the `status` dropdown automatically** — just re-run it
  (§6) and the collection appears under **Content → moderation**. New rows arrive
  as `received` (other values `cancelled` · `resolved`) and are also emailed to
  `BUG_REPORT_TO`; read the `message`/`email`/`page_url`/`user_agent` columns for
  triage. A full admin already has update + delete; grant them explicitly only if
  you triage from a non-admin role.

## 5. Wire the Translations interfaces (the multilingual part)

- **i18n_strings:** edit the `key` field's relation so `i18n_translations` is the
  translations collection; on `i18n_strings` add a *Translations* interface field
  pointing at `i18n_translations` with `language` as the language field (related
  to `languages.code`). You now edit every string across all 8 languages in one
  panel.
- **legal_pages:** same idea with `legal_translations` (fields `title`, `body`;
  set `body` to a **Markdown** interface). Add other languages here — the client
  shows the requested language, falling back to English.

## 6. Permissions (scope the admin role)

Give your admin/editor role:

- **Full CRUD** on the content collections (`languages`, `i18n_*`, `legal_*`,
  `blocklist_terms`).
- **Read-only** on `reports`, `users`, `credit_transactions`, `usage_sessions`,
  `admin_audit`. Do **not** grant update on `users.balance` / `users.banned_until`
  — those change only through the Flows below.

## 7. Flows — the privileged action buttons

Create the **Flows** below (Settings → Flows), each a **Manual** trigger on a
collection, with a **Webhook / Request URL** operation. Use
`{{$env.ADMIN_API_SECRET}}` for the header and `{{$env.VOX_API_URL}}` for the base.

For every webhook operation: **Method** `POST`, **Header**
`X-Admin-Secret: {{$env.ADMIN_API_SECRET}}`, **Body** as below. The manual trigger
exposes the selected row id as `{{$trigger.keys[0]}}`; add confirmation fields for
the inputs (days, reason, amount, …). Set `actor` to `{{$accountability.user}}`.

| Flow | Trigger collection | URL | Body |
|------|--------------------|-----|------|
| **Ban user** | `users` | `{{$env.VOX_API_URL}}/api/admin/ban` | `{ "user_id": "{{$trigger.keys[0]}}", "days": {{days}}, "reason": "{{reason}}", "actor": "{{$accountability.user}}" }` |
| **Unban user** | `users` | `{{$env.VOX_API_URL}}/api/admin/unban` | `{ "user_id": "{{$trigger.keys[0]}}", "actor": "{{$accountability.user}}" }` |
| **Adjust credits** | `users` | `{{$env.VOX_API_URL}}/api/admin/credit` | `{ "user_id": "{{$trigger.keys[0]}}", "amount": {{amount}}, "reason": "{{reason}}", "actor": "{{$accountability.user}}" }` |
| **Gift bonus** | `users` | `{{$env.VOX_API_URL}}/api/admin/bonus` | `{ "user_id": "{{$trigger.keys[0]}}", "amount": {{amount}}, "message": "{{message}}", "actor": "{{$accountability.user}}" }` |
| **Gift subscription** | `organizations` | `{{$env.VOX_API_URL}}/api/admin/org/gift-subscription` | `{ "org_id": "{{$trigger.keys[0]}}", "plan": "{{plan}}", "months": "{{months}}", "credits": "{{credits}}", "message": "{{message}}", "actor": "{{$accountability.user}}" }` |
| **Resolve report** | `reports` | `{{$env.VOX_API_URL}}/api/admin/report/resolve` | `{ "report_id": "{{$trigger.keys[0]}}", "action": "{{action}}", "note": "{{note}}", "actor": "{{$accountability.user}}" }` |
| **GDPR delete** | `users` | `{{$env.VOX_API_URL}}/api/admin/user/delete` | `{ "user_id": "{{$trigger.keys[0]}}", "actor": "{{$accountability.user}}" }` |

`action` for Resolve report is `resolved` or `dismissed`.

**Gift subscription** runs on the **`organizations`** collection (a B2B org is the
subscription holder — `plan`, `credits_balance` and `subscription_*` live on the
org row, never on a user). It puts the org on an **active**, non-renewing
**Business** or **Enterprise** plan for `months` months (default **1**) and tops
up the org credit pool. `credits` defaults to the plan's monthly package amount ×
months when **left blank** (Business 1000 / Enterprise 5000 per month, env-tunable
via `ORG_CREDITS_BUSINESS_MONTHLY` / `ORG_CREDITS_ENTERPRISE_MONTHLY`), so the
common case — "gift one month of Business with the standard credits" — is just
*pick the plan and confirm*. The gift sets `current_period_end` so it **auto-expires**
(no Stripe sub backs it; the server's recording gate stops unlocking when the month
lapses), and it **refuses with `409`** when the org already has a managed Stripe
subscription, so a gift can't desync live billing. Re-gifting *extends* the period.
The `months`/`credits` inputs are sent quoted and parsed leniently, so blank = use
the default. **Gift bonus** (issue #11) grants a positive USD bonus to the user and
emails them a notification
(best-effort — needs `RESEND_*` configured; the response's `email_sent` flag and
the `admin_audit` detail record whether it went out). `amount` must be positive;
`message` is an optional note shown in the email. Use **Adjust credits** instead
for refunds/manual corrections (it takes a signed amount, no email). Every call
writes an `admin_audit` row, so the `admin_audit` collection is your full action
history.

> **Reproducible setup for Gift bonus.** Rather than building the flow by hand,
> create it through the Directus REST API with `setup-bonus-flow.mjs` (idempotent
> — a no-op once the flow exists). It clones the trigger config of an existing
> manual `users` flow so the button lands in the same place as the others. A
> Directus Flow lives in Directus's own tables (not the app schema), so this is a
> setup *script*, not an app `.sql` migration. Run:
>
> ```bash
> DIRECTUS_URL=https://<your-directus> \
>   DIRECTUS_ADMIN_EMAIL=… DIRECTUS_ADMIN_PASSWORD=… \
>   node directus/setup-bonus-flow.mjs
> # or, instead of email+password: DIRECTUS_TOKEN=<static admin token>
> ```

> **Reproducible setup for Gift subscription.** Same idea, on the `organizations`
> collection, via `setup-gift-subscription-flow.mjs` (idempotent; repairs in place
> if it already exists). Run `setup-backoffice.mjs` (§9) **first** so `organizations`
> is a registered collection — the manual button only shows on a managed collection.
>
> ```bash
> DIRECTUS_URL=https://<your-directus> \
>   DIRECTUS_ADMIN_EMAIL=… DIRECTUS_ADMIN_PASSWORD=… \
>   node directus/setup-gift-subscription-flow.mjs
> # or, instead of email+password: DIRECTUS_TOKEN=<static admin token>
> ```

## 9. KPI dashboards + collections in one shot (`setup-backoffice.mjs`)

Rather than enabling collections and building Insights panels by hand, provision the
**whole backoffice** through the REST API (spec
[0051](../specs/0051-directus-backoffice/spec.md)). Idempotent — re-running refreshes
collection metadata and rebuilds every dashboard's panels:

```bash
DIRECTUS_URL=https://<your-directus> \
  DIRECTUS_ADMIN_EMAIL=… DIRECTUS_ADMIN_PASSWORD=… \
  node directus/setup-backoffice.mjs
# or, instead of email+password: DIRECTUS_TOKEN=<static admin token>
```

It does two things:

1. **Data model** — registers the app tables as collections, grouped into six
   folders (**accounts · business · sessions · moderation · ai_features · content**)
   with icons, colours and display templates, so the admin reads like a product, not
   raw SQL. The **business** folder (spec 0106) carries `organizations`,
   `organization_members`, `organization_invites`, `projects`,
   `organization_credits_transactions` and `audit_logs` — registering `organizations`
   is what gives the **Gift subscription** Flow button (§7) a home.
2. **Insights** — four dashboards of KPI panels:
   - **📊 Overview** — users (total / new 30d / banned), calls (total / live),
     translations, chat, files; new-users and calls per day.
   - **💳 Billing & Stripe** — **Revenue** (sum of `purchase`), purchases, revenue
     30d, avg purchase; **promo credits granted** (`free_credit`+`bonus`, the cost of
     acquisition), bonus, credits spent (`usage`), outstanding balance, AI charges;
     revenue/day series; **recent Stripe events** (movements log) + recent
     transactions. *Note:* "Revenue" = completed Stripe purchases (the webhook only
     handles `checkout.session.completed`; refunds aren't recorded), and "spese"
     means promotional credit granted — infra costs (Deepgram/Groq/Railway) are
     external to the DB.
   - **🛡️ Moderation** — reports (open / resolved / dismissed), active bans,
     blocklist, audit count; reports/day; open-reports + recent-actions lists.
   - **🚀 Acquisition & Features** — attributed vs organic users, a **users-by-source**
     bar chart, and feature usage (glossaries, bookmarks, AI reports, sentiment,
     emails).

> Run-and-iterate: like `setup-bonus-flow.mjs`, this can't be CI-tested against a
> live Directus. Each panel is created independently — a panel that a given Directus
> version rejects is logged and skipped (the rest still provision). Paste any
> `!`-prefixed warnings back and we adjust.

### Marketing attribution (`users.source`)

Migration `007` adds `users.source`: the `?source` (or `utm_source` / `ref`) the
visitor arrived with, captured client-side and stamped on the account at **first
login only** (first-touch). Tag a campaign by linking with the param, e.g.
`https://voxtranslate.app/?source=reddit-launch` — every account created in that
session is attributed to `reddit-launch`, and rolls up in the **Users by source**
chart. Add `users.source` to the read columns when you scope the admin role (§6).

## Visual walkthrough

Screenshots of the actual screens (in `directus/screenshots/`):

1. **Data Model** (`1-data-model.png`) — your DB tables ready to enable as collections.
2. **Flows → Create Flow** (`2-flows.png`).
3. **Flow Setup** (`3-flow-setup.png`) — name the flow (e.g. "Ban user").
4. **Trigger Setup** (`4-flow-trigger.png`) — pick **Manual** (runs from a button on
   the selected collection); then add a **Webhook / Request URL** operation with the
   values from the table in §7.

## 8. Verify end-to-end

1. Edit a UI string (e.g. `connect`) in Directus → reload the app → the new label
   shows (the client merges `/api/content/i18n` over its bundled defaults).
2. Open `/terms` in the app → the body comes from `legal_translations`
   (`/api/content/legal/terms`).
3. From a test user row, press **Ban user** → that user can no longer join; the
   `admin_audit` collection shows the action.
4. Add a term in `blocklist_terms` → restart the server → that word is filtered
   in transcripts/chat.

## 10. User map (opt-in location) — superadmin only

A filterable map of every user who opted into location sharing. The data layer is
already built by the server; this is a one-time Directus setup.

**How the data gets there.** The frontend shows an optional "share your location to
help us improve the service" prompt (never required). On accept it POSTs to
`/api/user/location`, which stores `latitude`/`longitude`/`location_consent`/
`location_updated_at` on the `users` row (migration `027_user_location.sql`).
On boot the server also runs `location::ensure_geometry` (best-effort): it enables
PostGIS and adds a generated `users.location` `geometry(Point,4326)` column kept in
sync from lat/lng — exactly what the Directus **Map** layout needs. (Supabase Postgres
ships PostGIS, so this just works; on a DB without it the server logs a warning and
skips the geometry column, and you'd fall back to the raw lat/lng numbers.)

**Set it up in Directus:**

1. **Settings → Data Model → `users`** → if `location`, `latitude`, `longitude`,
   `location_consent` aren't listed yet, click **⋯ → Import fields from database**
   (Directus reads new columns from the live schema). The `location` column shows up
   as a **Map / Geometry** field.
2. Open the **`users` collection** and switch the layout (top-right) to **Map**.
   Pick `location` as the geometry field. Each consenting user becomes a pin.
3. **Filter** (so you only plot people who actually opted in, and slice the view):
   - `location_consent` **equals** `true`
   - optionally `location_updated_at` within the last N days (recency)
   - optionally join/scope by org for B2B drill-downs.
4. **Basemap (optional, nicer tiles):** Settings → Project Settings → **Map** →
   add a **Mapbox** access token. Without it Directus uses its built-in basemap.
5. **Permissions:** this is a **superadmin** view — keep the `location*` fields readable
   only by the admin/superadmin role (Settings → Roles), not by any B2B-scoped role,
   so customer admins never see end-user coordinates.

Withdrawing consent (the app's DELETE `/api/user/location`, or a future settings
toggle) nulls the coordinates and flips `location_consent` to false, so the pin
disappears on the next load.

## 11. Live Connect administration

Live Connect (spec 0121) is administered here by platform superadmins and a few
authorised operators. Directus stays a **thin admin UI**: it never writes the
`live_*` tables, every action is a Flow button that calls the VoxTranslate server
(`/api/admin/live/*`, the shared secret from §1), and the server writes an
`admin_audit` row for it. Customer-facing workflows — desks, QR download and print,
session history, search, usage — stay in the B2B dashboard. Directus and the
dashboard do **not** share an authorization model.

### Provision it

Needs Directus **11** (permissions hang off *policies*), the §7 env vars
(`ADMIN_API_SECRET`, `VOX_API_URL` in `FLOWS_ENV_ALLOW_LIST`), and
`setup-backoffice.mjs` (§9) run first so `organizations`, `projects` and
`organization_credits_transactions` are registered collections.

```bash
# See every call first. Needs no Directus, no URL, no credentials, changes nothing:
node directus/setup-live-connect.mjs --dry-run

DIRECTUS_URL=https://<your-directus> \
  DIRECTUS_ADMIN_EMAIL=… DIRECTUS_ADMIN_PASSWORD=… \
  node directus/setup-live-connect.mjs
# or, instead of email+password: DIRECTUS_TOKEN=<static admin token>
```

Idempotent: a re-run updates in place (a second run changes no counts). It creates
the *Live Connect* folder, the five collections below (four tables of the live feature and the `live_desk_usage_daily` rollup), two roles (nothing else in
this repo creates roles), five Flows and one dashboard. It creates **no users**:
add operators in Directus and give each ONE of the two roles. Flags:
`--no-transcript-preview` (see Privacy), `--print-transcript-filter` (see below).

### What each role can do

| | Live Connect Support | Live Connect Privacy |
|---|---|---|
| Read `live_desks` | name, location, language, status, token *version*, dates | same |
| Read `live_sessions` | status, languages, end reason, timings, owner/org/project/desk ids, participants (below), events (below) | same |
| Read `live_participants` | role, language, consent and join/leave times | same |
| Read `live_session_events` | kind and time | same |
| Read `live_desk_usage_daily` | desk, day, number of conversations, credits (no names, no text) | same |
| Org ledger | amount and date of `type = live_connect` rows only | same |
| Insights | the *Live Connect - Operations* dashboard | same |
| Buttons | Revoke desk, Regenerate desk, End session, Reconcile session | those four **+ View transcript** |
| Write to any `live_*` table | no | no |

### What is deliberately NOT exposed

* **The QR.** It is derived from a server secret and never stored. Support sees the desk's
  `token_version` but not `public_id` (part of the URL). **Regenerate desk never shows the
  new QR**: the organisation downloads it from its own dashboard, so an operator never
  holds a credential.
* **Tokens.** `live_sessions.invite_hash`, `live_participants.token_hash` (SHA-256
  fingerprints) and `call_sessions` pointers are hidden and absent from both roles.
* **People.** `live_participants.display_name` (guest and staff names) is personal data:
  admin-only, flagged in the field note, absent from Support and Privacy. User names are
  not readable either (`owner_user_id` shows as an id).
* **Conversation content.** Neither role can read `transcript_events`, `transcripts`,
  `session_participants`, `call_sessions`. The only path to it is the **View transcript**
  button, and only for the Privacy role.
* **Event details.** `live_session_events.detail` (free-form diagnostics) is admin-only.
* `live_usage_ticks` is not a Directus collection at all: its primary key is composite and
  Directus ignores tables without a single primary key. Per-session billing is verified
  with **Reconcile session**. Per-desk credits come from `live_desk_usage_daily` instead
  (below).
* **`live_desk_usage_daily`** is a rollup *table* (one row per desk and UTC day: the number of
  conversations and the organisation credits they were debited), created by migration 068
  and filled from the `live_desk_usage` view by `refresh_live_desk_usage()`; the server calls
  it from its hourly housekeeping, so a chart can be up to an hour behind (an operator can run
  `SELECT refresh_live_desk_usage();` for a fresh one). It is a table and not the view itself
  because Directus 11 only introspects base tables: a view is invisible to it (checked on
  11.17.4). It holds ids, a day and two counts, no personal data, and is read-only for both
  roles. Directus connects with the database owner's rights, so the view's `security_invoker`
  setting does not affect it; the migration also revokes both objects from the Supabase API
  roles (`anon`, `authenticated`).

"Hidden" fields are hidden from the app's forms and lists and excluded from Support and
Privacy by an explicit allow-list (a column added later stays invisible until someone
lists it). A Directus **administrator** can still read anything through the API: keep that
group small. Only administrators should be able to edit Flows: whoever edits a Flow can
read `ADMIN_API_SECRET` (§7 note) and bypass the role check inside the transcript Flow.

### Existing roles: close the live-conversation leak yourself

This script does **not** modify existing roles. But a live conversation reuses the meeting
tables, so `call_sessions`, `transcript_events`, `session_participants`, `transcripts`
(and `session_reports`, `session_sentiments`, `session_emails`, `transcript_bookmarks`,
`chat_files`) **already hold Live Connect rows and are already exposed to any role you
gave read access**. For every role that should not read live conversations, add this
permission filter to those collections:

* on `call_sessions` — a NULL `kind` is an ordinary meeting, so `_neq` alone would hide it:

  ```json
  { "_or": [ { "kind": { "_null": true } }, { "kind": { "_neq": "live_connect" } } ] }
  ```

* on each table that has a `session_id` (`transcript_events`, `session_participants`,
  `transcripts`, …) — the same condition through the relation:

  ```json
  { "session_id": { "_or": [ { "kind": { "_null": true } }, { "kind": { "_neq": "live_connect" } } ] } }
  ```

`node directus/setup-live-connect.mjs --print-transcript-filter` prints the exact JSON for
every affected collection. Also check the roles of any B2B-scoped user: the same rule
applies to them.

### Actions (Flow buttons)

Select **one** row and press the button; Revoke / Regenerate / End ask for a **reason**.
The Flow sends `actor` = your Directus email (looked up for you) and the reason to the
server, then tells you the outcome as a Directus notification (a refusal such as
`session_over`, or an unreachable server, is reported the same way). Selecting several
rows is refused.

| Button (on) | Server route | Notes |
|---|---|---|
| Revoke desk (`live_desks`) | `/api/admin/live/desk-revoke` | Same effect as the org admin's revoke |
| Regenerate desk (`live_desks`) | `/api/admin/live/desk-regenerate` | Answers version + count only, never the QR |
| End session (`live_sessions`) | `/api/admin/live/session-end` | `ended_by_admin`, finalises; 409 if already over |
| Reconcile session (`live_sessions`) | `/api/admin/live/session-reconcile` | Read-only: ticks vs ledger → `ok` / `mismatch` / `unlinked` (erased conversations cannot be proven) |
| View transcript (`live_sessions`) | `/api/admin/live/session-transcript` | Privacy role only |

### Privacy rules (S15, S17)

* **Audit.** Every button writes `admin_audit` (who, which id, the reason, your IP). The
  Directus *Admin audit* collection is the history; the audit never contains tokens, QR
  material, names or transcript text.
* **View transcript** needs a reason of at least 10 meaningful characters, and the audit
  row is written **before** anything is read: if it cannot be written, nothing is shown.
  The button is visible only to the Privacy role, and the Flow checks the role again
  itself.
* **Where the excerpt goes.** Directus cannot display a Flow's answer, so the outcome
  arrives as a notification. For this button it carries a short excerpt (3 utterances,
  120 characters each, no names). A Directus notification is stored in Directus, and is
  **e-mailed to the operator when Directus mail is configured**. If that is not
  acceptable, provision with `--no-transcript-preview`: the notification then carries
  counts only. Delete the notification when you are done either way.
* The audit trail and the excerpt are separate copies of different things: the audit says
  *that* a read happened and why, never *what* was read.

### Reading the dashboard

*Live Connect - Operations* has: open sessions now, sessions and failures in the last 7
days, expired-while-waiting (compare with the 7-day total for the rate), sessions per day,
failed/abandoned per day, sessions by end reason and by desk, org credits spent per day,
credits by desk over 30 days with a top-10 list of the busiest desk-days, and lists of failed
and recent sessions. Panels run with the **viewer's** permissions, so
a Support user can only ever chart what Support can read. Credits by desk come from
`live_desk_usage_daily` (hourly refresh); the bar chart shows every desk because Directus'
bar-chart panel has no top-N option, so the top 10 is the list next to it. The customer-facing
per-desk figures stay in the B2B dashboard's usage view.

## Endpoint reference (server)

Read (public): `GET /api/content/i18n`, `GET /api/content/legal/{slug}?lang=xx`.
Admin (require `X-Admin-Secret`): `POST /api/admin/{ban,unban,credit,bonus,report/resolve,user/delete}`,
`POST /api/admin/live/{desk-revoke,desk-regenerate,session-end,session-reconcile,session-transcript}` (Live Connect, §11),
`POST /api/admin/org/gift-subscription` (gift a B2B org a Business/Enterprise month + credits).
User location (require session JWT): `POST /api/user/location` (opt-in store), `DELETE /api/user/location` (withdraw).
