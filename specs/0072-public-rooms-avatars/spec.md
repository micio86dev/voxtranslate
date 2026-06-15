# 0072 — Public-rooms lobby: overlapping participant avatars

| | |
|---|---|
| **Status** | Draft |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-15 |
| **Shipped** | — |
| **Version** | — |
| **Commits** | — |
| **Depends on** | [0022](../0022-guest-public-room-gate/spec.md), [0070](../0070-call-chat-game-ux-fixes/spec.md) (avatar render helper) |

## 1. Context & Problem

The home lobby lists public rooms with each participant as a name + language-flag chip
(spec 0022). It's functional but flat — there's no at-a-glance sense of *who* is in a
room. A social-style overlapping avatar stack ("AvatarGroup") makes the lobby feel
alive and lets users recognise people quickly. The name/flag chips are liked and stay;
this just adds the avatars.

## 2. Goals / Non-Goals

**Goals**
- Show each public room's participants as overlapping round avatars (image when the
  user has one, initial + gradient otherwise), alongside the existing name/flag chips.

**Non-Goals**
- No change to the chips, room ordering, or the guest sign-in gate (0022).
- No new avatar storage — reuse the `avatar_url` the server already holds per peer.
- No hover cards / click-through on avatars (v1 is display-only).

## 3. Requirements

- **R1 — Avatar stack.** *Given* a public room in the lobby, *then* its online
  participants render as overlapping round avatars (social style); *when* a participant
  has an `avatar_url`, *then* the image is shown; *otherwise* an initial + gradient.
- **R2 — Keep the detail.** *Given* the same room, *then* the existing name + flag chips
  remain below the avatar stack.
- **R3 — Privacy parity.** *Given* public rooms are already public (names/langs listed),
  *then* exposing the same users' avatars is consistent; guests (no `avatar_url`) show
  initials only — no new PII surface beyond what a room already publishes.

## 4. Design & Architecture

- **Server:** add `avatar: Option<String>` to `protocol::Member`; populate it from the
  peer's existing `Peer.avatar_url` in `RoomManager::public_rooms` (`server/src/rooms.rs`).
  `GET /rooms` now carries `participants[].avatar`.
- **Client:** `renderRooms` (`client/src/scripts/app.ts`) builds a `.room-item-avatars`
  stack, reusing the `fillAvatar` helper (spec 0070 R2.3 — image with initials/gradient
  fallback) for each participant, then the existing chips. CSS in `index.astro`: circles
  with a `--bg` ring and negative left margin for the overlap.
- **Key decision:** reuse `fillAvatar` + the already-broadcast `avatar_url` — no new
  endpoint, storage, or data exposure beyond the public room listing.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `Member.avatar` + populate in `public_rooms`; client avatar stack + CSS | `server/src/{protocol,rooms}.rs`, `client/src/scripts/app.ts`, `client/src/pages/index.astro` |

## 6. Testing & Verification

- Server: existing `rooms::tests::public_rooms_filters_and_counts` still passes;
  `cargo fmt` + `cargo check` clean.
- Client: `astro check` + build; headless render of the lobby confirms overlapping
  avatars (image + initials fallback) above the chips.

## 7. Deployment & Operations

- **Server deploy is MANUAL** (`railway up` from `server/`) — the `avatar` field won't
  appear in `GET /rooms` until the server is redeployed. Client auto-deploys on `main`
  (Vercel); it degrades gracefully (no `avatar` → initials) against the old server.
- No env vars, no migration.

## 8. Risks / Open Items

- Avatar URLs are third-party (Google) images loaded in the lobby — already the case in
  call tiles (0070); `referrerPolicy=no-referrer` + initials fallback on load error.
- Room cap is 4, so the stack is ≤4 — no overflow "+N" needed for now.

## 9. References

- Files: `server/src/{protocol,rooms}.rs`, `client/src/scripts/app.ts`, `client/src/pages/index.astro`.
- Related: [0070](../0070-call-chat-game-ux-fixes/spec.md) (`fillAvatar`), [0022](../0022-guest-public-room-gate/spec.md) (lobby).
