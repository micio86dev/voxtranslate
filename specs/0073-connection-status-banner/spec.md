# 0073 — Connection-status banner (offline / reconnecting / back online)

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-16 |
| **Shipped** | 2026-06-16 |
| **Version** | — |
| **Commits** | `1cb624d` |
| **Depends on** | [0002](../0002-video-calls-translated-chat/spec.md) (signaling WebSocket), [0070](../0070-call-chat-game-ux-fixes/spec.md) (secondary-CTA / button design language) |

## 1. Context & Problem

A real-time call app is only as good as the connection under it, but the client gave
no global feedback when the network dropped. If a user went offline, or the signaling
WebSocket silently died and started its 2 s reconnect loop (spec 0002), the UI just
froze — no audio/video updates, no explanation. Users couldn't tell "my Wi-Fi died"
from "the app is broken." Mainstream apps (YouTube, Gmail) solve this with a small
bottom snackbar that states the network state plainly and confirms recovery. We had
none; the only related signals were in-call-only and content-specific (the weak-uplink
nudge from spec 0030 and the unused `connectionLost` string).

## 2. Goals / Non-Goals

**Goals**
- A single, global, always-available indicator of network/transport health, on **every**
  screen (home, pre-join, in-call, session details).
- Three legible states with the conventional colour semantics: **offline (red)**,
  **transport struggling / reconnecting (amber)**, **recovered (green, transient)**.
- Never block the UI underneath; honour i18n, accessibility, and reduced-motion.

**Non-Goals**
- No retry/network logic change — it reflects the existing `navigator` events and the
  WS reconnect loop (0002); it does not add or alter reconnection behaviour.
- Not a replacement for the in-call weak-uplink nudge (spec 0030) — that stays a
  distinct, content-specific toast.
- No per-peer / WebRTC ICE state surfacing (mesh health is out of scope here).

## 3. Requirements

- **R1 — Offline (red).** As a user who loses connectivity, I want to know immediately.
  - *Given* any screen, *when* the browser fires `offline` (or `navigator.onLine` is
    false at boot), *then* a red pill reading "You're offline" slides up from the bottom
    and stays until connectivity returns.
- **R2 — Reconnecting (amber).** As a user in a call whose transport drops, I want to
  know the app is trying to recover.
  - *Given* an active call, *when* the signaling WebSocket closes unexpectedly
    (`code !== 1000`, not a manual leave) and enters its retry loop, *then* an amber pill
    reads "Connection problems — reconnecting…".
- **R3 — Back online (green, transient).** As a user, I want confirmation when things
  recover.
  - *Given* the banner was showing offline **or** reconnecting, *when* the browser comes
    back online or the WS reopens, *then* a green "Back online" pill flashes and
    auto-hides after ~2.6 s. *Given* everything was already fine, *then* no green flash
    fires on first paint.
- **R4 — Non-blocking & accessible.** *Given* the banner overlaps content, *then*
  `pointer-events: none` lets clicks pass through; the pill is `role="status"`
  (`aria-live="assertive"` for offline, else `polite`); the status dot pulse is stilled
  under `prefers-reduced-motion`; text clears WCAG AA (≥4.5:1).
- **R5 — Localised.** *Given* any of the 8 supported UI languages, *then* the three
  messages render in that language.

## 4. Design & Architecture

- **Components / files:**
  - `client/src/scripts/net-status.ts` — new module. A **pure, DOM-free decision core**
    `nextBannerState(prev, online, degraded) → { health, banner }` plus a thin imperative
    shell that lazily creates the pill on `<body>` and paints it.
  - `client/src/scripts/app.ts` — boots it (`initNetStatus()`) and feeds the transport
    signal (`setNetworkDegraded(true|false)`).
  - `client/src/layouts/Base.astro` — `.net-status` styles in the **global** stylesheet
    (so the JS-created element is styled without an Astro scope attr; see the global
    button-primitives decision in commit `d3530d5`), built on the `:root` design tokens.
  - `client/src/scripts/i18n.ts` — `netOffline` / `netReconnecting` / `netBackOnline`
    across all 8 languages.
