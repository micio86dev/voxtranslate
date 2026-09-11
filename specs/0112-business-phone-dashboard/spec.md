# 0112 — Business Phone dashboard

| | |
|---|---|
| **Status** | 🚧 In progress |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-09-11 |
| **Shipped** | — |
| **Version** | — |
| **Commits** | — |
| **Depends on** | [0111](../0111-translated-voip/spec.md), [0106](../0106-voxtranslate-for-business/spec.md), [0009](../0009-session-transcripts/spec.md) |

## 1. Context & Problem

Spec 0111 shipped a complete **outbound** translated telephone call in 1.50.0: the
provider abstraction, the pricing engine, the credit hold/settle lifecycle, idempotent
webhooks, the media bridge and the consent announcement in 84 languages. Its §4 planned
`dashboard/src/pages/[lang]/phone/` to hold "Dialer, active call, history, detail, admin".

**Only the dialer shipped, and it shipped broken.** Verified against the code on
2026-09-11:

| # | Defect | Evidence |
|---|---|---|
| D1 | The `my-lang`, `their-lang` and `tier` selects are declared empty and never populated. `?.value ?? 'en'` returns `''` because `??` does not catch the empty string, so **every dial carries an empty `source_language`/`target_language`** into `service::dial` → `create_phone_peer`. | `phone.astro:61,65,72,399,400` |
| D2 | Two links are shipped to production that **404**: the settings link and every entry in the recent-calls list. | `phone.astro:19,479` |
| D3 | `phone.astro` is the only file in the dashboard using `class="input"` (×7), `class="checkbox"` (×3) and `btn-secondary` (×2). **None of the three exists.** The component layer is `.btn* .card .field .label .badge`. The number input, four selects, three checkboxes and two buttons render as browser defaults. | `global.css:54-94` |
| D4 | `GET …/voip/calls/{id}` returns `actual_provider_cost_usd` and `gross_margin` — our wholesale cost and our margin — to **any org member**. | `routes.rs:532-533` |
| D5 | `saveVoipSettings` is exported and typed with **zero callers**. The whole `VoipSettings` interface is unreachable. | `api.ts` |
| D6 | **215 translated strings have no UI**: `phone.settings.*` (26 keys) and `phone.detail.*` (17 keys) across all five locales. | `src/i18n/*.json` |
| D7 | `phone-dialer.ts` has tests but is **not in the coverage allowlist**, so the dashboard's 85/85 gate has never seen the dialer's logic. | `vitest.config.ts` |

> **Correction, 2026-09-11.** An earlier draft of this spec claimed CI never runs
> `dashboard/` at all. That is wrong. The parent repo's workflow does not, but
> `dashboard/.github/workflows/ci.yml` runs lint, typecheck, `test:unit`
> (`vitest run --coverage`, so the 85/85 thresholds *are* enforced), the build and the
> full Playwright suite on every push to `main`/`develop` and every pull request. Each
> submodule is its own repo with its own pipeline and deploy target, which `CLAUDE.md`
> states as a project-wide rule; a duplicate job in the parent would run the same work
> twice against a pinned commit. The real gap was the coverage allowlist above.

D1 is the load-bearing one: the feature's entire promise is that the two sides hear each
other in their own language, and the dialer cannot currently express which languages those
are. D4 contradicts the house rule that `engine/metadata.rs` proves with the test
`engine_info_never_leaks_cost_or_markup`.

This spec closes that gap. It adds **no new telco surface** — no inbound, no number
purchasing, no contacts. Those are 0114–0116, and each of them would otherwise be built on
top of a dialer that sends an empty language.

## 2. Goals / Non-Goals

**Goals**
- A dial cannot leave the browser without an explicit, valid language pair.
- Every link the dashboard renders resolves.
- Every control uses a class that exists.
- The customer sees what they were charged; they never see what it cost us.
- The 215 already-translated strings get the UI they were written for.
- The dialer's logic modules are inside the coverage allowlist, so the dashboard's
  existing 85/85 thresholds actually apply to them.
- New `voip/` + `telephony/` Rust code is gated at 85% line coverage.

**Non-Goals**
- Inbound calling ([0116](../0116-voip-inbound/spec.md), not yet written).
- Number search / purchase / verification write paths ([0115](../0115-voip-numbers/spec.md), not yet written). This spec adds only a **read-only** list so the caller-ID select is honest.
- Contacts / address book ([0114](../0114-voip-contacts/spec.md), not yet written).
- Raising the global Rust coverage floor above 66%.
- Any change to the per-minute pricing policy. The 20% gross-margin floor stands.

