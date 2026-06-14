# 0054 — Whiteboard layering, mobile visibility & double-tap zoom

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | VoxTranslate |
| **Created** | 2026-06-14 |
| **Shipped** | 2026-06-14 |
| **Version** | — |
| **Commits** | `acff8c7` |
| **Depends on** | [0045](../0045-collaborative-whiteboard/spec.md), [0023](../0023-call-toolbar-overflow-menu/spec.md), [0034](../0034-focus-mode-pin/spec.md) |

## 1. Context & Problem

Three reported UI defects, all in the in-call surface (issue #71):

1. **⋯ menu opens behind the whiteboard (desktop).** With the collaborative
   whiteboard (spec 0045) open, opening the overflow menu (spec 0023) renders the menu
   *behind* the board, so its items are unreachable. By raw z-index the menu (`z-index: 30`)
   already out-ranks the whiteboard (`z-index: 7`) — both resolve in `#call`'s stacking
   context, so CSS painting order *should* put the menu on top. It doesn't, because the
   whiteboard's `<canvas>` is promoted to its own GPU compositor layer and **punches
   through** the overlapping menu. This is the same hardware-overlay class the code already
   fights for the focused screen-share video (the `transform: translateZ(0)` on `.more-menu`,
   the `.part-hand` notes), but the menu's own layer is not enough to beat the canvas layer.

2. **Whiteboard intermittently invisible on mobile.** Opening the board sometimes shows a
   blank stage. `Whiteboard.resize()` sizes the backing store from `canvas.clientWidth/Height`
   and **early-returns when either is 0** (`if (!w || !h) return;`). `toggleWhiteboard()`
   removes `.hidden` and calls `resize()` **synchronously in the same tick**; on mobile the
   stage layout isn't always settled at that instant (dynamic URL-bar viewport, the menu the
   board was launched from still collapsing), so `clientWidth/Height` reads 0, `resize()`
   bails, and the canvas is left at its default 0×0 — blank — with nothing to re-trigger it
   until the next window `resize`. Hence "intermittent / sometimes works".

3. **Unintended page zoom on repeated icon taps.** Rapidly tapping a control icon on mobile
   triggers the browser's native **double-tap-to-zoom**, zooming the whole UI. The controls
   have no `touch-action`, so the default gesture applies.

## 2. Goals / Non-Goals

**Goals**
- The ⋯ menu always paints **above** the whiteboard (and every other stage overlay), on
  every browser/GPU, with no dependence on the canvas compositing lottery.
- Opening the whiteboard **reliably** sizes and shows the canvas, on mobile included.
- Rapid taps on call controls **never** zoom the page; pinch-zoom (accessibility) still works.

**Non-Goals**
- Restyling the whiteboard, the overflow menu, or the control bar.
- Disabling page zoom globally (no `user-scalable=no` / `maximum-scale` — keeps pinch-zoom).
- A whiteboard rewrite or a `ResizeObserver`-based auto-fit (the single-frame defer is enough).

## 3. Requirements

- **R1 — Menu above the whiteboard.** As a user with the whiteboard open, *when* I open the
  ⋯ menu, *then* the menu and all its items render fully **above** the board and are clickable.
  - *Given* the whiteboard is active, *when* `#more-menu` is shown, *then* no part of the
    board's canvas overlaps/hides the menu.
- **R2 — Menu above every stage overlay.** *Given* a mini-game / quiz / timer popover is open,
  *when* I open the ⋯ menu, *then* the menu still paints on top (no regression vs. today).
- **R3 — Whiteboard visible on open (mobile).** As a mobile user, *when* I open the whiteboard,
  *then* the board surface and any existing strokes render immediately — never a blank stage.
  - *Given* the overlay was hidden, *when* it is shown, *then* the canvas backing store is sized
    to the stage on the next frame and the op-log is redrawn.
- **R4 — No double-tap zoom on controls.** As a mobile user, *when* I tap a control icon several
  times quickly, *then* the page does **not** zoom; the action just fires repeatedly.
  - *Given* any `.control-btn` / toolbar button, *when* double-tapped, *then* `touch-action:
    manipulation` suppresses the zoom gesture while leaving pan + pinch-zoom intact.

## 4. Design & Architecture