- **State model:** `NetHealth = 'ok' | 'degraded' | 'offline'`. Inputs are
  `navigator.onLine` (browser) and an app-reported `degraded` flag (transport). Offline
  outranks degraded. The green `restored` banner is emitted **only** on a transition out
  of a non-`ok` health, never when `prev === 'ok'` — which is the one piece of logic worth
  unit-testing, hence the pure core.
- **Signal wiring (sequence):**
  1. Boot → `initNetStatus()` adds `online`/`offline` listeners + paints initial (hidden).
  2. `ws.onclose` (unexpected) → `setNetworkDegraded(true)` → amber.
  3. `ws.onopen` (reconnected) → `setNetworkDegraded(false)` → green flash → hide.
  4. `leaveCall()` → `setNetworkDegraded(false)` so an intentional leave never shows amber.
- **Key decisions:**
  - *Split a pure core from the DOM shell* → the tricky "flash only on recovery" rule is
    unit-tested in the `node` test env (no DOM), matching the repo's pure-helper testing
    pattern; the shell stays defensive (`typeof document/navigator`).
  - *Styles in `Base.astro` global, not a page-scoped `:global()` patch* → consistent with
    the global design-system primitives; keyframes resolve correctly (a scoped `@keyframes`
    referenced from a `:global()` rule would not).
  - *Semantic colours darkened via `color-mix(... #000)`* → white text on raw
    `--danger/--warning/--success` fails AA; the darkened fills + a vivid same-hue border
    keep the colour identity while passing contrast.
  - *Bottom-edge snackbar with `pointer-events: none`* → reads like YouTube/Gmail and
    never traps a click over the "Enter room" CTA or the in-call controls.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Pure state core + DOM shell (lazy pill, online/offline listeners, degraded API) | `client/src/scripts/net-status.ts` |
| S1 | Wire into app: boot init + WS open/close + leave-call clear | `client/src/scripts/app.ts` |
| S2 | Global styles (tokens, AA colours, pulse, reduced-motion) | `client/src/layouts/Base.astro` |
| S3 | i18n strings ×8 languages | `client/src/scripts/i18n.ts` |
| S4 | Unit tests for `nextBannerState` | `client/src/scripts/net-status.test.ts` |

## 6. Testing & Verification

- **Unit (`net-status.test.ts`, +7 cases):** pins R1–R3 on the pure core — offline wins
  over degraded, amber on online+degraded, green flash only on recovery from a bad state,
  no flash when already settled.
- **End-to-end (Playwright `context.setOffline()`):** a real `offline → online` transition
  drives the red pill then the green "Back online" pill; the amber state rendered and
  screenshotted. Confirms the browser-event wiring, not just the logic.
- **Gates:** `astro check` 0 errors; client build; `vitest` **195/195** passing.

## 7. Deployment & Operations

- **Client-only.** Auto-deploys on push to `main` via Vercel — no server change, no env
  vars, no migration. (Server/Railway untouched.)
- No feature flag; the banner is inert unless the browser reports offline or the WS drops.

## 8. Risks / Open Items

- Amber is only raised by the **signaling WS** drop. A degraded *media* path (WebRTC ICE)
  that keeps the WS alive won't turn it amber — that's the weak-uplink nudge's job (0030);
  a future iteration could fold ICE `disconnected` into `setNetworkDegraded`.
- `navigator.onLine` is best-effort (true can mean "has a NIC", not "has internet"); the
  WS-drop path covers the "online but actually broken" case during calls.
- Fixed bottom-centre position can visually overlap bottom content on very short
  viewports; `pointer-events: none` keeps it harmless, but placement may want per-screen
  tuning later.

## 9. References

- Commits: `1cb624d` (PR #160).
- Files: `client/src/scripts/net-status.ts`, `client/src/scripts/net-status.test.ts`,
  `client/src/scripts/app.ts`, `client/src/scripts/i18n.ts`, `client/src/layouts/Base.astro`.
- Related: [0002](../0002-video-calls-translated-chat/spec.md) (signaling WS + reconnect),
  [0030 weak-network nudge] (in-call uplink toast), global button/token refactor (`d3530d5`).
