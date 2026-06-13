# 0039 — Bookmarks always require a label

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-13 |
| **Shipped** | 2026-06-13 |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0013](../0013-call-bookmarks/spec.md) |

## 1. Context & Problem

Spec 0013 pinned a bookmark **instantly** on the 🔖 press (server-stamped "now"),
then offered an *optional* label for ~3 s. So a call could accumulate unlabelled
"No label" pins — noise that's useless to review later. The owner's rule: **never
save a bookmark without a label during a call.**

## 2. Goals / Non-Goals

**Goals**
- A saved bookmark **always** carries a non-empty label.
- Preserve the *moment* even though the save is deferred until the label is typed.
- Apply the rule to both creation (🔖) and the side-panel inline edit.

**Non-Goals**
- Changing the side panel, exports, or the bookmark REST API shape (the `ts`/`label`
  fields already exist).
- Server-side enforcement (the server still accepts a label-less pin via the API;
  this is a call-UI rule). A future server validation could harden it.

## 3. Requirements

- **R1 — Label-first create.** Pressing 🔖 captures the moment and opens a
  **required-label** prompt; the pin is POSTed only when a non-empty label is
  confirmed (Save / Enter). An empty submit flags the prompt (red) and keeps it open.
- **R2 — Abandon = nothing saved.** Escape, "All bookmarks", or walking away from an
  **empty** prompt saves nothing (the empty prompt auto-closes after ~6 s; a
  half-typed label is *not* auto-closed).
- **R3 — Moment preserved.** The pin's timestamp is the client clock captured at the
  **press**, sent as `ts` — so a labelling delay doesn't drift the pin.
- **R4 — Edit can't empty a label.** The side-panel inline edit no longer clears a
  label to empty; an emptied edit reverts to the current label.

## 4. Design & Architecture

- `client/src/scripts/bookmarks.ts`
  - 🔖 click → `pendingTs = new Date().toISOString()` + `openPrompt()` (no POST).
  - `commitPin()` — guards a non-empty label, then `addBookmark(session, {label, ts})`;
    shows a transient success/failure result that auto-dismisses.
  - `armDismiss()` only schedules a close for an **empty** prompt; `hidePop()` clears
    `pendingTs` (a closed prompt discards the moment).
  - `startEdit().finish()` requires non-empty text to persist (R4).
- `client/src/scripts/api.ts` — `addBookmark(sessionId, { label?, ts? })` now sends
  `{ label, ts }` (omitted ⇒ server stamps "now", preserving other callers).
- `client/src/scripts/i18n.ts` — new `bookmarkLabelPrompt` ×8; `bookmarkLabelPh`
  drops "(optional)" ×8.
- `client/src/pages/index.astro` — popover comment + accessible label updated.
- **Key decisions:**
  - *Defer the POST, capture the client `ts`.* 0013 avoided client clock skew by
    server-stamping on an instant POST. Requiring a label means waiting, so we send
    the press-time `ts` — a few seconds of NTP skew is far smaller than the
    label-typing delay it replaces, and the moment stays accurate.
  - *Auto-close only when empty.* Never yank a half-typed label; only an untouched
    prompt times out (and an untouched prompt has nothing to lose).

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Label-first prompt + commit + client `ts` | `bookmarks.ts`, `api.ts` |
| S1 | Inline edit can't empty a label | `bookmarks.ts` |
| S2 | Strings ×8 + markup/aria | `i18n.ts`, `index.astro` |

## 6. Testing & Verification

- `astro check` clean; **101/101** unit tests; production build OK.
- Manual: 🔖 → prompt; empty Save → red nudge, stays open; type + Enter → pin saved
  with that label at the press moment; Escape/empty walk-away → nothing saved;
  inline-edit to empty → reverts.

## 7. Deployment & Operations

- **Client-only** — ships via the Vercel autodeploy on `main`. No server change.

## 8. Risks / Open Items

- The server API still accepts a label-less pin; only the UI enforces the rule. A
  server-side `400` on empty label would make it airtight (deferred).

## 9. References

- Files: `client/src/scripts/bookmarks.ts`, `client/src/scripts/api.ts`,
  `client/src/scripts/i18n.ts`, `client/src/pages/index.astro`.
