# 0109 — Downloadable invoices (B2C + B2B) and electronic invoicing

| | |
|---|---|
| **Status** | Phase 1 implemented · Phase 2 parked (2026-08-07) |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-08-07 |
| **Shipped** | — |
| **Version** | — |
| **Commits** | — |
| **Depends on** | [0106](../0106-voxtranslate-for-business/spec.md) |

## 1. Context & Problem

Both customer segments pay us today, and neither can obtain a document they can put
in their accounting:

- **B2C (consumers)** buy prepaid credit packages through Stripe Checkout in
  `mode=payment` (`stripe_handler::create_checkout_session`). The session is created
  **without `invoice_creation[enabled]`**, so Stripe produces a *payment receipt*
  only — no Invoice object, no invoice number, no PDF, nothing listed under a
  customer. There is also no `stripe_customer_id` persisted for consumer accounts,
  so there is nothing to attach a history to.
- **B2B (organizations)** are on Stripe subscriptions (`mode=subscription`) plus
  one-off credit top-ups. Subscriptions *do* generate real Stripe Invoices, and
  `organizations.stripe_customer_id` is already stored (migration 017), so the
  documents exist — they are simply never surfaced in the Dashboard. The only route
  to them today is the Stripe Billing Portal (`create_portal_session`), which is
  off-product and not discoverable as "download my invoices".

A payment receipt is not an invoice. For a business customer it is not a deductible
document, and for VAT purposes it does not stand in for one.

Separately, the goal asks for **fattura elettronica** (Italian SDI / FatturaPA XML).
That is not a rendering problem — it is a fiscal-identity problem, and it is gated on
a decision that does not live in this codebase. See §8.

## 2. Goals / Non-Goals

**Goals**

- Every paid transaction — consumer credit purchase, org subscription renewal, org
  top-up — produces a numbered invoice document that the payer can download as PDF.
- A consumer can list and download their invoices from the client app.
- An org admin can list and download the organization's invoices from the Dashboard,
  without leaving the product for the Stripe portal.
- Invoices are listed grouped by month, newest first, and each carries: number, issue
  date, period, net / VAT / gross totals, currency, status.
- Billing identity (address, and VAT / tax id for business buyers) is collected at
  purchase time so the invoice is issued to the right legal person.
- The document source sits behind one abstraction, so an SDI e-invoice path or a
  merchant-of-record path can be added without touching the API or the UI.

**Non-Goals**

- Generating our own PDF layout. Stripe's hosted invoice PDF is the artefact; we do
  not re-render it.
- Any tax *calculation* logic of our own. Rates and VAT treatment come from Stripe
  Tax, not from code we write.
- Credit notes / refund documents (follow-up).
- Dunning, retries, or payment-failure flows — untouched.

## 3. Requirements

- **R1 — Consumer purchases produce an invoice.** As a consumer, I want each credit
  purchase to generate an invoice, so that I have a fiscal document.
  - *Given* I complete a Checkout Session for a credit package, *when* the payment
    succeeds, *then* Stripe issues an Invoice for it and the server persists its id,
    number, issue date, totals and hosted PDF URL.
  - *Given* the same webhook event is delivered twice, *when* it is processed,
    *then* exactly one invoice row exists (idempotent on Stripe invoice id).

- **R2 — Consumer can list and download.** As a consumer, I want my invoices in the
  app, so that I do not have to search my email.
  - *Given* I have ≥1 invoice, *when* I open the billing section, *then* I see them
    grouped by month with number, date, gross total and a download action.
  - *Given* I click download, *when* the request is authorised, *then* I receive the
    PDF for **my own** invoice and never another user's.

- **R3 — Org admin can list and download from the Dashboard.** As an org admin, I
  want the organization's invoices in the Dashboard, so that accounting can pull them
  monthly.
  - *Given* my org has a `stripe_customer_id`, *when* I open Dashboard → Billing →
    Invoices, *then* I see every subscription invoice and every top-up invoice.
  - *Given* I am a member but not an admin/owner, *when* I request the org invoice
    list, *then* the request is rejected with 403.

- **R4 — Billing identity is captured.** As a business buyer, I want my company name,
  address and VAT number on the invoice, so that it is deductible.
  - *Given* I start a checkout, *when* the session opens, *then* it collects a
    billing address and offers tax-id entry.
  - *Given* I entered a VAT id, *when* the invoice is issued, *then* the VAT id
    appears on the PDF.

- **R5 — Monthly access.** As either segment, I want the current month's documents
  available without contacting support.
  - *Given* an invoice was issued at any point in month M, *when* I open the list at
    any later time, *then* it appears under M and stays downloadable.

