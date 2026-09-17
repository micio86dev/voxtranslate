# 0120 — Web-app VoIP dialer for B2B subscribers

| | |
|---|---|
| **Status** | 🚧 Implemented, pending live verification |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-09-17 |
| **Shipped** | — |
| **Version** | — |
| **Commits** | `96158399`, `338243d5`, `59214277`, `39d21a6d`, `bba1d52c`, `f6fcabe0`, `ec66e535` |
| **Depends on** | [0111](../0111-translated-voip/spec.md), [0112](../0112-business-phone-dashboard/spec.md), [0114](../0114-voip-contacts/spec.md) |

## 1. Context & Problem

Placing a translated phone call today means leaving the call app, dialing in the dashboard,
then clicking a `?room=` link that opens a THIRD tab where the audio actually lives. The call
app already owns mic capture, translated playback and subtitles; the dial/quote/contacts
routes already accept the app's own JWT (same auth endpoint, no server change needed). This
spec covers dialing, monitoring and ending a translated phone call from inside the call app
itself — a second client of the settled 0111/0112/0114 server contracts, with zero server
changes.

## 2. Goals / Non-Goals

**Goals**
- One click inside the app places a translated call: no tab hop, no device-check detour.
- The CTA and dial flow reuse the app's existing subscription-gating and call-state
  machinery instead of inventing new screens or a new gating mechanism.

**Non-Goals**
- Any server-side change — this is a second client of the settled 0111/0112/0114 contracts.
- Extracting `dashboard/src/scripts/phone-dialer.ts` into a shared package — `dashboard/` is
  a separate git repo (submodule); the pure dial logic was ported into `client/` instead,
  with the dashboard's own test cases copied verbatim to bound drift.
- Keep-alive on navigate-away, DTMF, call history UI, WS push for call state (REST poll, as
  the dashboard already does).
- Visual/UX polish beyond the `impeccable`-skill-reviewed CTA/dial-panel/in-call presentation
  delivered here.

## 3. Requirements

- **R1 — The Call CTA is gated on active subscription.** As an org member, I want the CTA
  to reflect whether my org can place a call.
  - *Given* `subscription_status === 'active'` for at least one of the member's orgs, *then*
    the Call CTA is visible, following the existing `ensureBizOrgs`/`updateWorkspaceLink`
    gating pattern rather than a new one.
  - *Given* no org has `subscription_status === 'active'`, *then* the CTA is not rendered.

- **R2 — Any MEMBER may dial.**
  - *Given* a signed-in user with role `MEMBER` or higher in an active org, *when* they use
    the CTA, *then* they may dial; no `ADMIN`-only restriction is added client-side.

- **R3 — Microphone permission is acquired before the call is placed.** As a subscriber, I
  must never be billed for a call carrying no audio.
  - *Given* the user presses Call, *when* the app requests microphone access, *then* the app
    MUST dial only after permission is granted and a live audio track is acquired.
  - *Given* permission is denied, or audio acquisition fails, *then* the app MUST NOT place
    the call.
  - *Given* the call was placed and the audio track later fails, *then* the app hangs up
    rather than continuing an audio-less call.

- **R4 — One-click entry skips the device-check prejoin.**
  - *Given* the user presses Call, *when* dialing succeeds, *then* the app SHALL enter the
    existing call screen directly, skipping the camera/device-check prejoin screen used for
    ordinary video-room joins.

- **R5 — Leaving ends the call; no keep-alive.**
  - *Given* a phone call is in progress, *when* the user navigates away or closes the tab,
    *then* the call ends; no reconnect or keep-alive attempt is made.

- **R6 — `leaveCall()` stays the single idempotent reset point (regression: 1.58.5).**
  - *Given* the phone entry path ends through any exit (hangup, dial failure, R5's
    navigate-away, or normal completion), *then* `leaveCall()` unconditionally and
    idempotently hides both `prejoinScreen` and `callScreen` and shows `homeScreen`.
  - *Given* an ordinary video-room call ends, *then* the same `leaveCall()` behavior is
    unchanged — no stale prejoin panel resurfaces under the session screen; the 1.58.5 bug
    class is not reintroduced.

- **R7 — Ordinary video-room join/leave has zero behavior change.**
  - *Given* a user joins or leaves a normal video room with no phone dial involved, *then*
    the existing `goPrejoin`/`startCall`/`leaveCall` flow behaves exactly as before.

- **R8 — Contacts are reachable from the dial UI.**
  - *Given* the org's address book (spec 0114) has contacts, *when* the user opens the dial
    UI, *then* they can pick a contact, prefilling its number, preselecting that number's own
    language, and offering its linked project.
  - *Given* the org has no contacts yet, *then* the dial UI still allows dialing a raw
    number; the contact picker is merely empty, not broken.

