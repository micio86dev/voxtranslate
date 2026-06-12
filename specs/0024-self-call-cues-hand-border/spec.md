# 0024 — Self Call Cues + Raised-hand Tile Border

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-12 |
| **Shipped** | 2026-06-12 |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0002](../0002-video-calls-translated-chat/spec.md), [0020](../0020-session-sound-cues-sticky-reactions/spec.md) |

## 1. Context & Problem

Two small in-call polish items:

1. **No cue for your *own* join/leave.** [Spec 0020](../0020-session-sound-cues-sticky-reactions/spec.md)
   added cues for *other* people arriving/leaving but explicitly made a **self**
   cue a non-goal ("you initiated those"). In practice a Google-Meet-style **beep
   when you enter and leave a call** is reassuring feedback that the action landed —
   the user asked for it, so we're reversing that non-goal.
2. **Raised-hand border only showed in PiP.** `setHandIndicator()` already toggles
   a `.hand-raised` class on the tile and the CSS set
   `outline: 2px solid var(--warning)` — but an **outline gets composited under the
   hardware video layer** (`.video-cell` has `transform: translateZ(0)` and the
   tile holds a live WebRTC feed), so the amber border was invisible in the grid.
   It *did* show in the Document-PiP window, whose cloned tiles render through a
   different (non-overlay) path — hence "I only see it on the picture-in-picture".

## 2. Goals / Non-Goals

**Goals**
- A distinct **beep on self-join** and **self-leave** (Meet-style), separate from
  the peer join/leave chimes.
- The raised-hand **amber border shows on the grid tile**, on top of the video.

**Non-Goals**
- Changing the peer join/leave/hand cues (0020) — those stay as-is.
- A self cue on reconnect suppression (a re-`room_joined` will re-beep; acceptable).
- Reworking the active-speaker outline (same latent compositing quirk, not reported).

## 3. Requirements

- **R1 — Self-join beep.** *When* `room_joined` arrives, *then* a rising two-note
  triangle beep plays.
- **R2 — Self-leave beep.** *When* I leave a call I had actually joined, *then* a
  falling two-note triangle beep plays. *Given* a room-full bounce (never joined),
  *then* no beep (guarded on `callStartedAt > 0`).
- **R3 — Distinct from peer cues.** The self beeps use a different register/timbre
  (A4↔E5 triangle) than the peer join/leave chimes (D5/A5 sine).
- **R4 — Hand border visible.** *Given* a participant (self or peer) raised their
  hand, *then* their grid tile shows an amber border drawn **over** the video.

## 4. Design & Architecture

- **Components / files:**
  - `client/src/scripts/sfx.ts` — `playCallEnterSound()` (A4→E5 rising, triangle,
    gain 0.08) and `playCallLeaveSound()` (E5→A4 falling). Reuse the shared lazy
    `AudioContext` + click-free `play()` envelope.
  - `client/src/scripts/app.ts` — `playCallEnterSound()` in the `room_joined`
    case; `playCallLeaveSound()` at the top of `leaveCall()`, guarded by
    `callStartedAt > 0` so a non-entry (room-full) doesn't beep.
  - `client/src/pages/index.astro` — replace the `.video-cell.hand-raised`
    `outline` with a `::after` overlay (`position:absolute; inset:0; border:3px
    solid var(--warning); z-index:4; pointer-events:none`) so the border paints
    above the composited video feed, matching the cell's 10px radius.
- **Key decisions:**
  - *Overlay, not outline* — an `outline`/`border` on the cell sits under the
    hardware video layer; an absolutely-positioned `::after` over the
    `position:relative` cell paints above it. This is the reliable cross-browser
    way to frame a live video.
  - *Guard the leave beep on `callStartedAt`* — the single reliable "we were in a
    call" flag (set in `room_joined`, cleared in `leaveCall`), so error bounces
    that call `leaveCall()` without an entry stay silent.
  - *Triangle in a lower register for self* — audibly distinct from the sine peer
    chimes so you can tell "me" from "someone else".

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Two self cues + unit tests | `sfx.ts`, `sfx.test.ts` |
| S1 | Wire enter (`room_joined`) + guarded leave (`leaveCall`) | `app.ts` |
| S2 | Hand-raised border as a `::after` overlay | `index.astro` |

## 6. Testing & Verification

- **Unit (`sfx.test.ts`):** `playCallEnterSound` → two triangle tones, rising;
  `playCallLeaveSound` → two triangle tones, falling. Pins R1–R3.
- `astro check` clean; **100/100** client unit tests; production build OK.
- Manual: join a room → enter beep + (with a hand raised) the amber border on the
  tile in the grid, not just PiP; leave → leave beep; room-full → no beep.

## 7. Deployment & Operations

- Client-only change → ships with the Vercel autodeploy on `main`. No server
  change, no env vars, no migration.

## 8. Risks / Open Items

- The active-speaker outline (`var(--success)`) has the same compositing quirk and
  could be migrated to the same `::after` pattern if it's reported.

## 9. References

- Reverses 0020's self-cue non-goal at the user's request.
- Files: `client/src/scripts/sfx.ts`, `client/src/scripts/app.ts`,
  `client/src/pages/index.astro`, `client/src/scripts/sfx.test.ts`.
