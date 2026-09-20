# voip-webapp-caller-id-picker

## Objective
Fix the webapp (`client/`) VoIP dialer so it cannot submit a caller-id number that the
server will refuse, matching the dashboard's existing, tested behaviour.

## Problem
User report: outbound calls from `app.voxtranslate.app` always fail with the red error
"Quel numero non è un numero in uscita verificato per questa organizzazione"
(`caller_id_unverified`). Root cause investigated and confirmed (prod DB + Telnyx API,
read-only):

- The org's only purchased number, `+34959740254`, was bought 2026-09-14 14:57:12 UTC and
  released via the admin `release()` endpoint 68s later (14:58:20 UTC) — a real, irreversible
  Telnyx DELETE (confirmed: Telnyx GET on that number now 404s). `audit_logs`/`admin_audit`
  have no rows for that org/window, so who/what triggered the release is not traceable.
- Calls placed from the **dashboard** (2026-09-15 → 2026-09-19, all `completed`) never used
  `+34959740254` — `voip_calls.caller_e164` shows `+34828670421`, which is
  `TELNYX_DEFAULT_CALLER_ID` (the platform's own fallback number), not an org-owned number.
  The dashboard's dial panel works because it never offers the dead number in the first
  place: `dashboard/src/pages/[lang]/phone.astro` renders a `<select id="caller-id">`
  populated only with `usableCallerIds()` (`dashboard/src/scripts/phone-catalogue.ts:238`,
  filters `outbound_enabled && verification_status === 'verified'`), so a released number
  never appears as an option and the empty default option falls back to the platform number.
- The **webapp** (`client/src/pages/index.astro`) instead renders `<input id="phone-caller-id"
  type="text">` — free text, no filtering, no number list at all. The user typed their own
  (dead) number in, `client/src/scripts/app.ts:2638` sent it verbatim as `caller_id`, and the
  server's `resolve_caller_id` (server/src/voip/routes.rs:539, unchanged/correct — this refusal
  is deliberate compliance behaviour shipped 3 days ago, commit b98d834d) rightly refused it.
  `client/src/scripts/voip.ts` has no `listVoipNumbers()` at all — the capability the webapp
  needs was simply never ported from the dashboard (design doc comment at voip.ts:97 even says
  `caller_id` is meant to be "re-resolved server-side from owned, verified numbers").

This is a real webapp frontend gap. Server-side behaviour is correct and out of scope.

## Scope
- `client/src/scripts/voip.ts`: add `VoipNumber` interface + `listVoipNumbers(orgId)`
  (`GET …/voip/numbers`), mirroring `dashboard/src/lib/api.ts:1450-1479`.
- `client/src/scripts/phone-dialer.ts`: add `CallerIdCandidate` interface + pure
  `usableCallerIds()` filter, mirroring `dashboard/src/scripts/phone-catalogue.ts:219-240`
  verbatim in behaviour (`outbound_enabled && verification_status === 'verified'`).
- `client/src/scripts/phone-dialer.test.ts`: unit tests for `usableCallerIds` (TDD: RED
  first, then GREEN), mirroring `dashboard/src/scripts/phone-catalogue.test.ts`'s cases for
  the same function.
- `client/src/pages/index.astro`: replace `<input id="phone-caller-id" type="text">` with
  `<select id="phone-caller-id">` containing one default `<option value="">` using the
  EXISTING i18n key `phoneCallerIdPlaceholder` ("Organisation default") as its label — no
  new i18n keys needed (client already has `phoneCallerIdLabel` + `phoneCallerIdPlaceholder`
  in all 84 locales; reuse them, do not add new ones).
- `client/src/scripts/app.ts`: add `loadPhoneCallerIds(orgId)` mirroring
  `loadPhoneProjects()` (lines 2415-2429) — clear select, add default `""` option (label from
  `phoneCallerIdPlaceholder`), fetch `listVoipNumbers`, filter with `usableCallerIds`, append
  options, set `.value` to the `is_default` one if present. Call it from
  `openPhoneDialPanel()` (line 2451) right after `await loadPhoneProjects(phoneOrgId)`.
  Change `phoneCallerIdInput`'s type to `HTMLSelectElement` and its dial-payload read at
  line 2638 from `.value.trim() || null` to `.value || null`.

