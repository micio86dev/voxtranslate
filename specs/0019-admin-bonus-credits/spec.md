# 0019 — Admin Bonus Credits + Email Notification

| | |
|---|---|
| **Status** | ✅ Shipped & live |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-12 |
| **Shipped** | 2026-06-12 |
| **Version** | v1.1.0 |
| **Commits** | PR #13 (`69bbacd`) |
| **Depends on** | [0005](../0005-accounts-credits-billing/spec.md), [0007](../0007-backoffice-directus/spec.md), [0016](../0016-follow-up-email/spec.md) |

## 1. Context & Problem

The backoffice (Directus, spec 0007) already lets an admin *adjust* a user's
balance via `POST /api/admin/credit` — a signed manual correction (grant or
refund) with no user-facing side effect. Issue #11 asks for a distinct, friendly
flow: an admin **gifts a bonus** (positive USD) to a user, and the user is
**emailed a notification** that they received it.

The pieces already exist: the credit ledger (`billing.add_credits`, spec 0005),
the admin-action pattern (shared `ADMIN_API_SECRET` + `admin_audit`, spec 0007),
and the Resend transactional-email client (spec 0016). This feature composes
them into one explicit "bonus" action.

## 2. Goals / Non-Goals

**Goals**
- A backoffice action that gifts a **positive** USD bonus to a user.
- Notify the recipient by email when the bonus lands.
- Audit the action (`admin_audit`), like every other privileged action.
- Keep the grant authoritative: the email is best-effort and never blocks or
  reverses the credit.

**Non-Goals (future)**
- Refunds / negative adjustments — those stay on `/api/admin/credit` (signed).
- Bulk / scheduled / rule-based bonus campaigns.
- Localized email per user (no per-user locale is stored; the email is English).
- In-app notification (only email + the live balance on next connect).

## 3. Requirements

- **R1 — Gift a bonus.** As an admin, I want to gift a user a USD bonus, so that
  I can reward or compensate them.
  - *Given* a valid `ADMIN_API_SECRET` and `amount > 0`, *when* I `POST
    /api/admin/bonus`, *then* the user's balance increases by `amount`, a
    `bonus` ledger row is written, and an `admin_audit` row records it.
- **R2 — Notify by email.** *Given* Resend is configured, *when* a bonus is
  granted, *then* the recipient gets an email stating the amount + new balance
  (and the admin's optional note); the response's `email_sent` reflects the
  outcome.
- **R3 — Grant never depends on the email.** *Given* Resend is unconfigured or
  the send fails, *when* a bonus is granted, *then* the credit is still applied
  and the response returns `email_sent: false` (logged, non-fatal).
- **R4 — Validation & auth.** *Given* a non-positive/`NaN` amount, *then* `400`.
  *Given* a missing/invalid admin secret, *then* `403` (before any work).

## 4. Design & Architecture

- **Components / files:**
  - `server/src/admin.rs` — `bonus` handler + `send_bonus_email` (lookup contact
    → build → Resend send, best-effort) + pure `bonus_email` template +
    `html_escape`.
  - `server/src/lib.rs` — route `POST /api/admin/bonus`.
  - `directus/README.md` — the **"Gift bonus"** Flow row.
- **Protocol / API:** `POST /api/admin/bonus`
  - Auth: `X-Admin-Secret` or `Authorization: Bearer` = `ADMIN_API_SECRET`.
  - Body: `{ user_id: uuid, amount: number>0, message?: string, actor?: string }`.
  - 200 `{ ok: true, balance: "<decimal>", email_sent: bool }`; errors
    `400` (amount), `403` (auth), `503` (billing/admin unconfigured),
    `500` (grant failed).
- **Sequence:** auth → validate `amount > 0` → `add_credits(kind="bonus",
  description=message)` → look up `(email, name)` → Resend send (best-effort) →
  `admin_audit` (incl. `email_sent`) → respond.
- **Key decisions:**
  - *Dedicated `/bonus` endpoint, not a flag on `/credit`* — a bonus is a
    positive gift with a user-facing email; refunds/corrections are a different
    intent (signed, silent). Two clear actions beat one overloaded one.
  - *Email best-effort* — the money is the contract; a transient Resend failure
    must not lose or duplicate a grant. `email_sent` surfaces the outcome to the
    caller + audit.
  - *`bonus` ledger kind* — distinguishes gifts from `admin_adjust` /
    `purchase` / `usage` in `credit_transactions` for reporting.
  - *HTML-escape the admin note* — it's free text rendered into the email body.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `bonus` handler + `send_bonus_email` + `bonus_email` template + route | `admin.rs`, `lib.rs` |
| S1 | Backoffice Flow docs | `directus/README.md` |
| S2 | Tests (unit + integration) | `admin.rs`, `tests/billing.rs` |

## 6. Testing & Verification

- **Unit (`admin.rs`):** `bonus_email` renders amount/name/balance, falls back on
  an empty name, includes + HTML-escapes the admin note; `html_escape` covers
  the specials — pins R2.
- **Integration (`tests/billing.rs`, DB-gated):** a valid gift credits the
  balance, writes a `bonus` ledger row, and returns `email_sent: false` (test
  state has no Resend — pins R1/R3); non-positive amount → `400`; bad secret →
  `403` (R4).
- **Prod (v1.1.0):** verified live — route returns `403`/`400` on the guard
  paths, and a controlled real grant to a throwaway test user returned
  `email_sent: true` (Resend delivered) with the correct ledger row, then the
  test user was deleted.

## 7. Deployment & Operations

- No migration (reuses `users` / `credit_transactions` / `admin_audit`).
- The notification email needs `RESEND_*` configured (already set in prod, spec
  0016); without it the feature still grants, just `email_sent: false`.
- Backoffice: add the **"Gift bonus"** Manual Flow on `users` (see
  `directus/README.md`) posting `{ user_id, amount, message, actor }`.

## 8. Risks / Open Items

- No per-user email locale → English notification only.
- No rate-limit on bonus size/frequency beyond admin trust + the `admin_audit`
  trail; a future cap/approval step could harden against fat-finger gifts.

## 9. References

- Issue: #11
- Files: `server/src/admin.rs`, `server/src/lib.rs`, `directus/README.md`
- External: https://resend.com/docs (transactional email)
