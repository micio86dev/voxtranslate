# 0061 — Immersive call overlays: reaction chips, on-video clock/room/info + participant badge

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | VoxTranslate |
| **Created** | 2026-06-14 |
| **Shipped** | 2026-06-14 |
| **Version** | — |
| **Commits** | `f3eb22b` |
| **Depends on** | [0060](../0060-meet-ui-refinements/spec.md), [0055](../0055-meet-like-session-ui/spec.md) |

## 1. Context & Problem

Issue #98 is the follow-up polish to spec 0060. Two gaps remain versus Google Meet / Zoom:

1. **Reactions still read as flat characters.** 0060 moved them to a floating tray, but the
   buttons are transparent on the pill — they don't look like distinct, tappable chips.
2. **The header is a dashboard row, not an immersive call.** Room code, clock, duration,
   participant count and balance all sit in a `.call-header` *above* the video, so the call
   feels framed by a toolbar rather than overlaying the stage. Meet overlays a subtle, gradient-
   backed cluster *on the video*: time + room top-left, a participant badge top-right.

## 2. Goals / Non-Goals

**Goals**
- **Reaction chips** — each quick reaction is a filled, rounded button with a hover state and
  consistent sizing, visually consistent with the control bar.
- **On-video meta, top-left** — `HH:MM | ROOM ⓘ` overlaid on the stage over a subtle dark
  backdrop, replacing the separate header row.
- **On-video participant badge, top-right** — an avatar (your initial, gradient) + live count.
- **Less-intrusive balance** — visibility, session duration and balance move behind the ⓘ
  info popover instead of living permanently in the header.

**Non-Goals**
- New reaction transport / emoji set (reuse 0055's `sendEmoji` + the 5/s limiter).
- New balance / billing flows — the Buy-Credits modal still owns top-ups; ⓘ is read-only.
- Touching video/audio or the WebRTC path.

## 3. Requirements

- **R1 — Reaction chips.** *Given* I'm in a call, *then* each quick reaction in
  `#quick-reactions` is a filled circular chip (control-bar look), highlights on hover, pops on
  tap; the tray itself is a transparent, centered group. Tapping still fires `sendEmoji`
  (bounded by the 5/s limiter); rapid taps spam.
- **R2 — On-video clock + room + info.** *Then* the top-left of the video stage shows the live
  wall-clock (`HH:MM`), a separator, the room-code (copies on click, brief "Copied" feedback)
  and an ⓘ info button — over a subtle dark backdrop, not in a header row. The clock appears on
  join and clears on leave.
- **R3 — On-video participant badge.** *Then* the top-right of the video stage shows a badge
  with my avatar initial (gradient) and the live participant count (self + peers), updating on
  join/leave.
- **R4 — Info popover holds balance.** *Then* the ⓘ button toggles a popover listing room
  **visibility**, **session duration** (live), and **balance**; it closes on outside-click or
  Escape. No permanent balance pill sits over the video.

## 4. Design & Architecture

- **Components / files:**
  - `client/src/pages/index.astro`:
    - **Remove `<header class="call-header">`.** Move its children onto the stage as absolutely-
      positioned overlays inside `.video-stage`:
      - `.stage-meta` (top-left): `#header-clock` (a `::after` renders the `|` only while the
        clock is shown), `#call-room`, `#call-info` (ⓘ), plus the existing `#transcript-indicator`
        and `#glossary-badge` (hidden until active). A `#call-info-pop` popover (child of
        `.stage-meta`) holds `#call-vis`, `#session-timer`/`#session-elapsed`, `#call-balance`.
      - `#part-count` (top-right): restyled to an avatar (`#part-avatar`, gradient initial) +
        `#part-count-n`.
    - Both clusters get a dark semi-opaque pill (`rgba(0,0,0,.5)` + blur) so text keeps WCAG-AA
      contrast over video (mirrors `.video-overlay`).
    - `.react-btn` reworked to a filled circular chip (`--surface-elevated`, hover `--border`,
      tap-pop); `.reaction-bar` made transparent so the chips are the visual.
    - `.rec-badge` moved to top-center and `.timer-badge` stacked below the participant badge so
      neither collides with the new persistent overlays.
  - `client/src/scripts/app.ts`:
    - `start/stopSessionTimer` also show/hide `#call-info` (clock/duration/count already toggle).
    - `updateParticipantsList` paints `#part-avatar` (initial + `avatarGradient`).
    - New `toggleInfoPop` (mirrors the timer popover): outside-click + Escape close.
    - The room-copy handler swaps the **room badge's own** text to "Copied" for 1.2 s (decoupled
      from `#call-vis`, which now lives in the popover).
  - `client/src/scripts/icons.ts`: add an `info` icon.
  - `client/src/scripts/i18n.ts`: add `callInfoTip` + `visibility` to all 8 locales.
- **Key decisions:**
  - **Keep every existing id** (`#header-clock`, `#call-room`, `#call-vis`, `#session-timer`,
    `#part-count[-n]`, `#call-balance`, `#quick-reactions`/`.react-btn`) → zero JS-wiring churn;
    only markup placement + styling change. Billing/`call.spec` selectors keep working (they
    read text/class, not visibility).
  - **Overlays live inside `.video-stage`** → the stage's `isolation: isolate` (spec 0054) keeps
    them below the control-bar `.more-menu`, so no z-index fight.
  - **Balance behind ⓘ** → satisfies #98's "less-intrusive place" without losing it; the
    Buy-Credits modal remains the place to top up.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Reaction chips (`.react-btn` filled, `.reaction-bar` transparent) | `index.astro` |
| S1 | `.stage-meta` overlay (clock \| room ⓘ) + `#call-info-pop`; remove `.call-header` | `index.astro`, `app.ts`, `icons.ts`, `i18n.ts` |
| S2 | Participant avatar badge top-right | `index.astro`, `app.ts` |
| S3 | Reposition `.rec-badge` / `.timer-badge` to clear the new overlays | `index.astro` |

## 6. Testing & Verification

- **Automated:** `astro check` (0 errors), build, unit tests green.
- **e2e (`meet-ui.spec.ts`, updated):** clock overlay shows `HH:MM`; the reaction tray is
  `#quick-reactions.reaction-bar` with 4 firing `.react-btn` chips; the participant badge shows
  the live count `1→2→1`; opening ⓘ reveals the duration (ticking) + balance. The a11y suite
  (`a11y.spec.ts`) stays green (overlays use a dark backdrop → AA contrast; ⓘ has an
  accessible name; the closed popover is `display:none` and skipped).

## 7. Deployment & Operations

- Client-only. No env vars, migrations, or server changes. Vercel auto-deploys on `main`.

## 8. Risks / Open Items

- Persistent on-video overlays cover a sliver of the top corners; acceptable and matches Meet.
- Balance is now one tap (ⓘ) away rather than always-on; the low-balance banner still warns
  proactively, so glanceability is preserved where it matters.

## 9. References

- Issue: #98 (refines spec 0060 / #94; control bar from 0023; stage isolation from 0054).
- Files: `client/src/pages/index.astro`, `client/src/scripts/app.ts`,
  `client/src/scripts/icons.ts`, `client/src/scripts/i18n.ts`, `client/e2e/meet-ui.spec.ts`
