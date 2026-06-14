# 0057 — Picture-in-Picture: in-window controls + discoverability

| | |
|---|---|
| **Status** | In progress |
| **Owner** | VoxTranslate |
| **Created** | 2026-06-14 |
| **Shipped** | — |
| **Version** | — |
| **Commits** | `<pending>` |
| **Depends on** | [0002](../0002-video-calls-translated-chat/spec.md), [0033](../0033-screenshare-pan-zoom/spec.md), [0053](../0053-screenshare-camera-pip/spec.md) |

## 1. Context & Problem

The Document Picture-in-Picture window (desktop Chromium) pops the call out into a floating
window so it stays visible while you work in other tabs. Today that window is a **display-only**
clone of the video stage (`btnPip` → `documentPictureInPicture.requestWindow()` → clones
`.video-stage`, re-attaches muted MediaStreams). Issue #73:

1. **No controls in PiP.** Once popped out you can't mute, toggle camera, share, raise your hand,
   or end the call without returning to the main tab — the PiP window is useless for anything but
   watching.
2. **"Auto-activation feels unexpected."** The issue reports PiP appearing automatically on page
   change. **It doesn't** — there is no `visibilitychange`→PiP code, and Document PiP *cannot*
   auto-open: `requestWindow()` requires transient user activation (a gesture), by spec. So PiP
   never surprises the user. The real gap is **discoverability** — users don't know the option
   exists. We address that with a one-time hint instead of (impossible, and unwanted) auto-popup.

## 2. Goals / Non-Goals

**Goals**
- A control bar inside the PiP window: **mute mic, toggle camera, screen-share, raise hand, end
  call** — each reflecting live state and driving the *same* logic as the main control bar.
- Closing the PiP window or leaving the call tears the controls down cleanly.
- A one-time, unobtrusive hint that the call can be popped out to PiP (discoverability), shown when
  the user returns from working in another tab during a call.

**Non-Goals**
- Replicating the full ⋯ overflow menu inside PiP (TTS, blur, record, whiteboard, mini-games…) —
  the five core controls cover the real need; the rest stays in the main window.
- Auto-opening PiP on tab switch — technically impossible (no user gesture) and explicitly the
  "unexpected" behaviour the issue dislikes.
- Mobile PiP (Document PiP is desktop-Chromium only, already gated by `btnPip` visibility).

## 3. Requirements

- **R1 — PiP controls drive the call.** *Given* the PiP window is open, *when* I click its mic /
  camera / share / hand / end buttons, *then* the same action fires as on the main bar (mute,
  camera, screen-share, hand-raise, leave), affecting all peers.
- **R2 — PiP controls reflect live state.** *Given* a state change anywhere (main bar, voice
  command, a peer event), *when* it lands, *then* the matching PiP button updates its icon/active
  state — both directions stay in sync via `setControlState()`.
- **R3 — Clean teardown.** *Given* I close the PiP window (its ✕ or the ⊞ toggle) or leave the
  call, *then* the PiP controls are dropped and no stale references remain.
- **R4 — End call from PiP.** *Given* the PiP window, *when* I click its end-call button, *then*
  the call ends and the PiP window closes (no orphaned floating window).
- **R5 — Discoverability hint.** *Given* I'm in a call on a PiP-capable browser and haven't seen
  the hint, *when* I switch away to another tab and come back, *then* a one-time toast tells me I
  can keep the call visible via Picture-in-Picture; it never repeats (localStorage-gated).

## 4. Design & Architecture

- **Components / files:**
  - `client/src/scripts/app.ts`:
    - Extract the inline mic and hand handlers + the share toggle into named
      `toggleMicrophone()` / `toggleHand()` / `toggleScreenShare()` (no behaviour change) so both
      the main buttons and the PiP buttons share one path.
    - `buildPipControls(w)` — append a `.pip-controls` bar to the PiP `document` with five
      `.pip-ctl-btn`s wired to those functions (`end` → `leaveCall()`, which already closes PiP).
      Refs stored in module-level `pipCtl`.
    - `syncPipControls()` — mirror `micOn`/`camOn`/`isSharingScreen`/`handRaised` onto the PiP
      buttons; called at the end of `setControlState()` and on PiP open. Cleared (`pipCtl = null`)
      on PiP `pagehide`, on the ⊞ toggle-close, and in `leaveCall`.
    - A `visibilitychange` listener for the R5 hint (gated by call-active + PiP-supported +
      not-already-open + `localStorage` flag).
  - `client/src/pages/index.astro` — `:global(.pip-controls)` / `:global(.pip-ctl-btn)` CSS.
    **Must be `:global`**: the PiP window copies `document.styleSheets`, but buttons created in
    `w.document` carry no Astro scope attribute, so scoped rules wouldn't match them.
  - `client/src/scripts/i18n.ts` — one new key `pipHint` ×8 languages; PiP button titles reuse
    existing `muteTip`/`camTip`/`screenShareTip`/`handTip`/`leaveTip`.
- **Cross-window calls:** the PiP buttons live in `w.document` but their click listeners are
  closures in the *main* window's script (same JS realm), so they call `toggleMicrophone()` etc.
  directly and mutate the main call state — no message passing.
- **Key decisions:**
  - **Reuse `setControlState()` as the single sync point** → the PiP buttons can never drift from
    the main UI; one render path feeds both.
  - **Hint, not auto-PiP** → honours the issue's "visual feedback" ask while respecting the API's
    user-gesture requirement and avoiding the surprise pop-up users dislike.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Extract `toggleMicrophone` / `toggleHand` / `toggleScreenShare` | `app.ts` |
| S1 | `buildPipControls(w)` on PiP open + `:global` CSS | `app.ts`, `index.astro` |
| S2 | `syncPipControls()` from `setControlState` + teardown | `app.ts` |
| S3 | R5 discoverability hint + `pipHint` i18n ×8 | `app.ts`, `i18n.ts` |

## 6. Testing & Verification

- **Automated:** `astro check`, `astro build`, the 146-test unit suite, and the a11y e2e stay green.
- **Manual (desktop Chromium):** open PiP → its bar shows mic/cam/share/hand/end; muting on the
  main bar flips the PiP mic icon and vice-versa (R1/R2); raise hand from PiP → peers see the hand;
  end-call from PiP closes the window and leaves (R4); close PiP → controls gone, reopen works (R3);
  in a call, switch tab + return → one-time PiP hint appears, never again (R5).
- **No new unit test:** behaviour is cross-window DOM wiring over existing, tested call logic
  (`toggleMicrophone` etc. are extractions); covered by the manual pass per the spec 0053 convention.

## 7. Deployment & Operations

- Client-only, desktop-Chromium-gated. No env vars, migrations, or server changes. Vercel
  auto-deploys on `main`.

## 8. Risks / Open Items

- `startScreenShare()` from a PiP-window click relies on that click counting as the transient
  activation `getDisplayMedia` needs; expected to hold (it's a real user gesture) — verify in the
  manual pass.
- The hint uses `localStorage` (per-browser, per-origin); a user who clears storage sees it again.
  Acceptable for a one-time tip.

## 9. References

- Issue: #73 (refs specs 0002, 0033, 0053; Document Picture-in-Picture API)
- Files: `client/src/scripts/app.ts`, `client/src/pages/index.astro`, `client/src/scripts/i18n.ts`
