# 0119 — Self-service regulatory requirements for a purchased number

| | |
|---|---|
| **Status** | 🚧 In progress |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-09-16 |
| **Depends on** | [0115](../0115-voip-numbers/spec.md), [0111](../0111-translated-voip/spec.md) |

## 1. Context & Problem

A number bought with a regulatory requirement stays `pending_regulatory` forever: nothing
in the product submits the requirement, uploads a document, or re-reads the order's status
— `number_status()` has no callers. The only way forward today is a support request, and
whoever handles it has no self-service tool either. This change reverses the 0115 non-goal
("Automated regulatory document upload") now that the owner has confirmed the product
should complete the paperwork itself: discover what a country/number-type/action needs,
accept the values (including documents), forward them to the carrier as one submission,
and keep the number's status honest — including the case where the carrier rejects it and
correction is still possible.

## 2. Goals / Non-Goals

**Goals**
- List the regulatory requirements a purchased number's country, type and action call for,
  sourced from the provider rather than guessed.
- Accept textual, address and document values against that schema and forward them as one
  submission.
- Stream an uploaded document straight through to the provider; never store its bytes.
- Reuse an approved requirement group across a later purchase in the same org, country,
  number kind and action, instead of asking for the same paperwork twice.
- Keep order status current via a reconcile sweep, sped up (not replaced) by a webhook.
- Make a carrier rejection resubmittable, with the reason shown, rather than terminal.
- Fail a number only on an explicit provider deadline-miss cancellation — never infer it.

**Non-Goals**
- Blocking a purchase until requirements are pre-approved (a pre-purchase group flow).
- Webhooks for intermediate requirement states — not confirmed to exist on this account.
- Caller-ID ownership verification — a separate Telnyx subsystem, covered by 0115 R4.
- Storing document bytes anywhere on our side.

## 3. Requirements

- **R1 — Requirement discovery.** As an admin, I want to see exactly what a purchased
  number still needs, in the provider's own field/document shape.
  - *Given* a sub-order in `pending_regulatory` for a country/number-type/action, *when*
    the org requests its requirement list, *then* the system returns the field and
    document requirements the provider reports for that combination.
  - *Given* a `pending_regulatory` number created before this feature, with no
    `provider_sub_order_id`, *when* requirements are requested, *then* the system refuses
    with `regulatory_unlinked` (409) instead of fabricating a requirement list.

- **R2 — Requirement submission.** As an admin, I want to answer the requirement fields
  once and have them forwarded together.
  - *Given* a discovered schema for an order in `pending_regulatory`, *when* the org
    submits all required fields with valid values, *then* the submission is forwarded and
    the number status becomes `regulatory_review`.
  - *Given* a submission already forwarded (status `regulatory_review`), *when* the org
    submits again before a rejection, *then* the system refuses with
    `submission_already_pending` and forwards nothing.
  - *Given* a value the provider rejects (e.g. a malformed address), *when* forwarded,
    *then* the provider's rejection reason is surfaced and the number status is unchanged.
  - *Given* a provider outage, *when* a submission is forwarded and the provider answers
    5xx, *then* a retryable failure is reported and the number status is unchanged.

- **R3 — Document stream-through.** As the business, I want a legally sensitive document to
  cross our servers without resting on them.
  - *Given* a document within the size and content-type limits, *when* it is uploaded for a
    requirement field, *then* it streams to the provider, no bytes are stored on our side,
    and only `document_id` and `av_scan_status` are persisted.
  - *Given* a file over the size limit or of an unsupported content type, *when* uploaded,
    *then* the system refuses before streaming, with a distinct refusal code per case.
  - *Given* a document the provider flags as failing its antivirus scan, *when* the scan
    result is read, *then* `av_scan_status` is stored failed and the requirement is not
    considered satisfied.

- **R4 — Requirement-group reuse.** As an admin buying a second number in the same country
  and category, I want to skip paperwork we already completed.
  - *Given* no group exists yet for an org + country + number-kind + action combination,
    *when* a number in that combination is purchased, *then* a new group is created and
    attached to the sub-order.
  - *Given* an approved group for that combination, *when* a second number is purchased in
    it, *then* the group is attached and no new submission is required.
  - *Given* a group in `declined` or `expired` state, *when* a new number is purchased in
    that combination, *then* a new group is required instead of the stale one.

- **R5 — Order-status projection and resubmission.** As an admin, I want the number's
  status to say what is actually true, and to let me fix a rejection.
  - *Given* the provider reports the sub-order's requirements as its own
    `requirement-info-exception` state, *when* projection runs, *then* the number status
    becomes `regulatory_rejected`, the reason is shown, and submission is re-enabled. That
    provider-side name is never stored or exposed as our status word.
  - *Given* a number in `regulatory_rejected`, *when* the org submits corrected values,
    *then* the submission is forwarded and status returns to `regulatory_review`.

