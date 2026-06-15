# 0071 — User bug/error reporting (email to admins + backoffice triage)

| | |
|---|---|
| **Status** | Draft |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-15 |
| **Shipped** | — |
| **Version** | — |
| **Commits** | — |
| **Depends on** | [0005](../0005-accounts-credits-billing/spec.md), [0007](../0007-backoffice-directus/spec.md), [0016](../0016-follow-up-email/spec.md), [0019](../0019-admin-bonus-credits/spec.md), [0028](../0028-security-hardening/spec.md), [0064](../0064-high-traffic-abuse-hardening/spec.md) |

## 1. Context & Problem

VoxTranslate users are not technical. When something breaks they have no in-product
way to tell us — they'd have to find an email address off-site, so most issues go
unreported. We want a dead-simple "something's wrong" path: one obvious button, a
free-text box, send. Each report should (a) email the admins immediately (we already
run Resend, spec 0016) and (b) land in the Directus backoffice (spec 0007) as a
trackable item with a status the team can move through and delete when done.

## 2. Goals / Non-Goals

**Goals**
- Any user — guest or signed-in — can report a problem in ≤2 taps from the app.
- Each report is persisted server-side and emailed to a configurable admin address.
- The team triages reports in Directus: status `received → cancelled | resolved`, plus delete.
- Abuse-safe (rate-limited, length-capped) and privacy-respecting (only what the user
  typed + minimal diagnostic context).

**Non-Goals**
- No screenshot/attachment capture (text only for v1).
- No in-app status tracking for the reporter (no "my tickets" view) — fire-and-forget.
- No automatic error/exception capture (this is *user-initiated*; telemetry is 0050/0063).
- No reply-from-Directus workflow (admins reply by normal email for now).

## 3. Requirements

- **R1 — Report from anywhere.** As any user, I want an obvious "Report a problem"
  control. *Given* I'm on the app (signed in or guest), *when* I open it, *then* a small
  modal with a textarea + Send appears; *when* I submit a non-empty message, *then* I get
  a clear success confirmation (and a graceful error if it fails).
- **R2 — Delivered + stored.** *Given* a submitted report, *then* the server stores a row
  in `bug_reports` and (when Resend is configured) emails it to `BUG_REPORT_TO`; storage
  succeeds even if the email send fails (email is best-effort, logged).
- **R3 — Triage context.** *Given* a report, *then* it captures: message, created_at,
  user_id + email (if signed in), page URL, and user-agent — enough to reproduce, nothing
  more. No credentials, tokens, or call content.
- **R4 — Backoffice lifecycle.** *Given* the Directus backoffice, *then* an admin sees all
  reports, can set status (`received` default → `cancelled` | `resolved`), and can delete a
  report.
- **R5 — Abuse-safe.** *Given* the endpoint, *then* submissions are rate-limited per IP and
  the message is length-capped (server-enforced), consistent with 0028/0064.
- **R6 — Localized.** *Given* the UI language, *then* the button, modal, placeholder,
  success and error strings are localized (8 languages).

## 4. Design & Architecture

- **Components / files:**
  - Client: new `client/src/scripts/bug-report.ts` (modal open/close, submit, validation
    feedback); trigger button + modal markup in `client/src/pages/index.astro`; i18n keys
    in `client/src/scripts/i18n.ts`; POST helper in `client/src/scripts/api.ts`.
  - Server: `POST /api/bug-report` handler in `server/src/api.rs`; `BUG_REPORT_TO` in
    `server/src/config.rs`; row type + insert in `server/src/db.rs`; reuse `email.rs`
    (Resend, spec 0016) for the notification; rate-limit via the existing limiter
    (0028/0064); route registered in `server/src/lib.rs`.
  - DB: `server/migrations/009_bug_reports.sql`.
  - Backoffice: Directus collection over the `bug_reports` table (spec 0007).
- **Data model — `bug_reports`:**
  - `id uuid pk default gen_random_uuid()`, `created_at timestamptz default now()`,
    `message text not null`, `status text not null default 'received'`
    (`check status in ('received','cancelled','resolved')`),
    `user_id uuid null references users(id) on delete set null`, `email text null`,
    `page_url text null`, `user_agent text null`.
  - Indexed on `status, created_at desc` for the backoffice list.
