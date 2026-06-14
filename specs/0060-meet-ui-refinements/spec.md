# 0060 — Meet-style refinements: floating reactions, header clock, clearer count

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | VoxTranslate |
| **Created** | 2026-06-14 |
| **Shipped** | 2026-06-14 |
| **Version** | — |
| **Commits** | `54d7dd6` |
| **Depends on** | [0055](../0055-meet-like-session-ui/spec.md), [0023](../0023-call-toolbar-overflow-menu/spec.md) |

## 1. Context & Problem

Spec 0055 added quick reactions, a session-duration chip, and a participant count. Issue #94
is the follow-up polish to make them feel like Google Meet:

1. **Reactions are crammed into the bottom control bar** — cluttered, hard to spot, not
   separated from the primary controls.
2. **No current wall-clock time** in the header (Meet shows the time top-left). 0055 added
   *elapsed session duration*, which is useful but a different thing.
3. **The participant count reads as a faint, generic pill** — easy to miss, "not well designed".

(The "reactions close after one click / can't spam" complaint is already moot — 0055's buttons
are inline `sendEmoji` calls with no panel to close — but moving them to a dedicated tray makes
that obvious and the spamming fluid.)

## 2. Goals / Non-Goals

**Goals**
- A **floating reaction tray above the control bar** — centered, visually separated, always
  accessible, spammable (the existing 5/s limiter still applies).
- A **live wall-clock** chip in the header (top-left), Meet-style.
- A **clearer, more prominent participant-count** chip (top-right).

**Non-Goals**
- New reaction transport / emoji set / reaction sounds (reuse 0055's `sendEmoji` + limiter).
- Floating animated on-screen reactions or reactions-in-PiP (listed as future in #94).
- Removing the session-duration chip — it stays alongside the new clock.

## 3. Requirements

- **R1 — Floating reaction tray.** *Given* I'm in a call, *then* the reaction emoji sit in a
  centered pill **above** the control bar (not inside it), and tapping one fires the reaction
  for everyone without opening/closing anything; rapid taps spam (bounded by the 5/s limiter).
- **R2 — Live clock.** *Then* the header shows the current wall-clock time (locale `HH:MM`),
  top-left, updating every minute; it appears on join and clears on leave.
- **R3 — Clearer count.** *Then* the participant-count chip (top-right) is visually prominent
  (accent-tinted, full-contrast, bold) and still updates live on join/leave.

## 4. Design & Architecture

- **Components / files:**
  - `client/src/pages/index.astro`:
    - Move `#quick-reactions` **out** of `.control-bar` to a sibling **above** it; keep the
      `#quick-reactions` id (so `app.ts` wiring is untouched) and swap its class to
      `.reaction-bar` — a centered, blurred pill (`align-self:center`, the `.call-main`
      column `gap` provides the separation).
    - Add `#header-clock` (a plain text `<span>`) to `.call-header` after the room/visibility
      badges. Plain span ⇒ its text *is* its accessible name; no `role=status` so it doesn't
      announce every tick.
    - `.clock-badge` style (prominent plain text); `.part-count-badge` restyled to an
      accent-tinted, bold, full-contrast pill; `.react-btn` reworked for the tray (transparent
      on the pill, hover highlight, tap-pop).
  - `client/src/scripts/app.ts`: the existing 1 s session-timer tick now also renders the clock
    (`renderHeaderTimes`); `start/stopSessionTimer` show/hide `#header-clock` with the other chips.
- **Key decisions:**
  - **Keep the `#quick-reactions` id, just relocate + restyle** → zero JS churn; the button-
    injection loop and the 0055 e2e keep working.
  - **One interval for clock + duration** → re-rendering `HH:MM` each second is trivial and
    avoids a second timer; both clear together on leave.
  - **Tray in the column flow, not absolutely positioned** → no z-index/overlap fight with the
    isolated stage (spec 0054); the `.call-main` gap gives the "floating, separated" look safely.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Relocate reactions to `.reaction-bar` above the controls + restyle | `index.astro` |
| S1 | Live `#header-clock` chip + render in the session-timer tick | `index.astro`, `app.ts` |
| S2 | Restyle the participant-count chip | `index.astro` |

## 6. Testing & Verification

- **Automated:** `astro check` (0 errors), build, 146 unit tests green.
- **e2e (`meet-ui.spec.ts`, extended):** asserts the header clock shows `HH:MM` and the reactions
  now live in `#quick-reactions.reaction-bar` (its own tray) — plus the existing duration-ticks,
  count `1→2→1`, and four firing reaction buttons. The a11y suite stays green (the new clock span,
  tray, and restyled count add no WCAG violations). Run green locally against the `:3001` backend.

## 7. Deployment & Operations

- Client-only. No env vars, migrations, or server changes. Vercel auto-deploys on `main`.

## 8. Risks / Open Items

- The reaction tray adds a thin row above the controls (slightly less stage height); acceptable
  and matches Meet. Floating animated reactions remain a future enhancement (#94 "Future").
- Clock + duration are two small chips top-left; if it ever feels busy, they can merge into one.

## 9. References

- Issue: #94 (refines spec 0055; control bar from 0023).
- Files: `client/src/pages/index.astro`, `client/src/scripts/app.ts`, `client/e2e/meet-ui.spec.ts`
