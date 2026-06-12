# 0021 — Display Fixes: Screen-share Mirroring + Raised-hand Stacking

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-12 |
| **Shipped** | 2026-06-12 |
| **Version** | — |
| **Commits** | _(this PR — closes #17, fixes the raised-hand overlap)_ |
| **Depends on** | [0002](../0002-video-calls-translated-chat/spec.md), [0013](../0013-call-bookmarks/spec.md), [0020](../0020-session-sound-cues-sticky-reactions/spec.md) |

## 1. Context & Problem

Two in-call **display** bugs, both purely client-side rendering (no stream or
protocol change):

1. **Issue #17 — mirrored video.** Our own video tile is flipped with
   `transform: scaleX(-1)` so the camera self-view feels like a mirror — the
   correct, expected behaviour every meeting app uses, and harmless because the
   transform is *display-only* (the outgoing WebRTC track is untouched, so peers
   already see us correctly oriented). **But the same rule also flips a screen
   share**: `startScreenShare()` routes the screen into the self tile via
   `setSelfVideo()`, and the tile keeps its `.self` class, so the shared screen
   renders **mirrored** — any text/UI reads backwards to the presenter. (Issue
   #17 also asked for join/leave sound feedback; that shipped in
   [spec 0020](../0020-session-sound-cues-sticky-reactions/spec.md) via #15 —
   `playJoinSound`/`playLeaveSound` on `peer_joined`/`peer_left`. No further work
   there.)
2. **Raised-hand overlaps the right sidebars.** The animated hand indicator
   (`.hand-indicator`, `position: absolute` inside a `.video-cell` that is
   `transform: translateZ(0)` → its own compositor layer on Chrome) paints **on
   top of** the Participants and Bookmarks side panels when they're open. The
   identical bug was already fixed for the **chat panel** (`abe510be`,
   2026-06-11) by giving `.chat-panel` `position: relative; z-index: 1`; the
   other two right-hand panels never got the same treatment.

## 2. Goals / Non-Goals

**Goals**
- Never mirror a **screen share** (self tile shows it correctly oriented).
- Keep the **camera** self-view mirrored (natural self-preview, unchanged).
- The raised-hand indicator sits **behind** the Participants and Bookmarks panels
  when they're open, matching the chat-panel fix.

**Non-Goals**
- Changing what peers receive — the outgoing track was never mirrored; this is a
  local-display correctness fix only.
- A user toggle for self-view mirroring.
- The composite recorder — it does not mirror any tile (verified: no `scaleX` in
  `client/src/scripts/recording/`), so recordings are already correct.
- Re-doing #17's audio part (already shipped in 0020).

## 3. Requirements

- **R1 — Screen share not mirrored.** As a presenter, I want my shared screen to
  read correctly, so that text isn't reversed.
  - *Given* I start a screen share, *when* the screen appears in my self tile,
    *then* it is **not** flipped (no `scaleX(-1)`); *when* I stop sharing, *then*
    my camera self-view is mirrored again.
- **R2 — Camera self-view still mirrored.** *Given* I'm showing my camera,
  *then* my self tile is mirrored as before.
- **R3 — Hand behind right panels.** As a participant, I want the raised-hand
  emoji to stay within the video area, so that it doesn't bleed over an open side
  panel.
  - *Given* a peer (or I) raised a hand, *when* the Participants or Bookmarks
    panel is open, *then* the hand indicator paints **behind** that panel — same
    as the chat panel.

## 4. Design & Architecture

- **Components / files:**
  - `client/src/pages/index.astro` — CSS:
    - New override `.video-grid :global(.video-cell.self.sharing video) { transform: none; }`
      paired with the existing `.self video { transform: scaleX(-1) }`; a comment
      documents that the flip is display-only and never reaches peers.
    - `.participants-panel` and `.bookmarks-panel` each get
      `position: relative; z-index: 1;` (mirrors the chat-panel fix).
  - `client/src/scripts/app.ts` — `startScreenShare()` adds `.sharing` to the
    self cell; `stopScreenShare()` removes it. The class lives on the same cell
    that already carries the `.screen-share-badge`, so no new lookup.
- **Key decisions:**
  - *A `.sharing` class, not unmirroring in JS* — the mirror is a CSS concern;
    toggling one class keeps the camera/screen distinction declarative and
    co-located with the existing `.self` rule.
  - *Fix Participants + Bookmarks together* — they share the exact latent bug the
    chat panel had; fixing only the reported (Bookmarks) one would leave
    Participants broken. All three right panels now stack identically.
  - *No stream-level change* — confirms the issue's "what others see" concern was
    already correct; the fix is documentation + screen-share casing.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `.sharing` mirror override + camera-view comment | `index.astro` |
| S1 | Toggle `.sharing` on the self cell in start/stop share | `app.ts` |
| S2 | `z-index:1` on Participants + Bookmarks panels | `index.astro` |

## 6. Testing & Verification

- `astro check` clean; all 98 client unit tests pass; production build OK.
- Manual / E2E (`call.spec.ts` covers the screen-share path): start a screen
  share → self tile reads correctly (not mirrored); stop → camera mirrored again.
- Open Participants / Bookmarks with a hand raised → the hand no longer bleeds
  over the panel.

## 7. Deployment & Operations

- Client-only change → ships with the Vercel autodeploy on `main`. No server
  change, no env vars, no migration.

## 8. Risks / Open Items

- Low risk: CSS scoping + one DOM class. The `.sharing` class is added/removed
  only in start/stop share on a cell created once per session, so it can't drift.

## 9. References

- Issue: #17 (mirroring; audio part shipped in 0020)
- Prior art: `abe510be` (chat-panel z-index fix, 2026-06-11)
- Files: `client/src/pages/index.astro`, `client/src/scripts/app.ts`
