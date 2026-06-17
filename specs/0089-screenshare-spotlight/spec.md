# 0089 — Auto-spotlight the screen share (zoom-in / zoom-out)

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Micio Dev |
| **Created** | 2026-06-17 |
| **Shipped** | 2026-06-17 |
| **Version** | — |
| **Commits** | `62e6bad` |
| **Depends on** | [0033](../0033-screen-share/spec.md) (screen share + relay), [0055](../0055-meet-like-session-ui/spec.md) (focus/pin view) |

## 1. Context & Problem

When someone shared their screen, their tile stayed a small grid cell for everyone
else. The owner wants the share to take over the view: when a participant shares,
their screen **zooms into the focus view for all participants**, and **zooms back
out** to the grid when they stop — the Zoom/Meet pattern.

## 2. Goals / Non-Goals

**Goals**
- A share auto-focuses the sharer's tile (the existing focus/pin view) for everyone,
  with a zoom-in animation; reverts on stop.
- The pre-share focus (a manual pin) is restored on stop, not lost.

**Non-Goals**
- No new layout engine — reuse `pinnedPeerId` + `layoutVideos()` focus mode.
- No persistent override of manual pinning beyond the share's duration.

## 3. Design

- `app.ts` `spotlightShare(peerId, active)`:
  - active → remember the current focus once (`pinBeforeShare`), set
    `pinnedPeerId = peerId`, re-layout, and add a one-shot `.share-zoom` class to
    the focused tile (zoom-in pop).
  - inactive → restore `pinBeforeShare`, re-layout (tile shrinks back = zoom-out).
  - Guards: ignores a stop for a peer that isn't the current spotlight (handles
    overlapping shares); peer-left already clears `pinnedPeerId`.
- Wired from the `screen_share` relay (remote peers) and from
  `startScreenShare`/`stopScreenShare` (the local sharer, with `myId`).
- `index.astro`: `@keyframes share-zoom-in` on `.main-cell.share-zoom`; honours the
  global `prefers-reduced-motion` reset.

## 4. Testing & Verification

- `astro check` + vitest + build green.
- Manual (owner, real call): when a peer shares, their screen fills the view for
  everyone with a zoom-in; stopping returns to the grid. A manual pin set before
  the share is restored after.

## 5. Risks / Open Items

- Zoom-OUT is the layout returning to the grid (instant), not a mirrored reverse
  animation — can be smoothed later if the owner wants.
- Overlapping shares spotlight the latest one; the earlier sharer's stop is a
  no-op until the latest stops.

## 6. References

- Files: `client/src/scripts/app.ts`, `client/src/pages/index.astro`.
