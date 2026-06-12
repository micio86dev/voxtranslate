# 0034 — Secondary CTA Restyle + Overflow-menu Z-fix

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-12 |
| **Shipped** | 2026-06-12 |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0023](../0023-call-toolbar-overflow-menu/spec.md), [0003](../0003-client-experience-pwa/spec.md) |

## 1. Context & Problem

Two UI issues:

1. **Dead-grey secondary buttons.** `.btn-primary` is a solid accent fill (good),
   but `.btn-ghost` (the secondary CTAs — *Show all*, *Maybe later*, *Back*,
   dismiss) was a transparent box with a grey border and **grey (`--muted`) text**,
   so it read as disabled/dead. The owner disliked it.
2. **⋯ menu hidden after pinning a tile.** Tapping **+** (pin) puts the call into
   *focus mode*, where the main cell is `position:absolute; inset:0; z-index:1` with
   `transform: translateZ(0)` — a GPU compositor layer. Opening the **⋯ More** menu
   (`z-index:30`) on mobile then showed nothing: the composited video **punched
   through** the menu (the same hardware-overlay issue as the raised-hand border in
   0021/0024).

## 2. Goals / Non-Goals

**Goals**
- A secondary button that reads as an intentional, tappable control (clear
  hierarchy below the primary) — not a grey outline.
- The ⋯ menu renders above a focused screen-share video.

**Non-Goals**
- Restyling the primary button or the segmented control (`.seg-btn` is a toggle,
  different semantics).
- A new design system / token changes.

## 3. Requirements

- **R1 — Secondary CTA.** `.btn-ghost` becomes a quiet **filled** button: elevated
  surface (`--surface-elevated`), real text colour (`--text`), 500 weight; hover
  lifts via `brightness(1.15)` (matching the primary), `:active` presses,
  `:focus-visible` shows an accent ring (accessibility).
- **R2 — Menu on top.** The ⋯ menu paints above the focused video in focus mode.

## 4. Design & Architecture

- `client/src/pages/index.astro`:
  - `.btn-ghost` — `background: var(--surface-elevated); color: var(--text); border:
    1px solid var(--border)` + hover/active/focus-visible states.
  - `.more-menu` — add `transform: translateZ(0)` so it gets its own compositor
    layer and composites above the video's hardware overlay (z-index alone can't
    beat a punched-through overlay).
- **Key decisions:**
  - *Filled-tonal secondary, not an outline* — on a dark surface a real (if quiet)
    fill with white text is inviting and clearly clickable; the grey-outline ghost
    reads as disabled. Hierarchy stays intact: solid accent = primary, elevated
    neutral = secondary.
  - *Promote the menu to a layer* — the proven fix for "video overlay covers a CSS
    element" (cf. 0021/0024); cheaper and more reliable than restructuring the
    focus-mode stacking.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `.btn-ghost` filled-tonal restyle + states | `index.astro` |
| S1 | `.more-menu` `translateZ(0)` compositor layer | `index.astro` |

## 6. Testing & Verification

- `astro check` clean; production build OK. (CSS-only — no JS/test change.)
- Manual: secondary buttons now read as filled/tappable with a clear focus ring;
  pin a tile → open ⋯ on mobile → the menu appears over the focused video.

## 7. Deployment & Operations

- Client-only CSS → ships with the Vercel autodeploy on `main`. No server change.

## 8. Risks / Open Items

- None of note. If `.btn-ghost` ever needs to sit *inside* an already-elevated card,
  the elevated-on-elevated contrast is mild — the border keeps it legible.

## 9. References

- Files: `client/src/pages/index.astro` (`.btn-ghost`, `.more-menu`).
- Related: 0021/0024 (the same compositor-layer fix), 0023 (the ⋯ menu).
