# 0115 — Buying, verifying and keeping telephone numbers

| | |
|---|---|
| **Status** | 🚧 In progress |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-09-12 |
| **Depends on** | [0111](../0111-translated-voip/spec.md), [0112](../0112-business-phone-dashboard/spec.md) |

## 1. Context & Problem

`voip_numbers` has existed since migration 056 and **nothing in the product writes to it**.
No route creates, searches, buys, verifies or releases a number; `verification_status` is
only ever read, by `resolve_caller_id`, and nothing ever sets it to `verified`. Rows get
there by hand, over psql, by whoever remembered.

So an organisation cannot present its own number without a support request, and
`docs/voip-telnyx-setup.md` lists "Buy at least one number" as a manual portal step in the
human half of shipping. Spec 0112 added a read-only list precisely so the caller-ID select
could stop lying; this is the other half.

Numbers are also the first thing in this product that costs money **before** a call: a
one-off purchase, then a monthly charge that recurs whether or not anybody dials. That is
a different billing shape from a metered call, and it is where the brief's *provider cost
+ 20% markup* actually applies.

## 2. Goals / Non-Goals

**Goals**
- Search and buy a number from the dashboard, in human terms, without Telnyx vocabulary.
- Never buy twice because a request was retried.
- Show the one-off and the recurring price, inclusive of our markup, **before** confirming.
- Verify a number the organisation already owns, so it can be presented as caller ID.
- Model the lifecycle honestly, including the states where a regulator is the blocker.
- Charge the recurring cost, and never release a business phone number over a transient
  billing failure.

**Non-Goals**
- Porting an existing number into Telnyx. A regulated, multi-day, document-bearing process
  that deserves its own spec rather than a checkbox here.
- Telnyx Managed Accounts (brief §45). Investigated in §8; not depended on.
- Automated regulatory document upload. Where a requirement cannot be satisfied
  programmatically the product says so and links out, rather than pretending.

## 3. Requirements

- **R1 — Search reads like a person asking.** As an admin, I want to find a number by
  country and place, not by provider query syntax.
  - *Given* a country and optionally an area or city code and a type
    (local / national / toll-free), *then* matching numbers are listed with their monthly
    and one-off **customer** price and any regulatory requirement.
  - *Given* the provider cannot search, *then* the UI says so rather than showing nothing.

- **R2 — Buying is confirmed, priced and idempotent.** As an admin, I want to know what I
  am agreeing to, and to retry safely.
  - *Given* a number is chosen, *then* the confirmation names the one-off charge, the
    recurring charge and the next renewal date before anything is bought.
  - *Given* the same purchase request arrives twice, *then* one number is bought and one
    ledger entry is written.
  - *Given* the purchase fails at the provider, *then* **nothing is charged**.

- **R3 — Prices carry our markup, computed in one place.** As the business, I want the
  pass-through policy applied once and auditable.
  - *Given* a provider cost, *then* the customer price is `cost × (1 + markup)` with
    `markup` from configuration, defaulting to 20%.
  - *Given* any price shown or charged, *then* it came from that one function.
  - *Given* a purchase, *then* the provider cost, the markup rate and the final customer
    charge are **stored on the row**, so an old number is never re-priced at today's rate.

- **R4 — Only a verified number may be presented.** As the business, I want caller-ID
  spoofing to stay impossible.
  - *Given* a number the organisation already owns elsewhere, *then* verification can be
    started and its state is visible.
  - *Given* verification has not succeeded, *then* the number cannot be used as caller ID —
    `resolve_caller_id` already enforces this and must keep doing so.

- **R5 — The lifecycle is honest.** As an admin, I want to know what is happening to my
  number.
  - *Given* a number, *then* its status is one of `ordering`, `pending_regulatory`,
    `active`, `suspended`, `releasing`, `released`, `failed`, and the reason is shown.
  - *Given* a regulatory requirement blocks it, *then* the product says which, and does not
    claim it is active.

- **R6 — Renewal is charged, and failure does not lose the number.** As an admin, I want a
  warning, not a disconnection.
  - *Given* a renewal falls due, *then* the recurring customer charge is taken from the
    org's credits and written to the ledger, once.
  - *Given* the balance cannot cover it, *then* the number is **suspended, not released**,
    the organisation is told, and a grace period runs before anything irreversible.
  - *Given* a release, *then* it is an explicit act with its own confirmation.

- **R7 — Buying is an admin action.** *Given* a plain member, *then* search is readable and
  purchase, verify and release are refused server-side.

## 4. Design & Architecture

**The provider boundary gains six operations**, and no Telnyx name crosses it:

