# 0056 — Overflow ⋯ menu: fix ARIA aria-required-children violation

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | VoxTranslate |
| **Created** | 2026-06-14 |
| **Shipped** | 2026-06-14 |
| **Version** | — |
| **Commits** | `df5a85f` |
| **Depends on** | [0023](../0023-call-toolbar-overflow-menu/spec.md) |

## 1. Context & Problem

The in-call a11y audit (`e2e/a11y.spec.ts`, axe-core, WCAG 2.2 AA) fails with one **critical**
violation, surfaced while verifying spec 0055:

```
[critical] aria-required-children: Certain ARIA roles must contain particular children
  #more-menu
```

`#more-menu` carries `role="menu"` but its children are `.mm-group` wrapper `<div>`s and plain
`.control-btn` `<button>`s — none are `menuitem` / `menuitemradio` / `menuitemcheckbox`, which the
`menu` role **requires**. Worse, `role="menu"` advertises a composite widget with arrow-key roving
focus, but the overflow menu was never built that way — it's a disclosure popover (`btn-more` toggles
`aria-expanded`, opening focuses the first control, Escape / click-outside closes; there is no
arrow-key navigation). So the role is both **invalid** (axe) and **misleading** (it sets keyboard
expectations the widget doesn't meet). Pre-existing since spec 0023; unnoticed because the e2e a11y
job is skipped on PR CI.

## 2. Goals / Non-Goals

**Goals**
- Zero axe violations on the in-call screen (incl. the open ⋯ menu) under WCAG 2.2 AA.
- The menu's accessible semantics match its real behaviour (a labeled disclosure of controls).

**Non-Goals**
- Building a true ARIA menu with `menuitem` roles + arrow-key roving focus (heavier, and not what
  this control is — it's a grid of independent toggles/actions, each its own button).
- Any visual, layout, or interaction change.

## 3. Requirements

- **R1 — No a11y violation.** *Given* the in-call screen with the ⋯ menu open, *when* axe-core runs
  (WCAG 2.2 AA + best-practice), *then* there are **zero** violations.
  - *Given* `#more-menu`, *then* it no longer declares a role whose required children it lacks.
- **R2 — Behaviour unchanged.** *Given* the menu, *when* I open/close it (button, Escape, outside
  click) and operate its controls, *then* everything works exactly as before; each button keeps its
  own accessible name.

## 4. Design & Architecture

- **Components / files:** `client/src/pages/index.astro` — change `#more-menu` `role="menu"` →
  `role="group"`. It keeps its `data-i18n-label="moreTip"` (→ aria-label "More"), so it stays a
  **labeled group** of controls. `#btn-more` keeps `aria-haspopup="true"` + `aria-expanded` +
  `aria-controls` (the same disclosure pattern the emoji ⋯ toggle uses, which already passes axe).
- **Key decision:** `role="group"`, not full `menuitem` semantics → `group` has no required children
  (clears the violation), needs no JS, and honestly describes a disclosure of independent buttons.
  A real ARIA menu would also obligate arrow-key roving focus we don't implement — adding the role
  without the behaviour is what caused the misleading semantics in the first place.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `#more-menu` `role="menu"` → `role="group"` | `client/src/pages/index.astro` |

## 6. Testing & Verification

- **e2e (`a11y.spec.ts`):** the previously-failing `a11y: in-call screen has no WCAG violations`
  test now passes (all three audits: call, call+chat, call+participants) — run green locally against
  the `:3001` backend.
- **Automated:** `astro check`, `astro build`, and the unit suite stay green.
- **Manual:** open/close the ⋯ menu (click, Escape, outside click), toggle a couple of its controls —
  unchanged.

## 7. Deployment & Operations

- Client-only. No env vars, migrations, or server changes. Vercel auto-deploys on `main`.

## 8. Risks / Open Items

- None functional. If a true keyboard menu is ever wanted, that's a separate, larger change (roving
  `tabindex`, `menuitem*` roles, arrow-key handlers).

## 9. References

- Found while verifying spec 0055 (#72). Rule: axe `aria-required-children` (WCAG 2.2 AA).
- Files: `client/src/pages/index.astro`, `client/e2e/a11y.spec.ts`
