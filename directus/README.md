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
| **Resolve report** | `reports` | `{{$env.VOX_API_URL}}/api/admin/report/resolve` | `{ "report_id": "{{$trigger.keys[0]}}", "action": "{{action}}", "note": "{{note}}", "actor": "{{$accountability.user}}" }` |
| **GDPR delete** | `users` | `{{$env.VOX_API_URL}}/api/admin/user/delete` | `{ "user_id": "{{$trigger.keys[0]}}", "actor": "{{$accountability.user}}" }` |

`action` for Resolve report is `resolved` or `dismissed`. **Gift bonus** (issue
#11) grants a positive USD bonus to the user and emails them a notification
(best-effort — needs `RESEND_*` configured; the response's `email_sent` flag and
the `admin_audit` detail record whether it went out). `amount` must be positive;
`message` is an optional note shown in the email. Use **Adjust credits** instead
for refunds/manual corrections (it takes a signed amount, no email). Every call
writes an `admin_audit` row, so the `admin_audit` collection is your full action
history.

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

## Endpoint reference (server)

Read (public): `GET /api/content/i18n`, `GET /api/content/legal/{slug}?lang=xx`.
Admin (require `X-Admin-Secret`): `POST /api/admin/{ban,unban,credit,report/resolve,user/delete}`.
