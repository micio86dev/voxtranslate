# 0041 — Issue #50: room-visibility consistency + mobile blur aspect

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-13 |
| **Shipped** | 2026-06-13 |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0002](../0002-video-calls-translated-chat/spec.md), [0017](../0017-virtual-background/spec.md), [0022](../0022-guest-public-room-gate/spec.md), [0033](../0033-screenshare-pan-zoom/spec.md), [0034](../0034-ui-cta-zfix/spec.md) |

## 1. Context & Problem

GitHub issue **#50** bundles four reports. This spec resolves the two that are clean,
safe code fixes, records that a third was already fixed, and defers the fourth (it's a
feature, not a regression):

1. **Public/Private mismatch.** Joining a room by a *private* code still showed
   "Public" in the in-call badge. Cause: the badge displayed the **joiner's own
   visibility toggle** (default public), not the room's real visibility. The server
   knows the truth (the room stores its visibility; a later joiner's `public` param
   doesn't change it) but never sent it to the client. → **fixed.**
3. **Mobile camera blur distorted/stretched.** The blur canvas was sized from
   `track.getSettings()`, which on mobile disagrees with the actual decoded frame
   (camera reports landscape while the frame is portrait), so the composite stretched.
   → **fixed.**
2. **Mobile "⋯" menu under the video.** Already addressed in spec 0034: `.more-menu`
   has `z-index: 30` + its own `translateZ(0)` compositor layer so it paints above the
   GPU-composited video tiles. → **no change** (verify on the current build).
4. **Camera disappears during desktop screen-share.** Current design replaces the
   outgoing camera track with the screen track, so the sharer's tile shows the screen.
   Keeping **both** (screen + camera PiP) needs simultaneous dual-track send +
   renegotiation in the mesh — a feature, not a quick fix. → **deferred** (tracked).

## 2. Goals / Non-Goals

**Goals**
- The in-call visibility badge always reflects the room's **real** visibility.
- Camera blur keeps correct aspect on mobile (no stretch/distortion).

**Non-Goals**
- Dual-stream screen-share + camera PiP (#50.4) — separate feature.
- Background *replacement* (still blur-only, per 0017).

## 3. Requirements

- **R1 — Server-authoritative visibility.** `RoomJoined` carries `public: bool` (the
  room's actual visibility). On `room_joined` the client sets `session.isPublic` and
  the `#call-vis` badge from it. Forward-compatible: the client only overrides when
  the field is present (a `boolean`), so deploy order doesn't break anything.
- **R2 — Blur aspect.** The blur canvas is sized from the live decoded frame
  (`video.videoWidth/Height`, falling back to `getSettings()` then 640×480) and
  re-synced in `draw()` if the frame size changes (rotation), so the output never
  stretches.

## 4. Design & Architecture

- **Server** (`server/src/`): `rooms.rs` `Joined` gains `public` (set from the room's
  `Visibility`); `protocol.rs` `RoomJoined` gains `public: bool`; `lib.rs` captures
  `room_public` and sends it. Protocol test literals updated.
- **Client** (`client/src/scripts/`): `app.ts` `room_joined` reads `msg.public`;
  `virtual-background.ts` sizes/keeps the canvas at the real frame size.
- **Key decisions:**
  - *Visibility is server state, not a client toggle.* The joiner's toggle only
    matters when *creating* a room; for an existing room the server's value is
    authoritative, so the client must render that. Sending it on join is one additive,
    backward-compatible field.
  - *Canvas from `videoWidth`, not `getSettings()`.* The decoded frame is the only
    reliable source of the real aspect across mobile browsers/orientations.
  - *Defer dual-stream share.* Replacing camera with screen is the safe, common
    behavior; adding a simultaneous camera PiP is a renegotiation-heavy feature that
    shouldn't be rushed in under a bug-fix banner.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `RoomJoined.public` end-to-end | `rooms.rs`, `protocol.rs`, `lib.rs`, `app.ts` |
| S1 | Blur canvas sized from the real frame | `virtual-background.ts` |

## 6. Testing & Verification

- Server: `cargo build` clean; `room_joined` protocol test passes with the new field.
- Client: `astro check` clean; **101/101** unit tests; production build OK.
- Manual: join a private room by code → badge reads **Private** everywhere; enable
  blur on a phone in portrait → subject + blur keep correct aspect, no stretch.

## 7. Deployment & Operations

- **Server change** → needs `railway up` (the `RoomJoined.public` field). Until the
  server ships, the client guard leaves the old (toggle-based) badge — no breakage.
- **Client change** → Vercel autodeploy on `main`.

## 8. Risks / Open Items

- **#50.4 (camera during screen-share)** remains open — implement as a dual-stream /
  PiP feature (send camera + screen, renegotiate) in a dedicated spec.
- **#50.2** relies on the 0034 fix; if a device still shows it, capture the exact
  model/browser and revisit the stacking context.

## 9. References

- Issue: #50
- Files: `server/src/rooms.rs`, `server/src/protocol.rs`, `server/src/lib.rs`,
  `client/src/scripts/app.ts`, `client/src/scripts/virtual-background.ts`.