- **R6 — Deadline cancellation.** As the business, I want `failed` to mean exactly one
  thing: the provider gave up on this sub-order.
  - *Given* a number in `regulatory_rejected` past its provider deadline, *when* the
    provider reports the sub-order cancelled, *then* status becomes `failed` and the
    number is no longer resubmittable. No other condition may drive this transition.

- **R7 — Reconcile sweep.** As the business, I want order status kept current without
  depending on a webhook arriving.
  - *Given* a number left in `regulatory_review` with no webhook received, *when* the sweep
    runs, *then* it reads the current provider status and applies the same projection the
    webhook path would.
  - *Given* `VOIP_REGULATORY_RECONCILE=false`, *when* the sweep interval elapses or a
    `number_order.complete` webhook arrives, *then* neither advances status nor resets the
    next-check time automatically — though the manual refresh route still queries the
    provider directly on demand.

- **R8 — Webhook fast-path.** As the business, I want the webhook to only ever speed up
  a read we would do anyway, never to apply unverified state.
  - *Given* a signed `number_order.complete` webhook for a known order, *when* verified,
    *then* the affected number's next reconcile check is moved to now, so the sweep reads
    live status ahead of schedule. The payload's own status is never applied directly.
  - *Given* a webhook with an invalid or missing signature, *when* received, *then* it is
    rejected and no next-check time changes.
  - *Given* the same webhook event delivered twice, *when* both are processed, *then* the
    next-check time is nudged once in effect, with no duplicate status transition.

- **R9 — Org authorization and tenancy.** As the business, I want this exactly as strict as
  every other admin action on a number (0115 R7).
  - *Given* an order owned by org A, *when* an admin of org B requests its requirements or
    status, *then* the system refuses with `not_found`, disclosing nothing about org A's
    order.
  - *Given* a plain member of the owning org, *when* they attempt to submit requirements,
    *then* the system refuses server-side.

- **R10 — Dashboard UX and localization.** As an admin, I want to complete requirements
  from the numbers page, in my own language.
  - *Given* a number in `regulatory_rejected`, *when* the org views the numbers page,
    *then* the reason is shown and a resubmit form is offered, translated in the viewer's
    locale.
  - *Given* a new UI string for this feature, *when* any of the 5 dashboard locale files
    lacks the key, *then* the change is incomplete per the dashboard i18n bar.

## 4. Design & Architecture

- **Components / files:**
  - `server/src/telephony/mod.rs` — new trait operations (requirement discovery,
    requirement-group create/patch/get/attach, document upload, sub-order status), new
    domain types, and `PurchasedNumber` gaining the order/sub-order ids and the number
    kind. Telnyx and the mock provider implement every operation in lockstep (0111 D1).
  - `server/src/telephony/telnyx.rs` — the Telnyx adapter for the operations above,
    including true streaming upload and redacted error logging.
  - `server/src/telephony/mock.rs` — the same operations, fixture-shaped like Telnyx, plus
    scripting knobs for tests.
  - `server/src/voip/regulatory.rs` — new module: group claim/reuse, submission, document
    linking, the pure `transition()` projection, and the reconcile sweep.
  - `server/src/voip/numbers.rs` — `buy()` persists the order/sub-order ids and the number
    kind, and sets `outbound_enabled` from the purchased status rather than always `TRUE`.
  - `server/src/voip/webhook.rs` — `number_order.complete` is normalised and nudges the
    affected number's next reconcile check; it never applies status directly.
- **Data model:** migration `064_voip_number_requirements.sql` — order/sub-order ids,
  number kind and a reconcile schedule on `voip_numbers`; `voip_requirement_groups` keyed
  by org + country + number kind + action; `voip_requirement_documents` storing
  `document_id` + `av_scan_status` + content metadata, never bytes.
- **Protocol / API:** `GET/PUT /voip/numbers/{id}/requirements`, `POST …/documents`,
  `POST …/submit`, `POST …/refresh` under the existing business-org route prefix.
- **Sequence:** buy (unchanged charge order) → order/sub-order ids persisted →
  `pending_regulatory`, `outbound_enabled=false` → discover requirements (creates or reuses
  a group) → fill fields / upload documents → submit (attaches the group to the sub-order,
  status → `regulatory_review`) → sweep or webhook-nudged refresh reads provider status →
  `active` (approved), `regulatory_rejected` (resubmittable), or `failed` (deadline missed).