- **R6 — Failure is visible, not silent.** As an operator, I want invoice creation
  failures surfaced.
  - *Given* Stripe issues the payment but no invoice can be resolved, *when* the
    webhook is processed, *then* credits are still granted and the gap is logged at
    `error` with the event id — crediting must never depend on invoicing.

## 4. Design & Architecture

**Components / files**

| Module | Responsibility |
|---|---|
| `server/src/stripe_handler.rs` | Add `invoice_creation[enabled]=true`, `billing_address_collection=required`, `tax_id_collection[enabled]=true`, `customer_creation=always` to consumer + org top-up checkouts. Add `list_invoices(customer_id)` and `get_invoice(invoice_id)` REST calls. |
| `server/src/invoices.rs` *(new)* | `InvoiceProvider` trait + `StripeInvoices` implementation. Persist / read `invoices` rows. Single seam for a future SDI or MoR provider. |
| `server/src/api.rs` | New handlers: consumer list + download; org list + download. Extend both webhooks to record invoices. |
| `server/migrations/052_invoices.sql` *(new)* | `invoices` table + `users.stripe_customer_id`. |
| `client/src/scripts/…` | Consumer invoice list UI. |
| `dashboard/src/pages/[lang]/…` | Dashboard → Billing → Invoices. |

**Data model** — `invoices` (idempotency key is `stripe_invoice_id`):

| Column | Type | Notes |
|---|---|---|
| `id` | `UUID PK` | |
| `user_id` | `UUID NULL` | set for consumer invoices |
| `org_id` | `UUID NULL` | set for org invoices; exactly one of the two is non-null |
| `stripe_invoice_id` | `TEXT UNIQUE NOT NULL` | idempotency |
| `number` | `TEXT` | Stripe's invoice number |
| `issued_at` | `TIMESTAMPTZ NOT NULL` | drives the monthly grouping |
| `period_start` / `period_end` | `TIMESTAMPTZ NULL` | subscriptions only |
| `subtotal_cents` / `tax_cents` / `total_cents` | `BIGINT NOT NULL` | |
| `currency` | `TEXT NOT NULL` | |
| `status` | `TEXT NOT NULL` | mirrors Stripe (`paid`, `open`, `void`, …) |
| `hosted_invoice_url` / `invoice_pdf_url` | `TEXT` | short-lived; re-resolved on download |
| `created_at` | `TIMESTAMPTZ NOT NULL DEFAULT now()` | |

Also `ALTER TABLE users ADD COLUMN IF NOT EXISTS stripe_customer_id TEXT` — consumers
have no customer today, and without one there is no invoice history to attach.
Migration is idempotent per the project rule.

**Protocol / API**

| Method | Path | Auth | Returns |
|---|---|---|---|
| `GET` | `/api/billing/invoices` | user session | `{ months: [{ month: "2026-08", invoices: [...] }] }` |
| `GET` | `/api/billing/invoices/{id}/pdf` | user session, owner-only | `{ url }` — freshly resolved from Stripe |
| `GET` | `/api/business/organizations/{org_id}/invoices` | org admin/owner | same shape as above |
| `GET` | `/api/business/organizations/{org_id}/invoices/{id}/pdf` | org admin/owner | `{ url }` |

The stored `invoice_pdf_url` is **not** what the browser receives: Stripe's PDF links
expire, so the download endpoint re-fetches the invoice from Stripe and returns the
current URL. That keeps the authorisation check on our side and means no expiring link
is ever rendered into the DOM. Not found and not yours both answer `404`, so invoice
ids cannot be probed for existence.

**Sequence (consumer purchase)**

1. `POST /api/billing/checkout` → Checkout Session created with invoicing + address +
   tax-id collection enabled.
2. User pays. Stripe finalises an Invoice for the session.
3. `checkout.session.completed` → credits granted (unchanged, first), then the
   session's `invoice` id is resolved and an `invoices` row is upserted.
4. `invoice.payment_succeeded` (already subscribed for org billing) upserts the same
   row for renewals, so subscription periods appear without any polling.
5. User opens billing → `GET /api/billing/invoices` → grouped list.
6. User clicks download → server re-resolves the PDF URL from Stripe → `{ url }` →
   the client opens it in a new tab.

**Key decisions**

- *Stripe Invoicing as the document source, not a home-grown PDF.* Rationale:
  sequential numbering, VAT lines, retention and localisation are already solved and
  are exactly the parts that are legally load-bearing. Rejected: rendering our own
  PDF — it looks like a week of work and is a permanent liability.
