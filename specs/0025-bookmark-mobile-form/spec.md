# 0025 — Mobile-friendly Bookmark Quick-label Form

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-12 |
| **Shipped** | 2026-06-12 |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0013](../0013-call-bookmarks/spec.md), [0003](../0003-client-experience-pwa/spec.md) |

## 1. Context & Problem

The in-call bookmark quick-label popover (`#bookmark-pop`, spec 0013) is a single
**non-wrapping** horizontal flex row: a status title (`white-space: nowrap`), a
fixed **200px** text input, a primary **Save** button, and a ghost **Show all**
button, centered at the bottom of the video stage with `max-width: 94%`. On a
phone (~360px wide) that row is far wider than 94% of the screen, so the input and
buttons **run off-screen and get clipped** — you can't see or use the whole form.
Desktop is unaffected.

## 2. Goals / Non-Goals

**Goals**
- On mobile the whole form is visible and usable — title, input, both actions.
- Keep it **quick to use mid-call** (comfortable touch targets, thumb-reachable).
- Desktop layout unchanged.

**Non-Goals**
- Any DOM/JS change — this is a pure responsive-CSS fix (the markup already has the
  right elements in the right order).
- Restyling the desktop popover or the bookmarks side panel.

## 3. Requirements

- **R1 — No clipping on mobile.** *Given* a phone viewport (≤600px), *when* the
  bookmark popover appears, *then* the title, input, and both buttons are fully
  visible (nothing runs off-screen).
- **R2 — Quick to use.** *Then* the input spans the full width and the buttons are
  ≥44px tall (comfortable touch targets); the popover sits at the bottom of the
  stage, above the control bar.
- **R3 — Desktop unchanged.** *Given* a desktop viewport, *then* the popover keeps
  its compact single-row layout.

## 4. Design & Architecture

- **Components / files:** `client/src/pages/index.astro` — a `@media (max-width:
  600px)` block for `#bookmark-pop` only. The container becomes a **full-width
  sheet** (`left:8px; right:8px; transform:none; max-width:none`) and the existing
  flex row gains `flex-wrap: wrap`, so:
  - title → `flex: 1 1 100%` (full row, `white-space: normal` so a long localized
    title wraps instead of forcing width),
  - input → `flex: 1 1 100%; width: auto` (full row, 44px tall),
  - Save + Show all → `flex: 1 1 auto` (split the last row 50/50), 44px tall.
- **Key decisions:**
  - *CSS-only, wrap-don't-restructure* — the markup order (title, input, save,
    show-all) already yields the ideal stack once `flex-wrap` is on and the first
    two items are forced to 100%, so no DOM/JS churn and the desktop path is
    untouched.
  - *Full-width sheet, not a centered pill* — on a phone the popover should own the
    width; an 8px gutter sheet is thumb-friendly and leaves no room to clip.
  - *44px targets* — bumps the 40px desktop controls to the mobile touch minimum.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `@media (max-width:600px)` wrap + full-width sheet for `#bookmark-pop` | `index.astro` |

## 6. Testing & Verification

- `astro check` clean; client unit tests pass; production build OK.
- Manual: on a ≤600px viewport, pin a bookmark → the popover shows title, a
  full-width input, and Save / Show all on one row, all on-screen and tappable.

## 7. Deployment & Operations

- Client-only CSS change → ships with the Vercel autodeploy on `main`. No server
  change, no env vars, no migration.

## 8. Risks / Open Items

- None of note (scoped CSS in a phone-only media query).

## 9. References

- Built with the `frontend-design` skill.
- Files: `client/src/pages/index.astro` (`#bookmark-pop` mobile media query).
- Related: spec 0013 (in-call bookmarks).