- **Key decisions:** see `sdd/voip-number-order-requirements/design` (D1–D14) for the full
  rationale — trait shape, status vocabulary, the CHECK swap, caller-ID safety on buy,
  who writes reconciled state, the kill switch, the group claim race, streaming upload
  limits and PII-safe logging.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Spec + 0115 amendment | this file, `specs/0115-voip-numbers/spec.md` |
| S1 | Migration 064 + `NumberStatus` variants + `buy()` D4 | `server/migrations/064_*.sql`, `server/src/telephony/mod.rs`, `server/src/voip/numbers.rs` |
| S2 | Trait types/methods + mock + `transition()` | `server/src/telephony/{mod,mock,telnyx}.rs`, `server/src/voip/regulatory.rs` |
| S3 | Telnyx adapter: requirements/groups/sub-order/attach | `server/src/telephony/telnyx.rs` |
| S4 | Telnyx upload streaming + address resolution + redacted logging | `server/src/telephony/telnyx.rs` |
| S5 | `regulatory.rs` GET/PUT/submit/refresh + group claim | `server/src/voip/regulatory.rs` |
| S6 | Document upload route wiring | `server/src/voip/routes.rs`, `server/src/voip/regulatory.rs` |
| S7 | Sweep + kill switch + webhook nudge | `server/src/voip/regulatory.rs`, `server/src/voip/webhook.rs`, `server/src/config.rs` |
| S8-S10 | Dashboard: api client, panel/controller, i18n | `dashboard/` (separate change) |

## 6. Testing & Verification

- `NumberStatus::parse` recognises `regulatory_review`/`regulatory_rejected`; any unknown
  word still maps to `Ordering` (R5/R6 depend on this never silently activating a number).
- `buy()` persists the order/sub-order ids and sets `outbound_enabled=false` for a
  purchase that comes back with a regulatory requirement (R1, and the D4 caller-id fix).
- `transition()` maps every `SubOrderState` shape onto the right status/reason/outbound
  triple, exhaustively (R5, R6).
- The mock provider's requirement/group/document responses are fixture-shaped identically
  to the Telnyx adapter's parsed output (R1).
- A group claim race (two concurrent "ensure a group" calls) creates exactly one provider
  group; the loser is refused `requirements_busy` (R4).
- Cross-org number id requests are refused `not_found`, never `forbidden` (R9).
- A non-admin submit attempt is refused server-side; a member may still call `/refresh`
  (R9).
- A legacy `pending_regulatory` row with no `provider_sub_order_id` is refused
  `regulatory_unlinked` rather than served a fabricated requirement list (R1).
- An upload past the size cap aborts the stream and the handler answers 413; a declared
  `application/pdf` with PNG magic bytes is refused 415 (R3).
- A webhook with an invalid signature changes nothing; a duplicate delivery nudges the
  next-check time once in effect (R8).
- The sweep advances a stale `regulatory_review` order with no webhook received (R7).

## 7. Deployment & Operations

- `VOIP_REGULATORY_RECONCILE`, default `true` — gates the sweep and the webhook nudge; the
  manual `/refresh` route is unaffected. An env flip stops mass mutation from a drifted
  provider contract without a Git Flow hotfix.
- Migration `064_voip_number_requirements.sql` is additive; never edited after merge.
- Legacy `pending_regulatory` rows (bought before this feature, no order id) show
  `regulatory_unlinked` rather than a requirement list; counted read-only in production
  before this ships, as a rollout risk check.
- Rollback: revert the server release (the v2 CHECK still accepts the old status words;
  old code reads the new ones as `Ordering`), or set `VOIP_REGULATORY_RECONCILE=false` to
  stop the sweep and webhook nudge without a code revert.

## 8. Risks / Open Items

1. **PII leak via logs or temp buffers.** Mitigated by true streaming (no buffered bytes),
   a redacted logging variant for every new Telnyx call, and size/type caps enforced
   before any byte reaches the carrier.
2. **Unlinked Telnyx documents auto-delete after 30 minutes.** Mitigated by linking the
   uploaded document to its requirement group in the same request that uploaded it.
3. **Telnyx contract drift** (the exact `filter[...]` params, the PATCH field-value shape,
   the attach-endpoint path, and whether an address value is submitted as an address id)
   is verified by a `#[ignore]`, `VOIP_LIVE_TESTS`-gated smoke test before the adapter
   slices merge — see `sdd/voip-number-order-requirements/design` Open Questions.
4. Whether number-order webhooks reach the existing Telnyx webhook route at all is
   unconfirmed; the sweep is the source of truth either way, so this only affects latency.

## 9. References

- Depends on: [0115](../0115-voip-numbers/spec.md), [0111](../0111-translated-voip/spec.md)
- Design: `sdd/voip-number-order-requirements/design` (Engram)
- Files: `server/src/voip/regulatory.rs`, `server/migrations/064_voip_number_requirements.sql`
