# 0023 — Declutter the In-call Toolbar (Overflow "More" Menu)

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-12 |
| **Shipped** | 2026-06-12 |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0002](../0002-video-calls-translated-chat/spec.md), [0003](../0003-client-experience-pwa/spec.md) |

## 1. Context & Problem

The in-call control bar had grown to **15 round icon buttons on desktop / 11 on
mobile**, all the same size and weight, in one flat row (mic, camera,
background-blur, speak-translations, subtitles, raise-hand, bookmark, view,
screen-share, record, picture-in-picture, fullscreen, participants, chat, leave).
Fifteen identical targets = no hierarchy: the eye re-scans the whole row every
time, and on mobile the bar became a horizontally-scrolling strip. High cognitive
load for what should be a glanceable HUD.

This was developed with the **frontend-design** skill: the fix is information
architecture, not decoration — show only what's used continuously or needed in an
emergency, and collapse the rest behind a labelled overflow menu (icon **+ text**,
which is also more discoverable than the icon-only bar).

## 2. Goals / Non-Goals

**Goals**
- A small **primary** cluster that's always visible (desktop *and* mobile), no
  horizontal scroll.
- Everything secondary lives in a **⋯ More** popover, grouped and **labelled**.
- No loss of state awareness for the collapsed actions.

**Non-Goals**
- Removing or reworking any feature — every existing button still exists and keeps
  its id, handler, icon, and active-state classes (pure relocation + a toggle).
- A visual reskin of the buttons themselves (same `.control-btn` language).
- Per-user customization of which actions are primary (future).

## 3. Requirements

- **R1 — Primary cluster.** *Then* the bar shows exactly **Mic, Camera,
  Subtitles, Chat, Leave** plus a **⋯ More** button — chosen so the two universal
  call controls (mic/cam), the app's signature surface (subtitles — this is a
  *translation* app), comms (chat, with its unread badge), and the always-reachable
  Leave stay one tap away.
- **R2 — Overflow menu.** *When* I press ⋯, *then* a popover opens above the bar
  with the remaining controls as **icon + text** rows, grouped under **Translation**
  (speak-translations), **Stage** (layout, screen-share, background-blur, PiP,
  fullscreen, record) and **Room** (raise-hand, participants, bookmark). *When* I
  click a control, or click outside, or press Escape, *then* the menu closes.
- **R3 — State stays visible.** *Given* a collapsed action is active (TTS on,
  screen-sharing, recording, hand raised), *then* the ⋯ button shows an accent dot;
  and recording/share/hand also remain indicated on the video stage itself (REC
  badge, 🖥 badge, hand indicator) — so nothing important is hidden by collapsing.
- **R4 — Mobile.** *Then* the primary cluster fits one row without scrolling, and
  the menu pops above the bar.
- **R5 — Localized.** New strings (`moreTip`, `moreGroup{Translation,Stage,Room}`)
  in all 8 languages; menu item labels reuse the existing `*Tip` keys (no new
  strings for those).

## 4. Design & Architecture

- **Components / files:**
  - `client/src/pages/index.astro` — control-bar markup re-grouped into primary
    `.ctl-group`s + a `.more-group` holding `#btn-more` and the `#more-menu`
    popover; CSS for `.more-menu` / `.mm-group` / `.mm-head` / `.mm-grid` / `.mi`
    / `.mi-label`, the `.control-btn.has-active` dot, and a `.mi:has(> .control-btn.hidden)`
    rule so a hidden button (mobile share/fullscreen, guest bookmark) also hides
    its menu row. Mobile bar switched from `overflow-x:auto` scroll to
    `flex-wrap:wrap` (it fits now, and the popover is no longer clipped).
  - `client/src/scripts/app.ts` — `setControlState()` paints the ⋯ icon and the
    `has-active` dot; a small open/close controller (`setMoreOpen`) toggles the
    menu on click, closes on outside-click / Escape / acting on any item.
  - `client/src/scripts/icons.ts` — a `more` (three-dot) glyph.
  - `client/src/scripts/i18n.ts` — the 4 new keys × 8 languages.
  - `client/e2e/*` — `call`, `a11y`, `screenshare` specs open the ⋯ menu before
    clicking the now-collapsed `btn-tts` / `btn-participants` / `btn-share`.
- **Key decisions:**
  - *Relocate, don't rebuild* — each button keeps its `id`, so every handler,
    `innerHTML = icon(...)` paint, badge, and `active-*` class works untouched.
    Labels are **sibling** spans (outside the button) so the icon paint can't wipe
    them. Lowest-risk path to a big UX change.
  - *Subtitles is primary, participants is not* — for a translation product the
    subtitle toggle is the signature control; participant count is surfaced on its
    menu row instead.
  - *Safe to collapse record/share/hand* — their live state is already on the
    stage (REC badge, 🖥 badge, hand indicator), so the bar needn't echo it; the ⋯
    dot covers the rest.
  - *Close-on-pick* — standard overflow-menu behaviour; toggles that you'd flip
    repeatedly (subtitles) stayed in the primary row.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `more` icon | `icons.ts` |
| S1 | Re-grouped bar + ⋯ menu markup + CSS | `index.astro` |
| S2 | ⋯ paint + has-active dot + open/close controller | `app.ts` |
| S3 | i18n (4 keys × 8) | `i18n.ts` |
| S4 | E2E open the menu before collapsed-button clicks | `e2e/*.spec.ts` |

## 6. Testing & Verification

- `astro check` clean; 98/98 unit tests pass; production build OK.
- E2E updated so `call` / `a11y` / `screenshare` open ⋯ before the moved buttons.
- Visual: confirm on the PR's Vercel preview (primary row + ⋯ menu, desktop &
  mobile) — see §8.

## 7. Deployment & Operations

- Client-only change → ships with the Vercel autodeploy on `main`. No server
  change, no env vars, no migration.

## 8. Risks / Open Items

- **No automated visual check.** The call screen needs a live room, so this PR was
  verified by typecheck/build/unit + updated E2E; eyeball the **Vercel preview**
  before merging (button order, popover position on short viewports, the ⋯ dot).
- **`:has()` support.** The `.mi:has(...)` hide rule needs a modern browser
  (all current evergreen targets support it); a hidden button would otherwise show
  an orphan label — cosmetic only.

## 9. References

- Built with the `frontend-design` skill (installed via `npx skills add anthropics/skills`).
- Files: `client/src/pages/index.astro`, `client/src/scripts/app.ts`,
  `client/src/scripts/icons.ts`, `client/src/scripts/i18n.ts`, `client/e2e/*`.