- **Protocol / API:** `POST /api/bug-report` — body `{ message: string, pageUrl?: string }`
  (user-agent read from the request header; user/email resolved from the optional auth
  token). Responses: `200 {ok:true}`; `400` empty/too-long; `429` rate-limited. No auth
  required (guests can report); an auth token, if present, attributes the report.
- **Sequence (happy path):**
  1. User clicks "Report a problem" → modal opens.
  2. User types message → Send → `POST /api/bug-report`.
  3. Server validates (non-empty, ≤ MAX), checks the per-IP rate limit.
  4. Insert row (status `received`); attribute to the user if a valid token is present.
  5. Best-effort Resend email to `BUG_REPORT_TO` (subject + message + context); log failures.
  6. `200` → client shows success, closes the modal.
  7. Admin opens Directus → `bug_reports` collection → reads, sets status, or deletes.
- **Key decisions:**
  - *Store-then-email, email best-effort* — a report is never lost to an email outage.
  - *Guests allowed* — most confused users aren't signed in; gate only with rate limits.
  - *Reuse Directus over the raw table* — no new admin API; status + delete are native
    Directus capabilities once the collection is exposed (spec 0007 pattern).
  - *`BUG_REPORT_TO` env (default the owner's address)* — configurable per environment,
    no hard-coded recipient; absent → store-only (email skipped, logged).

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Migration `009_bug_reports`; `POST /api/bug-report` (validate, rate-limit, insert, best-effort email); `BUG_REPORT_TO` config; row type | `server/migrations/009_bug_reports.sql`, `server/src/{api,config,db,lib}.rs`, `server/src/email.rs` (reuse) |
| S1 | Client modal + "Report a problem" trigger (account bar) + submit + i18n | `client/src/pages/index.astro`, `client/src/scripts/{bug-report,api,i18n}.ts` |
| S2 | Directus: expose `bug_reports` collection (status as dropdown, enable delete); document in `directus/README.md` | `directus/` |

## 6. Testing & Verification

- **Server (Rust unit/integration):** empty message → 400; over-length → 400; valid →
  inserts a row with status `received`; rate-limit returns 429 past the cap; email path is
  best-effort (insert still succeeds when Resend is unset/fails). `cargo fmt` + clippy.
- **Client (vitest):** message validation (non-empty, length cap) as a pure helper;
  success/error UI state transitions where testable.
- **Manual / `/run`:** submit a report as guest and as a signed-in user; confirm the row in
  Postgres, the email arrival, and that the report appears + is editable/deletable in Directus.

## 7. Deployment & Operations

- **Env:** add `BUG_REPORT_TO` on Railway (default `micio86dev@gmail.com`). Resend vars
  already set (spec 0016).
- **Migration:** `009_bug_reports.sql` runs at server startup (embedded, idempotent).
- **Server deploy is MANUAL:** `railway up` from `server/` after merge (Railway is not
  auto-deploy). Client auto-deploys on `main` via Vercel.
- **Directus:** Directus reads the same Postgres, so the table appears; expose it as a
  collection and set `status` to a dropdown (received/cancelled/resolved) + allow delete.
  Directus is org-blocked in-browser for the owner — configure via the Directus admin or a
  setup script as per the [directus runbook](../../directus/README.md).

## 8. Risks / Open Items

- **Spam / abuse:** mitigated by per-IP rate limit + length cap; consider a soft per-session
  cap and (later) a honeypot if abused.
- **User-entered PII:** the free-text box may contain personal data — covered by the same
  retention/GDPR posture as other user content (0006); don't add extra context beyond URL +
  user-agent.
- **Directus auto-discovery:** a brand-new table may need manual collection/field setup in
  Directus (S2) — flagged, not automatic.
- **Entry points:** v1 surfaces the trigger in the account bar (home). An in-call entry
  (⋯ menu) is a follow-up if wanted.

## 9. References

- Specs: [0016](../0016-follow-up-email/spec.md) (Resend), [0007](../0007-backoffice-directus/spec.md) (Directus), [0019](../0019-admin-bonus-credits/spec.md) (admin email), [0028](../0028-security-hardening/spec.md)/[0064](../0064-high-traffic-abuse-hardening/spec.md) (rate limits).
- Files: `server/src/{api,config,db,email,lib}.rs`, `server/migrations/009_bug_reports.sql`, `client/src/scripts/{bug-report,api,i18n}.ts`, `client/src/pages/index.astro`, `directus/README.md`.
