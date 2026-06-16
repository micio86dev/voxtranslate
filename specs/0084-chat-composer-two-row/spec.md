# 0084 — Two-row chat composer (room to write)

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Micio Dev |
| **Created** | 2026-06-16 |
| **Shipped** | 2026-06-16 |
| **Version** | — |
| **Commits** | `042382f` |
| **Depends on** | [0070](../0070-call-chat-game-ux-fixes/spec.md) (chat composer + counter) |

## 1. Context & Problem

In the in-call chat, the composer packed the emoji button, attach button, the
textarea, and send into a single flex row. In the desktop side panel (320px
wide) that left the textarea only ~130–180px to type in — cramped, with short
wrapping lines. The owner reported "too little space to write" on desktop.

## 2. Goals / Non-Goals

**Goals**
- Give the textarea the full panel width so there's room to write, on desktop
  **and** mobile.
- No regression to the mobile bottom-sheet, the emoji popover, attach, the
  character counter, or Enter-to-send.

**Non-Goals**
- No widening of the chat panel (that would steal space from the video stage on a
  full mesh call).
- No change to send/auto-grow behaviour or limits.

## 3. Requirements

- **R1 — Full-width input.** The textarea spans the composer's full width.
- **R2 — Action bar.** Emoji + attach sit at the left of a slim row beneath the
  textarea; send is pinned to the right.
- **R3 — Parity.** Identical structure on the desktop side panel and the mobile
  bottom sheet; the emoji popover still opens upward; 44px tap targets preserved.

## 4. Design & Architecture

- `client/src/pages/index.astro`:
  - `.chat-input-row` becomes a column: `<textarea>` then `.chat-input-actions`.
  - `.chat-input-actions` is a flex row (emoji-wrap, attach, hidden file input,
    send); `.send-btn { margin-left: auto }` pins send right.
  - `#chat-input` drops `flex: 1`, keeps `width: 100%` and the existing
    `min-height: 44px` / `max-height: 120px` auto-grow (unchanged JS in
    `chat-input.ts`).
- No JS change: all wiring is by element id, not DOM structure.

## 5. Testing & Verification

- `vitest` (chat-input helpers unchanged) + `astro check` + build green.
- Manual: open chat on desktop (wide textarea) and mobile (bottom sheet); confirm
  emoji popover, attach, counter, and Enter-to-send still work.

## 6. Deployment & Operations

- Ships with the Vercel client deploy. No server change.

## 7. Risks / Open Items

- Purely presentational; the emoji popover and counter were re-checked against the
  new layout.

## 8. References

- Files: `client/src/pages/index.astro`.
