# 0031 — Room-size-adaptive Video Bitrate

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-12 |
| **Shipped** | 2026-06-12 |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0030](../0030-mobile-bitrate-weak-network/spec.md) |

## 1. Context & Problem

[Spec 0030](../0030-mobile-bitrate-weak-network/spec.md) capped each outbound video
stream at a **fixed** bitrate (~1 Mbps desktop / ~450 kbps mobile). But in a mesh
each peer sends `N−1` copies, so the **total uplink still grows linearly** with the
room: a fixed 1 Mbps/stream means ~1 Mbps up at 2-way but ~3 Mbps up at 4-way — the
exact moment the uplink is most stressed. The fix: cap per stream by a **total
upload budget split across the peers**, so the total uplink stays ~constant as the
room fills, and each stream gets the *most* possible when the room is small.

## 2. Goals / Non-Goals

**Goals**
- Per-stream bitrate = `budget ÷ peers`, re-applied whenever the peer count changes.
- A floor so video stays usable in a full room; the full budget on one stream when
  it's just two people.

**Non-Goals**
- Per-peer or per-network individual tuning (still one target for all senders).
- Changing the resolution ladder or the 4-peer cap (0030).

## 3. Requirements

- **R1 — Split by peers.** Each video sender's `maxBitrate` = `max(floor,
  budget / peerCount)`. Budget: **2.4 Mbps desktop / 1.2 Mbps mobile**; floor
  **200 kbps**.
- **R2 — Re-balance on change.** When a peer **joins or leaves**, re-apply the cap
  to **all** senders (the divisor changed).
- **R3 — Bounded total.** Total outbound video ≈ the budget regardless of N
  (≈ 2.4 Mbps desktop / 1.2 Mbps mobile at any room size up to 4).

## 4. Design & Architecture

- `client/src/scripts/webrtc.ts` — `MeshManager`'s `maxVideoBitrate` becomes a
  `videoBudget`; `targetBitrate()` = `max(MIN_VIDEO_BITRATE, ⌊budget / peers.size⌋)`;
  `applyBitrate()` sets that on every peer's video sender and is called at the end
  of `addPeer` (peer already counted) and after `removePeer`'s delete.
- `client/src/scripts/app.ts` — passes the **budget** (`IS_MOBILE ? 1.2 Mbps :
  2.4 Mbps`) instead of a per-stream cap.
- **Worked example (desktop, 2.4 Mbps budget):** 2-way → 1 stream @ 2.4 Mbps (but
  the browser/resolution rarely needs that); 3-way → 2 @ 1.2 Mbps; 4-way → 3 @
  0.8 Mbps. Total uplink ≈ 2.4 Mbps throughout.
- **Key decisions:**
  - *Divide a budget, don't fix per-stream* — bounds the total uplink (the real
    constraint in a mesh), and gives small rooms higher quality for free.
  - *Re-apply on every join/leave* — `setParameters` is cheap and the count is the
    only input; no need to track deltas.
  - *Floor at 200 kbps* — below it video is pointless; better to hold quality and
    let congestion control / the weak-network warning (0030) take over.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `videoBudget` + `targetBitrate()` + `applyBitrate()` | `webrtc.ts` |
| S1 | Re-balance on add/remove peer | `webrtc.ts` |
| S2 | Pass the budget from the app | `app.ts` |

## 6. Testing & Verification

- `astro check` clean; **101/101** unit tests; coverage thresholds met
  (lines 91.6 % / functions 86 %); production build OK.
- `webrtc.test.ts` unchanged-and-passing: adding/removing peers now also calls
  `applyBitrate` (guarded, no-ops under the fake `RTCPeerConnection`).
- Manual: join 2→3→4 and watch each tile's `outbound-rtp.targetBitrate` in
  `chrome://webrtc-internals` step down as peers join.

## 7. Deployment & Operations

- Client-only — ships with the Vercel autodeploy on `main`. No server change.

## 8. Risks / Open Items

- The budget is a single global figure; a future refinement could feed the
  `getStats` signal (0030) back into the budget (lower it when the uplink is
  struggling) — i.e. fully adaptive, not just room-size-aware.

## 9. References

- Amends the fixed cap from [0030](../0030-mobile-bitrate-weak-network/spec.md).
- Files: `client/src/scripts/{webrtc,app}.ts`.
