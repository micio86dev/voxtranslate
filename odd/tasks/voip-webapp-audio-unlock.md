# voip-webapp-audio-unlock

## Objective
Fix a live production bug: on a webapp-initiated VoIP call (entryMode='phone'), the
phone party hears the translated audio fine, but the web caller hears NOTHING when the
phone party speaks.

## Problem
User report (2026-09-20, real production call, voip_calls.id
8a9e5cdb-97d7-419f-aefc-f03cd65cc16c): phone side hears translated realtime audio
correctly; web side (the caller) hears total silence.

Investigated and ruled out first:
- **Not byte-order corruption**: prod Railway logs for this exact call show
  `phone leg L16 byte order detected` / `summary: decided`, `byte_order=little-endian`,
  converged in ~2s (5 frames) — yesterday's hotfix 1.60.2 (`e930e392`) fix holds. Byte
  order only affects phone→server decode anyway, not server→browser delivery.
- **Not yesterday's hotfix 1.60.2 changes** (`71d4ccb7`, hangup broadcast): diffed —
  zero occurrences of `pcmPlayback`/`AudioContext`/`unlock` in those commits.
- **Not WebRTC mesh/routing**: translated audio never uses WebRTC — it's a
  `ServerMessage::TranslatedAudio` JSON frame over the existing WS, broadcast via
  `rooms::broadcast_to_lang`/`broadcast_to_lang_engine` (server/src/rooms.rs), the same
  mechanism for every peer regardless of phone/web. No errors in prod logs at all.

**Root cause (code-confirmed)**: `client/src/scripts/pcm-playback.ts`'s `PcmPlayback`
plays translated audio through a Web Audio `AudioContext` that browsers keep
`suspended` until resumed inside a user-gesture call stack. The app already knows this
— `unlockTts()` + `pcmPlayback.unlock()` are called from the ordinary prejoin
`join-btn` click handler (`client/src/scripts/app.ts:2071-2072`) and the TTS-toggle
handler (`:4222-4223`) for exactly this reason.

The webapp phone dialer's call path — `phoneDialPanel` submit → `placePhoneCall` →
`enterPhoneCall` → `startCall()` — bypasses the prejoin screen entirely (documented in
`enterPhoneCall`'s own comment: "reuses `startCall()` unchanged, bypassing every
prejoin-only step") and NEVER calls `unlockTts()`/`pcmPlayback.unlock()`. So the
`AudioContext` is only ever lazily created async, off the user gesture, when the first
`translated_audio` WS message arrives (`pcm-playback.ts:71-78`) — its `ctx.resume()`
there is wrapped in `.catch(() => {})` and fails silently in that context. The phone
leg's own audio is immune by construction: it never touches a browser `AudioContext`
at all (delivered straight through `media::pump` to the PSTN leg, `session.rs`).

This bug predates yesterday's hotfix — introduced when the web-app VoIP dialer was
originally built (`bba1d52c`, "wire the phone CTA, dial panel").

## Fix
`client/src/scripts/app.ts`: added `unlockTts(); pcmPlayback.unlock();` at the top of
the `phoneDialPanel` `submit` handler (before any async work), mirroring the exact
pattern already used by `join-btn`. One synchronous user-gesture call site, two
already-tested functions — no new pure logic to unit-test.

## Constraints
- No server-side change; server audio delivery path is confirmed correct.
- No new i18n keys.

## Tasks
- [x] 1. Investigate and confirm root cause (code + prod log evidence, ruled out
      byte-order and yesterday's hotfix)
- [x] 2. Wire `unlockTts()` + `pcmPlayback.unlock()` into the phone-dial submit handler
- [x] 3. Full client test suite green (2074/2074), typecheck clean (0 errors)
- [ ] 4. Manual/e2e browser verification — NOT done: no existing e2e harness asserts
      `AudioContext` state, and building one is out of scope for this hotfix (flagged,
      not silently skipped). Final proof needs one real production call, same as
      yesterday's byte-order fix.
- [x] 5. Commit `7dd9e420` on `hotfix/1.60.4`, PR #422 → `main`, CI green (test,
      check-secrets, security-audit, e2e), merged `361fa17c`, tagged `v1.60.4`. Webapp
      prod deploy confirmed via Vercel: `dpl_CD48Eq6CBdF985bwyEz7bUszyDJu`, target
      production, status Ready, aliased `https://app.voxtranslate.app`.
      `deploy-server`/`deploy-media` correctly skipped (no `server/` files touched).
- [x] 6. Back-merged `main` → `develop` (`43dd2bc9`, no-ff), pushed. First CI run
      (35511861323) had ONE unrelated e2e failure —
      `e2e/screenshare.spec.ts:73 "screen share works without a camera (issue #4)"`,
      `#btn-share never became clickable` — zero overlap with this change (no
      screenshare/webrtc-mesh files touched) and the SAME commit content already passed
      e2e on PR #422 before merge; `deploy-staging` doesn't depend on e2e and had
      already succeeded independently. Reran the failed job (`gh run rerun --failed`):
      green on retry, confirming pre-existing flake, not a regression. Full run now:
      test/check-secrets/security-audit/e2e/deploy-staging all success.
      Hotfix branch pruned locally and on remote after merge.
- [ ] 7. Ask the user to re-test the same call in production and confirm they now hear
      the phone party — reported, awaiting their live confirmation.

## Verification evidence
(filled in as work proceeds)
