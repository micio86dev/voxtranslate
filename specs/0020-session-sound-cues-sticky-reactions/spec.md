# 0020 — Session Sound Cues + Sticky Emoji Reactions

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-12 |
| **Shipped** | 2026-06-12 |
| **Version** | — |
| **Commits** | _(this PR — closes #15)_ |
| **Depends on** | [0002](../0002-video-calls-translated-chat/spec.md), [0010](../0010-composite-recording/spec.md) |

## 1. Context & Problem

Issue #15 ("UX Improvements: Sound Feedback + Emoji Reactions Behavior") reports
two gaps in the in-call experience:

1. **Incomplete audio feedback.** A previous change (PR #5) added synthesised UI
   cues in `client/src/scripts/sfx.ts` and wired three of them: a *peer joined*
   chime, a *hand raised* ping, and a *screen-share started* motif. But there is
   still **no cue when a peer leaves** and **no cue when recording starts** — so
   you have to watch the grid to notice either. The composite recorder (spec
   0010) is privacy-relevant and especially deserves an audible marker.
2. **Reactions can't be sent in a sequence.** The emoji panel has two sections:
   *quick reactions* (broadcast to the room, float over the grid) and a *grid*
   that inserts emoji into the chat input. Clicking a quick reaction
   `sendEmoji()` closed the panel, and the click also bubbled to the
   document-level "click outside → close" handler. Net effect: one reaction per
   open. Modern apps (Discord, Zoom) let you fire several in a row.

The sound infrastructure already exists (one shared `AudioContext`, lazily
resumed; cues are short synthesised tones, no audio asset files). This feature
completes the cue set and makes the reaction panel "sticky".

## 2. Goals / Non-Goals

**Goals**
- Audible cue when a **peer leaves** the call.
- Audible cue when **recording starts**.
- Keep the reaction panel **open** after a quick reaction so several can be sent
  in a row.
- **Cap reaction bursts** client-side so a held / runaway click can't flood the
  room (issue #15's "optional rate limiting").

**Non-Goals**
- A user-facing mute toggle / preference for the cues (the `setSfxEnabled`
  switch already exists in `sfx.ts` for a future preference; no UI here).
- Custom / uploadable sounds, or audio asset files (cues stay synthesised).
- A self-join cue or a local-leave cue (you initiated those; cues are for
  *awareness of others*).
- **Server-side** reaction rate limiting — the client cap is proportionate for
  this UX issue; a malicious client bypassing it is out of scope (see §8).

## 3. Requirements

- **R1 — Leave cue.** As a participant, I want to hear a subtle sound when
  someone leaves, so that I notice without watching the grid.
  - *Given* I'm in a call, *when* a `peer_left` message arrives, *then* a gentle
    two-note **falling** chime plays (the inverse of the join chime).
- **R2 — Recording-start cue.** *Given* I start a composite recording, *when*
  `startRecording()` runs, *then* an assertive rising two-note cue plays.
- **R3 — Sticky reactions.** As a participant, I want to send several reactions
  quickly, so that reacting feels fluid.
  - *Given* the emoji panel is open, *when* I click a quick reaction, *then* it
    is broadcast **and the panel stays open**; *when* I click another, *then* a
    second reaction is broadcast.
  - *Given* I click anywhere outside the panel, *then* it still closes.
- **R4 — Burst cap.** *Given* reactions are fired faster than the cap (5 per
  second), *when* the window is full, *then* further sends are dropped silently
  until the window frees up (the panel stays responsive).

## 4. Design & Architecture

- **Components / files:**
  - `client/src/scripts/sfx.ts` — two new synthesised cues:
    `playLeaveSound()` (A5→D5 falling, sine) and `playRecordingStartSound()`
    (G4→D5 rising fifth, triangle, slightly higher gain). Both reuse the
    existing click-free `play()` envelope and the shared lazy `AudioContext`.
  - `client/src/scripts/reaction-rate-limit.ts` — **new** pure `RateLimiter`:
    a sliding window (`max` hits per `windowMs`) with an injectable clock for
    deterministic tests. No DOM, no globals.
  - `client/src/scripts/app.ts` — wiring:
    - `peer_left` case → `playLeaveSound()`.
    - `startRecording()` → `playRecordingStartSound()`.
    - Quick-reaction button handler → `e.stopPropagation()` (so the document
      "close on outside click" handler doesn't fire) + `sendEmoji()`.
    - `sendEmoji()` → no longer closes the panel; gated by a module-level
      `reactionLimiter = new RateLimiter(5, 1000)`.
- **Key decisions:**
  - *Falling chime for leave, mirroring the rising join chime* — the
    rise/fall pairing reads as "arrived / departed" without any words.
  - *Distinct timbre (triangle) + interval for recording* — it must not be
    mistaken for join/leave/screen-share; recording is significant.
  - *Drop-on-overflow, not queue/debounce, for the burst cap* — a dropped click
    is invisible under normal use (you never hit 5/s by hand) and avoids a
    delayed "echo" of reactions firing after you stop clicking.
  - *Client-side cap only* — proportionate to a frontend UX issue; the server
    already broadcasts reactions to everyone including the sender (so the sender
    sees their own float — no extra echo needed).
  - *`stopPropagation` on the reaction button* — the grid buttons already do
    this to stay open; the quick-reaction buttons now match.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Two new sfx cues + their tests | `sfx.ts`, `sfx.test.ts` |
| S1 | Pure `RateLimiter` + tests | `reaction-rate-limit.ts`, `reaction-rate-limit.test.ts`, `vitest.config.ts` |
| S2 | Wire leave + recording cues; sticky panel + cap | `app.ts` |
| S3 | Update the call e2e to assert the sticky panel | `e2e/call.spec.ts` |

## 6. Testing & Verification

- **Unit — `sfx.test.ts`:** `playLeaveSound` schedules two tones, first higher
  than second (falling); `playRecordingStartSound` schedules two triangle tones,
  second higher than first (rising). Pins R1/R2.
- **Unit — `reaction-rate-limit.test.ts`:** allows `max` then drops; refills as
  hits age out of the sliding window; frees exactly one slot when only the
  oldest ages out; window-edge boundary (strictly-greater cutoff); defaults to
  `Date.now()`. Pins R4. (Fully covered; added to the coverage `include`.)
- **E2E — `call.spec.ts`:** after a quick reaction, `#emoji-panel` is still
  visible and a **second** reaction floats on the peer's grid, then a grid emoji
  inserts into the chat input with the panel still open. Pins R3.
- All 98 client unit tests pass; `astro check` clean; production build OK.

## 7. Deployment & Operations

- Client-only change → ships with the Vercel autodeploy on `main`. No server
  change, no env vars, no migration.

## 8. Risks / Open Items

- **No server-side reaction limit.** The 5/s cap is client-side; a modified
  client could still spam. A future hardening could add a per-connection guard
  to the WS `Emoji` handler using the existing `server/src/rate_limit.rs`.
- **No mute UI.** `setSfxEnabled(false)` silences every cue but isn't surfaced;
  a settings toggle is a small follow-up if the cues prove intrusive.

## 9. References

- Issue: #15
- Files: `client/src/scripts/sfx.ts`, `client/src/scripts/reaction-rate-limit.ts`,
  `client/src/scripts/app.ts`, `client/e2e/call.spec.ts`
- Builds on: PR #5 (initial `sfx.ts` cues), spec 0010 (composite recording).