- **R9 — Every new string ships in all 84 locales.**
  - *Given* any new user-facing string introduced by this change, *then* it MUST exist as a
    key in all 84 files under `client/src/scripts/i18n/`, per root `CLAUDE.md`'s
    all-or-nothing i18n rule — no English-only placeholder, no partial rollout.

- **R10 — Server-side refusals are respected even when the client gate passed.** The client
  gate is UX only, never the security boundary.
  - *Given* the client shows the CTA (R1) but the server's own subscription/role re-check at
    dial time fails, *then* the app surfaces the refusal and does not place the call.
  - *Given* the org has an active subscription but no verified caller ID yet, *then* dialing
    is refused with that reason surfaced to the user.
  - *Given* a quote/rate is unavailable for the destination (e.g. an empty `voip_rates`
    table, as on staging today), *then* dialing is refused rather than attempted; this is a
    known data gap for verification, not a client defect.
  - *Given* the user's browser does not support WebRTC, *then* the CTA flow surfaces that
    limitation instead of attempting to dial.

## 4. Design & Architecture

Client-only, additive. Two new modules (`voip.ts` API client, `phone-dialer.ts` ported pure
logic) plus an `entryMode`-guarded branch through the EXISTING `startCall`/`leaveCall`
machine. Verified against the real server: `server/src/voip/routes.rs` quote/dial/hangup and
`contacts.rs` needed no change.

**Code-verified findings that shaped this design**