- *Provider behind a trait from day one.* Rationale: the SDI path (§8) is a different
  document pipeline entirely, and a merchant-of-record migration would replace the
  source wholesale. The API surface and both UIs must not move when that happens.
- *Crediting never depends on invoicing.* Rationale: a Stripe invoicing hiccup must
  not cost a paying user their credits. Invoice persistence is a second, non-fatal
  step in the webhook.
- *Stripe-direct with an Italian VAT entity, NOT a merchant of record.* Decided
  2026-08-07. The alternative (Paddle / Lemon Squeezy / Polar becoming the legal
  seller) would have removed both the invoicing and the SDI obligation at ~5% of
  revenue; it was rejected in favour of keeping the billing relationship. The
  consequence is accepted deliberately: EU VAT/OSS and Italian SDI obligations
  fall on us, and Phase 2 below is therefore in scope rather than optional.
- *`{ "url": … }` instead of a 302 on the PDF endpoints.* Rationale: both callers
  are cross-origin SPAs sending a bearer token. A redirect would either drop the
  Authorization header or land the browser on a Stripe URL it cannot read
  cross-origin. Returning the URL matches the existing checkout/portal endpoints,
  and the URL is resolved per click so an expiring link is never rendered into
  the DOM. No PDF transits or is cached by our server either way.
- *Subscription grants keyed on CONFIGURED price ids, not on Stripe's
  `subscription` field.* Rationale: enabling `invoice_creation` made top-up
  invoices reach the same webhook branch as renewals, and the old
  "unrecognised price → assume monthly plan" fallback would have granted a free
  month of credits per top-up. Matching on the price ids we configure keeps the
  decision on data we own; Stripe has moved the subscription link between API
  versions more than once. An unrecognised price on an invoice that *does* look
  like a subscription is logged at `error` rather than silently granted.

## 5. Implementation

All Phase 1 slices are implemented.

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Migration: `invoices` table + `users.stripe_customer_id` | `server/migrations/052_invoices.sql` |
| S1 | Checkout params: invoice creation, address, tax id, customer | `server/src/stripe_handler.rs` |
| S2 | `InvoiceProvider` trait + `StripeInvoices`; list/get Stripe calls | `server/src/invoices.rs`, `server/src/stripe_handler.rs` |
| S3 | Webhook persistence (consumer + org), idempotent, non-fatal | `server/src/api.rs` |
| S3b | Guard the org subscription grant against top-up invoices | `server/src/business/billing.rs` |
| S4 | Consumer endpoints: list + PDF URL | `server/src/api.rs`, `server/src/lib.rs` |
| S5 | Org endpoints: list + PDF URL, admin-gated | `server/src/business/billing.rs`, `server/src/business/routes.rs` |
| S6 | Consumer UI: an Invoices tab in Account → Billing, grouped by month | `client/src/scripts/app.ts`, `client/src/scripts/auth.ts`, `client/src/pages/index.astro` |
| S7 | Dashboard UI: an Invoices section on Credits & billing | `dashboard/src/pages/[lang]/credits.astro`, `dashboard/src/lib/api.ts` |
| S8 | i18n strings for both surfaces | `client/src/scripts/i18n/*.json` (84), `dashboard/src/i18n/*.json` (5) |

## 6. Testing & Verification

Tests were written before each slice (strict TDD). All are green.

Unit — `server/src/stripe_handler.rs` (3), `server/src/invoices.rs` (7):

- **S1** pins that consumer checkout sends `invoice_creation[enabled]=true` — the one
  param whose absence *was* the entire bug, and which no integration test can catch
  without live Stripe. Also pins the two mutually exclusive customer branches
  (`customer_creation=always` vs `customer` + `customer_update[address]`).
- **S1** pins that org top-ups are invoiced too and keep their credit metadata.
- **R5** grouping: three months → three groups in order; a month boundary splits
  even hours apart; an empty list yields no months; a non-contiguous list is NOT
  merged (that would mean `list` stopped sorting).
- Stripe-invoice parsing: `finalized_at` wins over `created` as the fiscal date; the
  newer `total_taxes[]` shape is read as well as the flat `tax`; a sparse invoice
  degrades to defaults rather than being dropped; no id → dropped.

Integration — `server/tests/invoices.rs` (8), DB-gated:

- **R1** a replayed invoice event (`finalized` → `payment_succeeded` → `finalized`)
  leaves exactly one row.
- **Regression** a top-up invoice records the document and grants **no** subscription
  credits, while a real renewal invoice still grants its full monthly allowance.
- **R2/R5** a consumer sees only their own invoices, grouped newest month first.
- **R2** another user's invoice and a random id both answer `404` — ids are not
  enumerable. Unauthenticated → `401`.
