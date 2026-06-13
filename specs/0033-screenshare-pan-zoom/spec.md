# 0033 — Screen-share Signaling + Mobile Pan/Zoom

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-12 |
| **Shipped** | 2026-06-12 |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0002](../0002-video-calls-translated-chat/spec.md), [0021](../0021-display-fixes-mirror-stacking/spec.md) |

## 1. Context & Problem

A shared screen is landscape; a phone is portrait (9:16), so the screen tile is
heavily cropped by `object-fit: cover` — you can't read the edges. The app already
had a **mobile pan** on the focused tile, but with three problems: (a) it appeared
on **any** focused tile, not just screen shares; (b) its toggle icon (a tiny `⤢`
glyph) was too small; and (c) it only **panned** — you couldn't **zoom** to read
small text. Also, peers had **no idea a peer was screen-sharing** (the media is just
a swapped video track), so the tile couldn't be gated correctly. This adds the
missing screen-share signal and turns the gesture into pan **+ pinch-zoom**, scoped
to screen-share tiles on mobile.

## 2. Goals / Non-Goals

**Goals**
- Peers learn when someone starts/stops sharing (so the tile shows a 🖥 badge and
  enables pan/zoom).
- The pan/zoom toggle shows **only on mobile, only on a screen-share tile**, with a
  **larger, clear icon**.
- **One finger pans, two fingers pinch-zoom** the shared screen.

**Non-Goals**
- Pan/zoom on camera tiles or on desktop.
- Server-side awareness of media content (it stays a relayed flag; the media is P2P).

## 3. Requirements

- **R1 — Signal.** `start/stopScreenShare` send `{type: screen_share, active}`; the
  server relays it (`broadcast_except`) as `ServerMessage::ScreenShare {peer_id,
  active}` (mirrors hand-raise).
- **R2 — Peer tile.** On receipt, the peer's tile gets `.sharing` + a 🖥 badge
  (removed when it stops).
- **R3 — Gated toggle.** The pan/zoom button appears **iff** `IS_MOBILE` **and** the
  focused tile is `.sharing` (self or peer); otherwise pan is torn down.
- **R4 — Bigger icon.** A 48 px button with the `move` SVG icon (replacing the small
  `⤢` glyph).
- **R5 — Pan + pinch.** In pan-mode: 1 finger translates, 2 fingers scale (clamped
  1×–4×); both compose into one `transform`.

## 4. Design & Architecture

- **Server:** `protocol.rs` adds `ClientMessage::ScreenShare {active}` +
  `ServerMessage::ScreenShare {peer_id, active}`; `lib.rs` relays it like hand-raise.
- **Client:**
  - `app.ts` — send on `start/stopScreenShare`; a `screen_share` handler →
    `setScreenShareIndicator(peer, active)` (toggles `.sharing` + 🖥) then
    `layoutVideos()` to re-gate pan; `layoutVideos` calls `setupPan` only for a
    `.sharing` focus cell on mobile, else `disablePan`.
  - `setupPan` rewritten: `AbortController`-scoped touch listeners + a button, with
    a `transform: translate(tx,ty) scale(s)` model; 2-finger pinch tracks the touch
    distance ratio; teardown via `abort()` so re-sharing rebuilds cleanly.
  - `icons.ts` — a `move` (4-arrow) icon; `i18n.ts` — `panZoomHint` (8 languages);
    `index.astro` — the 48 px `.pan-toggle`.
- **Key decisions:**
  - *Reuse `.sharing`* (already set on the self tile in 0021) for peer tiles too, so
    one class gates both the mirror-drop and the pan/zoom.
  - *Relay a flag, not media* — consistent with the P2P architecture; the server
    never sees the screen.
  - *AbortController teardown* — clean re-setup across start→stop→start without
    leaking listeners or losing the button.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `ScreenShare` protocol + relay | `server/src/{protocol,lib}.rs` |
| S1 | Send + receive + peer badge/`.sharing` | `app.ts` |
| S2 | Gate pan to mobile screen-share tiles | `app.ts` |
| S3 | Pan + pinch-zoom gesture + bigger icon | `app.ts`, `icons.ts`, `index.astro` |
| S4 | `panZoomHint` i18n (8 langs) | `i18n.ts` |

## 6. Testing & Verification

- Server `cargo fmt`/`clippy --all-targets` clean; client `astro check` clean,
  **101/101** unit tests, build OK.
- Manual (mobile): a peer shares → 🖥 badge + tile becomes pan-eligible; focus it →
  the move button shows; tap → drag to pan, two-finger pinch to zoom; share stops →
  button + badge gone.

## 7. Deployment & Operations

- Client ships via Vercel; **server needs `railway up`** for the new relay arm
  (until then peers just won't get the screen-share flag — graceful, no error).

## 8. Risks / Open Items

- The pinch math is anchor-on-touchstart (no focal-point zoom-to-finger); good
  enough for reading a screen, but a future refinement could zoom toward the pinch
  midpoint.

## 9. References

- Files: `server/src/{protocol,lib}.rs`, `client/src/scripts/{app,icons,i18n}.ts`,
  `client/src/pages/index.astro`.