- **Components / files:**
  - `client/src/pages/index.astro` — CSS only:
    - `.video-stage { isolation: isolate; }` — confines the whiteboard (and all stage overlays:
      grid, badges, popovers, mini-games) into **one** stacking context. That context sits at
      `z-index: auto` inside `#call`, **below** the sibling `.more-menu` (`z-index: 30`), so the
      composited canvas can no longer order itself above the menu. Surgical: the popover/mini-game
      ordering *within* the stage is untouched, and the control bar is not raised over the
      stage's own popovers (R2). The menu keeps its `translateZ(0)` as belt-and-suspenders.
    - `touch-action: manipulation` on `.control-btn` and the whiteboard tool/colour buttons.
  - `client/src/scripts/app.ts` — `toggleWhiteboard()` defers `whiteboard.resize()` to
    `requestAnimationFrame` so layout is settled (non-zero `clientWidth/Height`) before sizing.
- **Data model / Protocol:** unchanged.
- **Key decisions:**
  - **`isolation: isolate` on the stage, not raising the control bar** → fixes the punch-through
    by confining the canvas layer, without lifting the control bar over the timer/bookmark
    popovers (which a blanket `z-index` bump on `.control-bar` would do). `isolation` is the
    purpose-built way to open a stacking context with no transform/paint side effects.
  - **`requestAnimationFrame`, not a synchronous resize** → one frame guarantees the un-hidden
    stage has laid out; cheaper and simpler than a `ResizeObserver`, and self-corrects the mobile
    race instead of waiting for a window `resize`.
  - **`touch-action: manipulation`, not `user-scalable=no`** → kills double-tap zoom + the 300 ms
    click delay on the controls only, while preserving pinch-zoom everywhere (a11y / WCAG 1.4.4).

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Isolate the stage so the ⋯ menu paints above the whiteboard canvas | `client/src/pages/index.astro` |
| S1 | `touch-action: manipulation` on controls (kill double-tap zoom) | `client/src/pages/index.astro` |
| S2 | Defer `whiteboard.resize()` to rAF on open (mobile blank fix) | `client/src/scripts/app.ts` |

## 6. Testing & Verification

- **Automated (this change):** `astro check` (0 errors), `astro build`, and the 146-test unit suite
  all green; the three fixes are confirmed present in the production bundle (`isolation:isolate` on
  `.video-stage`, `touch-action:manipulation` ×3, `requestAnimationFrame(() => …resize())`).
- **Manual (desktop):** open whiteboard → open ⋯ menu → menu fully visible & clickable above the
  board (R1); repeat with a mini-game / quiz / timer popover open (R2).
- **Manual (mobile / responsive devtools — required gate):** open whiteboard repeatedly → board +
  strokes always render (R3); rapidly tap mic/cam/⋯ icons → no page zoom, pinch-zoom still works (R4).
  The R1 paint fix and the R3 layout race are GPU/viewport-specific and only reproduce on a real
  device, so an on-device pass is the close gate (same caveat as spec 0034's overlay).
- **No unit test:** the change is CSS + a one-frame defer with no new pure logic — covered by the
  manual/on-device pass per the spec 0045 / 0053 convention for canvas/DOM behaviour. A desktop
  `call.spec.ts` e2e was considered but rejected: it can't reproduce the mobile-only race, so it
  would guard nothing the manual pass doesn't (tracked as a follow-up if e2e gains a mobile project).

## 7. Deployment & Operations

- Client-only. No env vars, migrations, or server changes. Vercel auto-deploys on `main`.

## 8. Risks / Open Items

- The punch-through is GPU/driver-specific; `isolation: isolate` is the spec-correct fix but
  wants a real on-device pass (Chrome/Safari, desktop + mobile) before close — same caveat the
  focused-video overlay (spec 0034) carries.
- If a future overlay must sit *above* the ⋯ menu while on the stage, it can no longer rely on a
  raw `z-index` > 30 (it's now trapped under the stage's isolation) and must live outside `.video-stage`.

## 9. References

- Issue: #71 (refs specs 0045 whiteboard, 0023 overflow menu, 0034 focus-mode overlay)
- Files: `client/src/pages/index.astro`, `client/src/scripts/app.ts`, `client/src/scripts/whiteboard.ts`