1. The telephone is a full room peer (`voip/session.rs:150` `create_phone_peer`, name
   `"Phone"`, id `phone-<32hex>`, lang = recipient's). Without special-casing, `room_joined`
   would `addCell` it and `mesh.addPeer(id, false)`, leaving a permanently black tile and a
   dead PeerConnection.
2. No server logic hangs up the PSTN leg when the human leaves the room — `leaveCall()` only
   closes the WS. R5 requires the CLIENT to POST hangup on leave and on `pagehide`, or a
   billed call runs to `max_call_minutes`.
3. `client/src/scripts/i18n.test.ts` already asserts every locale has EXACTLY the `en` key
   set, placeholder parity and no empty values — the i18n review gate for the locale slice.
4. `t()` falls back to `en`, so a missing locale renders English, not a key name.

**Architecture decisions**

| Decision | Choice | Rejected | Rationale |
|---|---|---|---|
| Result envelope | `ApiResult<T> = {ok,status,data}` copied from `dashboard/src/lib/api.ts` into `voip.ts`, + `errorCode()` reading `{error}` | `business.ts`'s swallow-and-return-`[]` style | Refusal codes are the whole UX here; a call that spends money may not fail silently |
| Pure logic home | Port to `client/src/scripts/phone-dialer.ts` + `phone-dialer.test.ts` | Import across `dashboard/` (separate git repo); shared package | Port the dashboard's own test cases verbatim to bound drift |
| Refusal key shape | `phoneReason` + PascalCase(code) → `phoneReasonNoAnswer`; unknown → `phoneReasonGeneric` | Dashboard's dotted `phone.reason.*` | `client/` i18n is flat camelCase (`bizRecord`, `webinarTier`) |
| Entry path | `entryMode: 'room'\|'phone'` module flag; phone branch reuses `startCall()` unchanged | A 4th screen; a forked `startPhoneCall` copy of `startCall` | Inherits the 1.58.5 idempotent reset; a 4th screen reintroduces exactly that bug class |
| Error sink | `entryError(key)` writes to `prejoinStatus` (room) or the dial panel status (phone); 3 call sites in `startCall` | Duplicating `startCall`'s guards | `startCall`'s 3 early returns write into a hidden `#prejoin` on the phone path |
| Phone tile | Keep `addCell(phoneId,…)`, skip `mesh.addPeer`, restyle via `data-phone` | Suppress the cell and fork subtitle rendering | Subtitles/speaking/mute all target the cell; forking them duplicates the hardest part |
| Money-safety extraction | Extracted mic-before-dial + idempotent-hangup + entry-mode decision into a framework-free, DI module `phone-call.ts` | Leaving them untested inside `app.ts` | `vitest.config.ts` pre-existingly excludes `app.ts` from unit coverage (e2e-only, per project convention); the two money-safety invariants needed real RED→GREEN proof |
| Locale generation | One-off Groq script kept in the session scratchpad, NOT committed | Committing a translation script; inline agent authoring of 83×62 strings | Matches prior features; the repo has no such script and should not gain an unowned one |

**Entry path — reused vs. bypassed**

Reused as-is from `goPrejoin`: `session = {...}`, `writeCache`/`persistLang`,
`ensureCallModules()`, `loadLocale(getUiLang())`, `stopLobby()`, then all of `startCall()`.
Bypassed: every prejoin DOM write, `setupBizPrejoin`, `syncVoicePrep`,
`syncAudioSettingsBtn`, `acquireMedia()` (its `videoConstraints()` opens the camera),
`populateDevices()`, `camOn = true`. Replaced by `acquireMicOnly()`:
`getUserMedia({ audio: buildAudioConstraints(...) })` plus the same denoiser/`rawMeetStream`
handling, no `previewVideo`; sets `micOn=true, camOn=false`.

`leaveCall()` gains exactly one line, `void endPhoneLeg();` — nulls `activePhoneCall` first
(idempotent), clears the poll timer, and POSTs `/voip/calls/{id}/hangup` with
`keepalive: true`. Everything else in `leaveCall` was already unconditional, so the phone
path needed no special-casing and inherits 1.58.5 (R6/R7). A `pagehide` listener calls the
same function (R5; `keepalive` fetch, not `sendBeacon`, because the Authorization header is
required). Best-effort: a crashed tab still relies on the server's `max_call_minutes`
backstop.

**Mic-before-dial sequencing (R3, non-negotiable order)**

```
CTA click ──▶ inline dial panel (home screen, NOT a modal)
  │ contact picked / number typed ─ debounced looksDialable ─▶ POST /voip/quote ─▶ price shown
  ▼ press Call
1 getUserMedia({audio})  ── denied/failed ─▶ phoneMicRequired · STOP · NO dial · no charge
2 POST /voip/quote       ── {error:code}  ─▶ refusal copy · release mic · STOP
3 POST /voip/calls       ── {error:code}  ─▶ refusal copy · release mic · STOP
  │ 201 {call_id, session_id, room, status}
4 entryMode='phone' · session={room,lang,…} · startCall()   (no prejoin, no camera)
5 WS join ─▶ room_joined carries peer "phone-…" ─▶ phone cell, NO mesh.addPeer
6 poll GET /voip/calls/{id} @1500ms ─▶ phaseFromStatus ─▶ label + aria-live announcement
7 leave btn | terminal phase | pagehide ─▶ POST hangup(keepalive) ─▶ leaveCall()
```

Track loss after step 4 (`ended` on the audio track) hangs up rather than billing silence.

**CTA + dial panel**

- `#phone-cta` card on the home screen, revealed inside `updateWorkspaceLink()` by the same
  `ensureBizOrgs()` + `subscription_status === 'active'` gate, additionally
  `&& webrtcSupported()` so `startCall`'s own WebRTC guard is unreachable on this path (R10).
- The card expands INLINE into the dialer (no modal — price and disclosure must stay visible
  while typing).
- Contacts (R8): the destination field IS the contact search — letters debounce into
  `listVoipContacts({q})` in a listbox; digits are a raw number. Picking fills number +
  that number's own `language` + its linked project. Empty address book degrades to a plain
  number field.
- Secondary inputs (their language, project, caller ID) sit behind a `phoneDetails`
  disclosure, prefilled; the primary action stays one press. Price, disclosure summary and
  `willAskConsent` copy render above the button before the press, so pressing Call IS the
  confirmation — no second confirm step.

**In-call phone presentation**

`callScreen` gets `data-entry="phone"`; `videoGrid.dataset.mode = 'phone'`. The phone cell
keeps its DOM (subtitles/speaking/mute machinery untouched) but renders as one centred call
card: contact name/masked destination, `tabular-nums` destination, the language pair with
flags, and the phase as text plus a dot (never colour alone) — `announcement(prev,next,t)`
drives an `aria-live` region. The `<video>` element is hidden; the avatar slot holds a phone
glyph over `avatarGradient(name)`, ringed while the phone peer is the active speaker (reuses
the room's existing speaking signal). Duration comes from the poll's server
`duration_seconds` through the ported `formatDuration`, never a local clock. Controls keep
mic-mute and the red leave button (relabelled `phoneHangUp`); camera, screen share, PiP,
fullscreen, whiteboard, minigame, quiz, reactions, hand-raise, chat and room-record are
hidden — a telephone cannot receive any of them.

## 5. Implementation

| Slice (PR) | Branch | What | Key files | Actual lines |
|---|---|---|---|---|
| 1a | `feature/web-app-voip-dialer-01a-api-client` | Typed API client | `client/src/scripts/voip.ts`, `voip.test.ts` | 500 |
| 1b | `feature/web-app-voip-dialer-01b-phone-dialer` | Ported pure dial logic | `client/src/scripts/phone-dialer.ts`, `phone-dialer.test.ts` | 690 |
| 2a | `feature/web-app-voip-dialer-02a-phone-call-controller` | Money-safety module (mic-before-dial, idempotent hangup, entry-mode decision) | `client/src/scripts/phone-call.ts` + test, `voip.ts` keepalive option | 279 |
| 2b | `feature/web-app-voip-dialer-02b-entry-path` | `app.ts` phone entry wiring | `client/src/scripts/app.ts` | 186 |
| 3 | `feature/web-app-voip-dialer-03-cta-dial-panel` | CTA, inline dial panel, refusal-key surfacing, in-call presentation, `en.json` | `app.ts`, `index.astro`, `en.json`, `phone-dialer.ts` | 626 |
| 4 | `feature/web-app-voip-dialer-04-locales` | 83 generated locale files | `client/src/scripts/i18n/<locale>.json` × 83 | 5312 |
| 5 | `feature/web-app-voip-dialer-05-regression` | Full regression + new e2e proof | `client/e2e/phone-dialer.spec.ts` | 229 |

All branches are chained off tracker `feature/web-app-voip-dialer`, which branches off
`develop`. Total: 92 files changed, 7485 insertions, 90 deletions — entirely under `client/`
(`git diff --stat feature/web-app-voip-dialer..HEAD -- server dashboard` is empty, confirmed
independently at every phase boundary and again at HEAD).

**PR-size exceptions.** PR1 (1190 lines combined across 01a/01b) and PR3 (626 lines) exceed
the 400-line review budget; both `size:exception` classifications were reviewed and
explicitly approved by the repo owner via the standard batch-approval flow. PR2 (465 lines
combined, split into two sub-400 branches) needed no exception. PR4 (5312 lines) is exempt
from the line-count budget by the tasks-phase forecast: it is reviewed by the `i18n.test.ts`
parity gate (every file parses, carries exactly the `en` key set, no empty values), not by
line count.

## 6. Testing & Verification

Independently re-verified by `sdd-verify` against HEAD of
`feature/web-app-voip-dialer-05-regression` in strict-TDD mode. **All 10 requirements
(R1–R10): PASS. CRITICAL: 0 · WARNING: 0 · SUGGESTION: 0.**

- **Unit** (`npm run test:unit`): 103/103 files, 2059/2059 tests passing. Coverage 93.8%
  stmts / 86.3% branch / 93.35% funcs / 95.37% lines — meets the project's ≥85% lines/funcs
  gate. Byte-identical between the apply agents' reported numbers and the independent
  verify re-run.
- **Type/lint** (`npm run check`, astro check, 122 files): 0 errors, 0 warnings, 3
  pre-existing hints.
- **i18n parity** (`npm run test:unit -- i18n`): 270/270 passing across 2 files; 84 locale
  files independently spot-checked outside the test suite for exact `en.json` key-set
  parity (878 keys each) and zero empty values.
- **Structural boundary**: `git diff --stat feature/web-app-voip-dialer..HEAD -- server
  dashboard` → empty (R7 proof — zero bytes touched outside `client/`).
- **E2E** (`client/e2e/phone-dialer.spec.ts`, new): written and structurally exercised
  against the real built app — CTA visibility, dial-panel open, contact search/quote
  debounce, and the Call-button enabling on a priced quote were all directly observed
  working. The flow blocks at the `getUserMedia()` step in this project's specific
  execution sandbox, which cannot complete ANY `getUserMedia`-dependent Playwright flow —
  independently corroborated via an isolated bare-page repro (no application code involved)
  and by 10+ unrelated, pre-existing specs (`prejoin`, `call`, `billing`'s call tests,
  `screenshare` ×2, `talk`'s join/a11y tests, `room-full`, `whiteboard-frame`, `meet-ui`,
  `mobile-overlays`, `a11y`'s in-call check) failing identically. This is a disclosed
  sandbox/environment limitation, not a regression or a defect in the new test; it was
  confirmed by direct code reading rather than execution. Whoever next runs this in an
  environment with working fake-camera/mic support (CI, or a capable local machine) is the
  first real execution of the new test.
- **Existing e2e regression** (R7): every `getUserMedia`-independent spec passed with no
  observed regression; every `getUserMedia`-dependent spec hit the identical pre-existing
  sandbox limitation above.
- **Task-completion spot-check**: verify independently read `phone-dialer.ts`/`voip.ts`
  (Phase 1), `phone-call.ts` + its test (Phase 2 money-safety invariants — exactly-once
  hangup proven via a double-`end()` call assertion), CTA/contact/refusal wiring in `app.ts`
  (Phase 3), and the new e2e spec in full (Phase 5). No checkbox was found to overstate its
  actual state.

**Open, human-owned item (does not block this archive):** task 5.5 — a manual production
smoke test (active org + verified caller ID: real dial reaches a translated call, no stale
prejoin, ordinary video-room join/leave unaffected). Staging's `voip_rates` table is empty,
so a real dial can only be validated in production; this is a known, disclosed data gap, not
a client defect (R10). Owner: repo maintainer, production environment only, after delivery.

## 7. Deployment & Operations

No migration, no server deploy, no env var. Nothing under `server/` or `dashboard/` changed
(confirmed structurally, see §6). Delivery is independent of this archive: as of archive
time, the 7-branch chain (`01a` → `01b` → `02a` → `02b` → `03` → `04` → `05`, tracker
`feature/web-app-voip-dialer` off `develop`) exists only locally and has not been pushed,
opened as PRs, or merged — that is a separate, subsequent step under ordinary Git Flow
policy (feature branch off `develop`, `--no-ff` merge, prune after merge).

**Rollback plan.** Revert the feature branch chain; nothing persists. The CTA is additive
and gated, so reverting only the `index.astro` + CTA-gating hunk hides the entry point while
leaving `voip.ts`/`phone-call.ts` inert — the fast partial rollback. The dashboard dialer is
untouched and remains the fallback path throughout.

## 8. Risks / Open Items

| Risk | Status |
|---|---|
| Regressing the video-room path in `app.ts` | Mitigated — additive branch, TDD, structural zero-diff outside the phone-specific hunks, R7 independently verified PASS |
| 84-locale bulk swamping the review budget | Mitigated — isolated into its own PR (4), reviewed by i18n parity, not line count |
| Ported dial logic drifting from the dashboard's | Mitigated — same test cases ported verbatim; both files noted here for future drift checks |
| Mic permission denied after dial → paid call with no audio | Mitigated — mic acquired and a live audio track confirmed strictly before `POST /voip/quote`/`/voip/calls` (R3, independently verified) |
| Navigate-away drops a billed call | Accepted (no keep-alive is the confirmed design; `pagehide` best-effort hangup + server `max_call_minutes` backstop) |
| New e2e spec (`phone-dialer.spec.ts`) not yet proven green in any environment | Open — blocked on this session's sandbox-wide `getUserMedia` limitation (disclosed, not a code defect); needs a first real execution in CI or a capable local environment |
| Task 5.5 — manual production smoke test | Open, human-owned; not attempted by any automated agent; the last item before this feature can be called fully verified end-to-end |
| PR1 / PR3 `size:exception` | Resolved — repo owner explicitly approved both via the standard batch-approval flow |

## 9. References

- Commits: `96158399`, `338243d5`, `59214277`, `39d21a6d`, `bba1d52c`, `f6fcabe0`, `ec66e535`
- Branches: `feature/web-app-voip-dialer` (tracker) and its chain `-01a-api-client` →
  `-01b-phone-dialer` → `-02a-phone-call-controller` → `-02b-entry-path` →
  `-03-cta-dial-panel` → `-04-locales` → `-05-regression`
- Files: `client/src/scripts/voip.ts`, `client/src/scripts/phone-dialer.ts`,
  `client/src/scripts/phone-call.ts`, `client/src/scripts/app.ts`,
  `client/src/pages/index.astro`, `client/src/scripts/i18n/*.json` (84 files),
  `client/e2e/phone-dialer.spec.ts`
- Server contracts (read-only, unchanged): `server/src/voip/routes.rs`,
  `server/src/voip/session.rs`, `server/src/voip/contacts.rs`
- SDD artifacts (Engram, project `voxtranslate`): `sdd/web-app-voip-dialer/proposal` (obs
  #2159), `sdd/web-app-voip-dialer/explore` (obs #2155), `sdd/web-app-voip-dialer/spec` (obs
  #2160, draft-phase record — superseded by this document), `sdd/web-app-voip-dialer/design`
  (obs #2162), `sdd/web-app-voip-dialer/tasks` (obs #2165), `sdd/web-app-voip-dialer/apply-
  progress` (obs #2168), `sdd/web-app-voip-dialer/verify-report` (obs #2169),
  `sdd/web-app-voip-dialer/archive-report` (this closure record)