```rust
async fn search_numbers(&self, q: NumberSearch) -> Result<Vec<NumberOffer>, ProviderError>;
async fn purchase_number(&self, req: PurchaseRequest) -> Result<PurchasedNumber, ProviderError>;
async fn release_number(&self, id: &ProviderNumberId) -> Result<(), ProviderError>;
async fn number_status(&self, id: &ProviderNumberId) -> Result<NumberStatus, ProviderError>;
async fn start_caller_id_verification(&self, e164: &E164) -> Result<VerificationStart, ProviderError>;
async fn check_caller_id_verification(&self, id: &VerificationId) -> Result<VerificationState, ProviderError>;
```

Each returns `ProviderError::Unsupported` where the account or the API cannot do it, which
is how `fetch_cdr` and `fetch_rate_deck` already behave — an honest refusal rather than a
fabricated success.

**`NumberMarkupPolicy` lives beside `MarginPolicy` in `voip/pricing.rs`.**

The brief asks for *provider cost + 20% markup* on pass-through, and the per-minute call
price keeps its 20% **gross-margin floor** (= 25% markup) — two different policies because
they are two different things, and `docs/voip-billing.md` §1 already explains why confusing
them is expensive. A number has no translation component and no metering: it is a bill we
receive and pass on. One type, one function, one env var; no `* 1.20` anywhere else.

**Pricing is snapshotted onto the row.** `provider_cost_usd`, `markup_rate`,
`customer_charge_usd` and `currency` are written at purchase and at each renewal. Spec 0111
R27 already requires this for calls; a number that renews for years needs it more.

**Idempotency** reuses the shape that already works: a client-supplied key, a unique index,
and `ON CONFLICT DO NOTHING RETURNING`. The same reasoning as `voip_provider_events` — the
constraint is in the database, not in a handler that could forget.

**Renewal** is a sweep beside the existing ones in `webhook.rs`, not a new subsystem.
Suspension is reversible and release is not, so the sweep may only ever suspend.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Spec | this file |
| S1 | `NumberMarkupPolicy` + tests | `server/src/voip/pricing.rs` |
| S2 | Trait + types + mock + Telnyx adapter | `server/src/telephony/{mod,mock,telnyx}.rs` |
| S3 | Migration 061 — lifecycle, pricing snapshot, idempotency | `server/migrations/061_voip_number_lifecycle.sql` |
| S4 | Routes: search, buy, verify, release + tests | `server/src/voip/numbers.rs` |
| S5 | Renewal + suspension sweep | `server/src/voip/webhook.rs` |
| S6 | Dashboard: Numbers page, buy flow, verification | `dashboard/src/pages/[lang]/phone/numbers.astro` |

## 6. Testing & Verification

- The same purchase key twice buys one number and writes one ledger row.
- A provider failure during purchase leaves no row and no charge.
- The customer price is the provider cost times `1 + markup`, and the row records all three.
- An old number keeps its recorded price when the markup changes.
- A member cannot buy, verify or release; an admin can.
- Renewal with an empty wallet suspends and does not release.
- `resolve_caller_id` still refuses an unverified number (already covered; must stay green).

## 7. Deployment & Operations

- `VOIP_NUMBER_MARKUP_PERCENT`, default `20`.
- `VOIP_NUMBER_GRACE_DAYS`, default `7` — how long a suspended number survives before a
  human decides.
- Buying a number **spends real money at the provider**. Tests run against the mock; the
  live path is exercised only under `VOIP_LIVE_TESTS`, which is already double-gated.

## 8. Risks / Open Items

1. **Telnyx Managed Accounts (brief §45) — investigated, not adopted.** They would give
   provider-side isolation and per-organisation billing, and they are the natural home for
   a reseller. They also require account-level enablement this deployment does not have,
   and they would move the billing relationship between us and Telnyx for every customer at
   once. The isolation this product needs is already enforced in our own schema and asserted
   route by route. Recommendation: revisit when a customer needs their own carrier
   relationship, not before; nothing here depends on it either way.
2. **Regulatory requirements vary by country and change.** The product surfaces what the
   provider reports and links out; it does not model each jurisdiction.
3. **A suspended number is still costing us.** Suspension protects the customer's number at
   our expense for the grace period. That is a deliberate, bounded loss.
4. Porting is out of scope, so a customer's existing number reaches us only through
   verification (caller ID) or forwarding — 0116 covers the inbound half.

## 8b. Amendment — 2026-09-12

**Ledger descriptions carry data, not English prose.**

The purchase and renewal charges wrote `"Telephone number +39…"` and `"Monthly charge for
telephone number +39…"` into `credit_transactions.description`. A ledger row outlives the
session that wrote it and is read by whoever opens the books, possibly in another language,
so a sentence stored at charge time can never be right for every future reader. The
machine-readable `kind` — `voip_number_purchase`, `voip_number_renewal` — is the translatable
half and always was. The description now carries the number and nothing else.