## Constraints
- TDD mode: Strict (project-wide). Write/extend the `usableCallerIds` test FIRST (RED),
  then implement (GREEN). No invented evidence — actually run the test suite.
- i18n: reuse-only, no new keys, no new locale files touched.
- Do not touch server code — the backend refusal is correct and intentional.
- Do not touch `dashboard/` — it already works correctly; it's the reference pattern only.

## Release vehicle
Hotfix `hotfix/1.60.3` off `main` (current `main` HEAD = tag `v1.60.2`). Merge to `main`
first (tag `v1.60.3`), then back-merge to `develop`, per this repo's Git Flow rule. Deploy:
`main` push → prod (`api.voxtranslate.app` server; webapp `app.voxtranslate.app` client
build, confirm actual deploy target/CI job before assuming). `develop` push → staging.
Wait for green CI on both before calling this done.

## Tasks
- [x] 1. Implement `listVoipNumbers` in `client/src/scripts/voip.ts`
- [x] 2. TDD: write failing test(s) for `usableCallerIds` in `phone-dialer.test.ts`, then
      implement `usableCallerIds` in `phone-dialer.ts` until green
- [x] 3. Convert `#phone-caller-id` to a `<select>` in `index.astro` (reuse existing i18n
      keys only)
- [x] 4. Wire `loadPhoneCallerIds()` into `app.ts` / `openPhoneDialPanel()`, update the
      dial-payload read
- [x] 5. Run full client test suite + typecheck, confirm green
- [ ] 6. Manual verification in a real browser session — NOT done this run: no test org
      credentials with an active subscription available in this session to reach the phone
      panel live, and no existing e2e spec touches `phone-caller-id`/`phoneCallerId`
      (checked: zero matches under `client/e2e` and `**/*.spec.ts`). Unit coverage in task 2
      directly exercises the fix's core logic (a released number is filtered out), and
      `astro check` confirms the new markup/script wiring compiles and type-checks. Flagged
      as an open gap, not silently skipped.
- [ ] 7. Commit on `hotfix/1.60.3` (Conventional Commits), open PR/merge to `main`, tag
      `v1.60.3`, wait for green CI / prod deploy
- [ ] 8. Back-merge `main` → `develop`, wait for green CI / staging deploy
- [ ] 9. Report final verified state to the user, including that the underlying number
      `+34959740254` itself still needs to be re-purchased separately (out of this fix's
      scope) if the org wants to use their own Spanish number as caller id again

## Verification evidence

Files changed (real `git diff --stat`, working tree on `hotfix/1.60.3` vs `main`):
```
 client/src/pages/index.astro            |  2 +-
 client/src/scripts/app.ts               | 27 +++++++++++++--
 client/src/scripts/phone-dialer.test.ts | 59 +++++++++++++++++++++++++++++++++
 client/src/scripts/phone-dialer.ts      | 22 ++++++++++++
 client/src/scripts/voip.ts              | 28 ++++++++++++++++
 5 files changed, 135 insertions(+), 3 deletions(-)
```

RED (before implementing `usableCallerIds`, `npx vitest run src/scripts/phone-dialer.test.ts`):
```
 ❯ src/scripts/phone-dialer.test.ts (52 tests | 2 failed) 16ms
   ❯ usableCallerIds (2)
     × offers only a number that is both verified and outbound-enabled 2ms
     × offers nothing rather than something fabricated when the org owns nothing 0ms
TypeError: usableCallerIds is not a function
 Test Files  1 failed (1)
      Tests  2 failed | 50 passed (52)
```

GREEN (after implementing, same command):
```
 Test Files  1 passed (1)
      Tests  52 passed (52)
   Duration  493ms
```

Full client unit suite (`npx vitest run`):
```
 Test Files  103 passed (103)
      Tests  2074 passed (2074)
   Duration  8.65s
```

Typecheck (`npm run check` → `astro check`):
```
Result (122 files):
- 0 errors
- 0 warnings
- 3 hints
```
(the 3 hints are pre-existing, in unrelated files `public/sw.js`, `src/scripts/talk/page.ts`,
`src/scripts/tts/manager.ts` — not introduced by this change)