## 3. Requirements

- **R1 — A call is placed in a language I chose.** As a Business user, I want the language
  selects populated from the engine catalogue, so that the recipient actually hears their
  own language.
  - *Given* the dialer has loaded, *when* I open the "my language" or "recipient language"
    select, *then* it lists the languages the selected tier supports, named in my own
    locale.
  - *Given* either language is unset, *when* I submit, *then* the dial is refused in the
    browser with a message adjacent to the field, and **no request is sent**.
  - *Given* I change tier, *when* the new tier does not support a selected language,
    *then* that selection is cleared rather than silently sent.

- **R2 — The tier and project selects are real.** As a Business user, I want to choose a
  translation tier and optionally a project, so that the call is priced and filed the way I
  expect.
  - *Given* the dialer has loaded, *then* the tier select lists the engines from
    `GET /api/engines` with their per-minute rate, and the project select lists this
    organisation's projects via `listProjects`.
  - *Given* `voip_org_settings.require_project` is true, *when* no project is selected,
    *then* the dial is refused in the browser.

- **R3 — The caller ID select offers the numbers we actually own.** As a Business user, I
  want to pick which of my organisation's verified numbers the recipient sees.
  - *Given* my organisation has verified outbound numbers, *then* they are listed with
    their label.
  - *Given* it has none, *then* only the deployment default is offered, and no fabricated
    option appears.

- **R4 — Every rendered link resolves.** As any user, I want the dashboard never to link me
  to a 404.
  - *Given* the phone section, *when* I follow the settings link or any recent call,
    *then* I land on a page that exists.

- **R5 — Controls are styled.** As any user, I want form controls to look like the rest of
  the product.
  - *Given* any control in the phone section, *then* its class is defined in
    `global.css`, asserted by a test that fails on an unknown class.

- **R6 — Our cost and margin never leave the server.** As the business, I want provider cost
  and gross margin withheld from the API.
  - *Given* any org member calls `GET …/voip/calls/{id}`, *then* the response body contains
    neither `actual_provider_cost_usd` nor `gross_margin`.
  - *Given* the call has not been rated yet, *then* the customer sees a reconciliation
    **status**, never a zero (a zero reads as "free").

- **R7 — An admin can configure the phone service.** As an org admin, I want to edit every
  field of `VoipSettings` from the dashboard.
  - *Given* I am an ADMIN, *when* I save, *then* the change persists and is audit-logged.
  - *Given* I am a MEMBER, *then* the form is read-only in the UI and the server refuses
    the write regardless.

- **R8 — A finished call has a detail page.** As a Business user, I want to open a call,
  see what it was and what it cost, and reach what was said.
  - *Given* a completed call, *then* the page shows the recipient in full, the language
    pair, the tier, the status, the duration, the credits I was charged, the consent
    evidence, and whether the cost is final or still pending.
  - *Given* the call produced a transcript or a recording, *then* the page links to it.
  - *Given* the call failed, *then* the page says why, in the vocabulary the dialer uses.

- **R9 — Call history is filterable.** As a Business user, I want to find a call among
  hundreds.
  - *Given* the history page, *then* I can filter by project and page through results,
    using the `project_id`, `page` and `limit` parameters the endpoint has accepted since
    1.50.0 and nothing has ever passed.
  - *Note:* a date range is **not** in scope. `GET …/voip/calls` does not accept one, and
    adding it is a server change with its own tests — see §8. `history.astro` has date
    filters because `listOrgRooms` supports them; this endpoint does not, and a filter
    that silently does nothing is worse than no filter.
  - *Given* I am not an admin, *then* I see only my own calls (the server already enforces
    this — the UI must not imply otherwise).

- **R10 — A refusal tells me how to recover.** As a Business user out of credit, I want a
  way forward.
  - *Given* a dial refused with `insufficient_credits`, *then* the error offers a link to
    the credits page, following `resolveHaErrorCta()`.

- **R11 — The new code is gated at 85%.** As a maintainer, I want this work held to the
  bar the brief asks for, without pretending the rest of the server already meets it.
  - *Given* the dashboard's coverage run, *then* `phone-dialer.ts` and
    `phone-catalogue.ts` are inside the allowlist, so the existing 85/85 thresholds apply
    to the dialer's logic instead of skipping it.
  - *Given* the server's coverage run, *then* a gate scoped to `src/voip/` and
    `src/telephony/` fails below 85% lines, alongside the unchanged global floor of 66%.
  - *Given* the global floor, *then* it is **not** lowered.

## 4. Design & Architecture

**Components / files**

