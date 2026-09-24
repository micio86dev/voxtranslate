# phone-cta-card-button-and-quote-fields

## Objective
Two related fixes on the web-app phone dialer (home page):
1. The "Call a phone number" card at the bottom of the home (`#phone-cta`) has a title and
   a description but no action: its trigger was moved to the hero row. Give the card its
   own trigger too, since the card is where the dial panel actually expands.
2. A user of an org with `require_project` picks a project and still sees the red
   "This organisation requires a project for every call" refusal.

## Problem
1. Design (impeccable, product register): a card with copy and no affordance is a dead
   box. The sibling card `#host-webinar-cta` already uses title + description + button,
   so a button inside `#phone-cta` is the consistent home-page pattern; the hero button
   stays (user decision: keep the top CTA, add a second one in the card). Both triggers
   open the same inline panel and both hide while it is open.
2. `refreshPhoneQuote()` (`app.ts`) posts `/voip/quote` with only destination and the
   two languages: no `project_id`, no `caller_id`. The server checks project tenancy and
   caller-id identically for quote and dial (`check_project` in `voip/routes.rs`), so a
   `require_project` org refuses every quote with `project_required` and the Call button
   never enables. Changing the project / their language / caller-id selects does not
   re-quote either: only typing in the destination does. The project select also sits
   inside the collapsed `<details id="phone-details">`, so the user sees the refusal but
   not the field that resolves it.

## Fix
- `index.astro`: a `#phone-cta-open` primary button inside `#phone-cta` (same
  `phoneCtaButton` i18n key, no new strings), full-width like the webinar card's button.
- `phone-dialer.ts`: pure `phoneDialRequest(fields, quote)` builds the ONE request body
  used by both quote and dial: normalised destination, empty selects → `null`,
  `engine_id` from the quote when present. Unit tests (RED → GREEN).
- `app.ts`: `setPhoneTriggersVisible()` toggles both triggers at every site that toggled
  the hero one; quote and submit both go through `phoneDialRequest`; `change` on the
  project / their-language / caller-id selects re-quotes; a `project_required` refusal
  opens `#phone-details` and focuses the project select.
- `e2e/phone-dialer.spec.ts`: the card button opens the panel and both triggers hide;
  cancel restores both.

## Constraints
- No new i18n keys. Feature branch `feature/phone-cta-card-button` off `develop`.
- TDD strict: RED first on the pure helper. Runners: `npx vitest run <file>`, e2e via CI.
- Route: delegated writer (4 files across markup, app wiring, pure module, e2e).

## Tasks
- [x] 1. Design decision + root cause of the quote refusal (above).
- [x] 2. Card trigger button (`index.astro` + `app.ts` `setPhoneTriggersVisible`).
- [x] 3. Quote carries project/caller-id, re-quotes on select change, opens details on
      `project_required` (`phone-dialer.ts` helper + tests, `app.ts`).
- [x] 4. e2e assertion for the card trigger (runs in CI); `npx vitest run` 2080/2080,
      `npx tsc --noEmit` clean, `npm run check` 0 errors (3 pre-existing hints).
- [ ] 5. Work-unit commits, RDD assess, PR → `develop`, CI green (e2e included), merge.

## Verification evidence
- Writer (one delegated writer, 5 files): RED `phoneDialRequest is not a function`
  (4 failed) → GREEN `phone-dialer.test.ts` 56/56; full `npx vitest run` 103 files,
  2080/2080; `npx tsc --noEmit` clean; `npm run check` 0 errors, 0 warnings, 3 hints in
  untouched files (`public/sw.js`, `talk/page.ts`, `tts/manager.ts`); no eslint config.
- Parent spot check: `npx vitest run phone-dialer.test.ts phone-call.test.ts` 69/69,
  `npx tsc --noEmit` clean.
- e2e (`phone-dialer.spec.ts`, card-trigger round trip added) not run locally: needs the
  full backend stack; CI runs it on the PR.