- **R3** a plain org member gets `403`; an owner gets the list. A non-member gets
  `404` (org existence is not confirmed), and an admin asking under their own org for
  another org's invoice id gets `404`.

Run DB-gated tests against a Postgres **with pgvector** (migration 030 needs it);
plain `postgres:16` fails the migration, and `setup()` swallows that into a silent
skip:

```
docker run -d --rm --name vox-pg -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=voxtest \
  -p 5433:5432 pgvector/pgvector:pg16
DATABASE_URL=postgres://postgres:postgres@localhost:5433/voxtest cargo test
```

Still to do manually: Stripe test mode end-to-end — one consumer purchase and one org
renewal — verifying the PDF opens and carries the VAT id when supplied.

## 7. Deployment & Operations

- Migration `052` is additive and idempotent; no backfill of historical consumer
  purchases is possible (Stripe cannot retroactively invoice a past charge) — this is
  stated to the user in the empty state.
- Stripe Dashboard: invoice template, numbering scheme, and Stripe Tax must be
  configured before rollout; without Stripe Tax the PDF carries no VAT line.
- Webhook subscriptions: add `invoice.payment_succeeded` / `invoice.finalized` to the
  **consumer** endpoint (the org endpoint already has the former).
- No new environment variables for Phase 1.

## 8. Risks / Open Items

### Phase 2 — fattura elettronica (SDI). PARKED 2026-08-07.

**Why parked:** the owner operates as a sole trader and has no registered company,
so prerequisite 1 below — the fiscal identity to issue *from* — does not exist yet.
Phase 1 stands on its own and is unaffected: it produces numbered, VAT-capable
invoices whoever the seller turns out to be, and the `InvoiceProvider` seam means
resuming Phase 2 adds an implementation rather than reworking the API or the UI.

**Revisit when** the selling entity is registered, or when Italian B2B customers
start asking — whichever comes first. Note that a P.IVA holder invoicing Italian
customers is *already* required to go through the SDI (including under regime
forfettario since January 2024), so registering the entity and needing Phase 2 are
the same event, not two separate ones.

Stripe does not emit FatturaPA XML and does not transmit to the SDI, so Phase 2 is a
second [`InvoiceProvider`] implementation in front of a certified intermediary
(Fatture in Cloud, Aruba, TeamSystem, ACube, …). It needs, in this order:

1. **Fiscal identity of the seller** — P.IVA, codice fiscale, REA, regime. The Terms
   currently name a natural person, not a company; until the selling entity is
   registered there is nothing to issue an invoice *from*.
2. **Buyer fiscal data**, which we do not collect today: `codice fiscale` for
   consumers, P.IVA + `codice destinatario` (or PEC) for businesses. Checkout now
   collects a billing address and a tax id, which covers the business case
   partially; `codice destinatario` has no field anywhere yet.
3. **Intermediary choice + credentials**, and the `nature`/exemption codes per
   transaction type (domestic, EU B2B reverse charge, extra-EU).
4. **B2C via SDI**: `codice destinatario` `0000000` plus the buyer's codice fiscale,
   and the seller must still hand the consumer a readable copy — which the Phase 1
   PDF already does.

Until all four exist, Phase 1 is what ships: numbered, VAT-bearing invoices that a
customer can download. That is a real fiscal document; it is not a fattura
elettronica, and Italian B2B customers will still ask for one.

### Other open items

- **EU VAT is not configured by this change.** Stripe Tax must be switched on in the
  Stripe Dashboard or every PDF carries a zero VAT line. Code cannot fix that.
- Stripe hosted PDF URLs expire; the redirect design assumes an available Stripe API
  at download time. A Stripe outage means no downloads (acceptable: no data loss).
- Consumer purchases made before this ships will never have an invoice — Stripe
  cannot retroactively invoice a settled charge. The empty state says so.
- `InvoiceProvider::fetch_recent` is implemented but not yet wired to a backfill
  endpoint; it exists so an owner whose webhook was missed can be repaired without
  a schema change.
- The client ships 84 UI locales and the completeness test requires exact key
  parity, so all 84 carry the four new strings. The Dashboard's 5 locales carry the
  full invoices block.

## 9. References

- Files: `server/src/stripe_handler.rs`, `server/src/billing.rs`, `server/src/api.rs`,
  `server/migrations/017_org_subscriptions.sql`, `dashboard/src/pages/[lang]/credits.astro`
- Spec: [0106 — VoxTranslate for Business](../0106-voxtranslate-for-business/spec.md)
- External: Stripe Checkout `invoice_creation`, Stripe Tax, Stripe Billing Portal