| Path | Responsibility |
|---|---|
| `dashboard/src/scripts/phone-catalogue.ts` | **New, pure.** Turns `GET /api/languages` + `GET /api/engines` into select option lists, resolves tier↔language compatibility, and validates a dial request. Pure so it is unit-testable without a DOM. |
| `dashboard/src/pages/[lang]/phone.astro` | Dialer. Fixed classes, populated selects, guarded submit. |
| `dashboard/src/pages/[lang]/phone/settings.astro` | **New.** The `VoipSettings` form (R7). |
| `dashboard/src/pages/[lang]/phone/detail.astro` | **New.** The telephony record for one call, linking to `history/detail` for the conversation (R8). |
| `dashboard/src/pages/[lang]/phone/calls.astro` | **New.** Filterable history (R9). |
| `dashboard/src/components/phone/SectionNav.astro` | **New.** Tab bar for the phone section. |
| `dashboard/src/styles/global.css` | Adds `.checkbox` to the component layer. |
| `server/src/voip/routes.rs` | `CallDetailRow` loses two fields (R6); adds `GET …/voip/numbers` (R3). |

**Languages come from `GET /api/languages`, the catalogue that already exists.** The
server ships `engine/langmap.rs` over an embedded `languages.json` and already serves it
publicly, cached an hour: `languages` (`code`, `native`, `english`, `region`, `rtl`,
`flag`), `regions` (the grouping order the picker uses) and **`tiers`** (tier → supported
output languages).

That last field is the reason this beats the alternatives. `Intl.DisplayNames` would
produce plausible language *names* and know nothing about which tier can actually speak
them, so R1's third criterion — clearing a selection the new tier cannot serve — would have
had no source of truth and the dialer could offer a language the engine will refuse. A
hand-maintained table in the dashboard would drift from the client's the day a language is
added. Reusing the endpoint also means the phone picker names a language exactly as the
call app does, which is the same word the customer already learned.

**Per-call pages use a query param.** The dashboard is `output: 'static'`; a `[id].astro`
segment cannot prerender an unknown id. The established convention is
`history/detail.astro?session=` and `projects/detail.astro?id=`, and this follows it:
`phone/detail.astro?id=<call_id>`. `phone.astro:479` is updated to match.

**The transcript is not re-rendered on the call page.** A VoIP call *is* a `call_sessions`
row (spec 0111 §4), and `history/detail.astro?session=` already renders the original
transcript, the on-demand translation, the recording player and the TXT/PDF exports for any
such row — around eighty lines of logic, in one place, already translated under
`transcript.*`. Copying it would create a second copy to keep correct; extracting it would
refactor a working page that no test covers. The call page therefore owns the telephony
record — recipient, languages, tier, status, money, consent evidence — and links to the
conversation.

**Why the section nav is a tab bar, not more header links.** `Header.astro` is a single
flat row of 14 links already relying on `overflow-x-auto`. Four more would make the primary
navigation unusable on a narrow viewport to serve one section. A tab bar local to
`/phone/*` keeps the header's cost flat.

**R6 — what replaces the two removed fields.** `quoted_price_per_min` and
`credits_consumed` stay: they are what the customer agreed to and what they paid. The
reconciliation state becomes a string enum `cost_status: "pending" | "final"` derived
server-side from `actual_provider_cost_usd IS NULL`, so the UI can say "final cost
confirmed" without transporting the number. `phone.detail.providerCost` and
`phone.detail.margin` are therefore retired from use; `phone.detail.providerCostPending`
is repurposed for the status line.

**Key decisions**

