# 0090 — Fix the in-call invite panel on mobile

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Micio Dev |
| **Created** | 2026-06-17 |
| **Shipped** | 2026-06-17 |
| **Version** | — |
| **Commits** | `385576b` |
| **Depends on** | [0082](../0082-in-call-invite-branded-email/spec.md) (invite panel) |

## 1. Context & Problem

The invite bottom-sheet (spec 0082) was visibly broken on mobile (owner
screenshot): the **Copia / Invia buttons overflowed off the right edge**, squashing
the inputs to a sliver, and the **close button had no X** (an empty ghost pill).

Root cause of the overflow: `.invite-go` is a global `.btn-primary` (`width: 100%`),
so in a side-by-side `.invite-row` it took the whole row.

## 2. Goals / Non-Goals

**Goals**
- The action button sits **under** the input (owner's call), both full-width; the
  panel never scrolls horizontally.
- The close button shows the standard X.
- Correct text colours.

**Non-Goals**
- No change to the invite logic, the copy/email flow, or the bottom-sheet shell.

## 3. Design

- `index.astro`:
  - `.invite-row` → `flex-direction: column` (input on top, full-width button
    beneath). No more horizontal overflow.
  - Replace the **undefined** `var(--text-dim)` token with `var(--muted)` (4×) so
    the sub / labels / help / hint render the intended dim grey.
- `app.ts`: `$('invite-close').innerHTML = icon('close', 16)` (it was the only
  close button missing its icon).

## 4. Testing & Verification

- `astro check` + vitest + build green.
- Manual (owner): open the invite panel on mobile → inputs full-width, Copia /
  Invia full-width beneath, no x-scroll, a real X to close.

## 5. References

- Files: `client/src/pages/index.astro`, `client/src/scripts/app.ts`.
- Capacity rejection (full room → kicked with reason) already holds, incl. arriving
  via an invite link — the cap is enforced server-side on join (spec 0082).