| Decision | Rationale | Rejected alternative |
|---|---|---|
| Validation lives in a pure module, not in the `.astro` script | The dialer's existing logic module `phone-dialer.ts` is pure and well tested; the inline script is not testable | Validating inside the submit handler |
| Languages from `GET /api/languages` | It is the catalogue the call app already uses, and it is the only source that knows tier↔language compatibility | `Intl.DisplayNames` (no tier data); a second table in the dashboard (drifts) |
| `.checkbox` becomes a real component class | Three call sites invented it already; a checkbox needs its own accent/size tokens | Rewriting the checkboxes as `.field` (wrong — `.field` is a full-width text input) |
| Read-only `GET …/voip/numbers` now | The caller-ID select is a lie without it, and the table already exists | Waiting for 0115 and leaving a single hardcoded option |
| Retire `phone.detail.margin` rather than delete the key | Specs are append-only history; a retired key costs nothing and deleting one across 5 locales invites drift | Deleting the keys |

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Spec | `specs/0112-business-phone-dashboard/spec.md` |
| S1 | R6 — strip cost/margin from the API, mirror test of `engine_info_never_leaks_cost_or_markup` | `server/src/voip/routes.rs`, `server/tests/voip_api.rs` |
| S2 | R3 — read-only `GET …/voip/numbers` | `server/src/voip/routes.rs`, `server/tests/voip_api.rs` |
| S3 | R1, R2 — `phone-catalogue.ts` + its tests | `dashboard/src/scripts/phone-catalogue.ts` |
| S4 | R1, R2, R3, R5, R10 — dialer: populate, guard, fix classes | `dashboard/src/pages/[lang]/phone.astro`, `global.css` |
| S5 | R4, R7 — settings page | `dashboard/src/pages/[lang]/phone/settings.astro` |
| S6 | R4, R8 — call detail page, linking to the existing transcript view | `dashboard/src/pages/[lang]/phone/detail.astro` |
| S7 | R9 — history page + section nav | `dashboard/src/pages/[lang]/phone/calls.astro`, `components/phone/SectionNav.astro` |
| S8 | Dashboard home: phone KPI, recent calls, quick action | `dashboard/src/pages/[lang]/dashboard.astro` |
| S9 | R11 — scoped VoIP coverage gate; dashboard coverage allowlist | `.github/workflows/ci.yml`, `dashboard/vitest.config.ts` |

## 6. Testing & Verification

**Rust**
- `voip_api.rs`: the detail response body contains neither `actual_provider_cost_usd` nor
  `gross_margin` (R6) — the mirror of `engine_info_never_leaks_cost_or_markup`.
- `voip_api.rs`: `cost_status` is `"pending"` when unrated and `"final"` once rated (R6).
- `voip_api.rs`: `GET …/voip/numbers` lists only `outbound_enabled AND verified` rows, is
  scoped to the org, and refuses a non-member (R3).

**Dashboard unit (vitest)**
- `phone-catalogue.test.ts`: language options are named in the active locale; an unknown
  tag falls back to itself; tier change clears an unsupported language; a dial request with
  an empty language is rejected; `require_project` is honoured (R1, R2).
- A class-name guard asserting every class used in the phone pages is defined in
  `global.css` (R5).
- A key-parity test across the five locale files — none exists today; they agree by luck.

**Dashboard e2e (Playwright, API stubbed)**
- The existing 15 tests in `e2e/phone.spec.ts` stay green.
- New: selects are populated; submitting with no language shows an error and issues no
  request; the settings form round-trips; the detail page renders a transcript and shows no
  margin; history filters and pages; `insufficient_credits` renders a credits CTA.

**Manual, against `VOIP_PROVIDER=mock`**
- Place a call with a real language pair and confirm the phone peer is created with it.
- Confirm the detail page's network response carries no `gross_margin`.

## 7. Deployment & Operations

- **No migration.** No schema change; `voip_numbers` and the money columns are untouched.
- **No new environment variable.**
- **API change is a removal**, and the only consumer is this dashboard, shipped in the same
  release. Nothing else reads `gross_margin`.
- Rollout is unchanged: `VOIP_ROLLOUT_STAGE` and `voip_org_settings.enabled` still gate
  everything. A dashboard that renders more pages does not widen access.

## 8. Risks / Open Items

1. `Intl.DisplayNames` coverage varies slightly by browser and locale. The fallback is the
   language tag, which is ugly but never wrong. Pinned by a test.
2. The caller-ID select is honest but still empty for most orgs until 0115 lands the
   purchase and verification write paths. Rows must be inserted out-of-band until then.
3. Retiring `phone.detail.margin` leaves two unused keys in five locale files. Cheaper than
   a five-file deletion, and 0117 (admin tooling) may want them.
4. Adding `phone-dialer.ts` and `phone-catalogue.ts` to the coverage allowlist raises the
   measured denominator. Both are near 100% today, so the gate has headroom; the module
   that sits closest to the floor is `api.ts`, and it is the one to watch as the client
   grows.
5. **Call history cannot be filtered by date.** `GET …/voip/calls` accepts only
   `project_id`, `page` and `limit`. The meeting history has date filters because its
   endpoint supports them; matching that here needs a server change and belongs with the
   analytics work in 0117, where date ranges are the whole point.

## 9. References

- Specs: [0111](../0111-translated-voip/spec.md) (the feature this completes)
- Files: `dashboard/src/pages/[lang]/phone.astro`, `server/src/voip/routes.rs`,
  `server/src/engine/metadata.rs` (the leak precedent)
- Docs: `docs/voip-billing.md`, `docs/voip-telnyx-setup.md`
