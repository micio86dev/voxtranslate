# 0032 — Network-adaptive Video Budget (AIMD)

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-12 |
| **Shipped** | 2026-06-12 |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0030](../0030-mobile-bitrate-weak-network/spec.md), [0031](../0031-adaptive-bitrate/spec.md) |

## 1. Context & Problem

[0031](../0031-adaptive-bitrate/spec.md) split the upload budget by room size, but
the budget itself was **fixed**. Two users on a great connection got the same total
as two users on a congested one. We already poll `getStats` for the weak-network
warning (0030) — feed that signal back into the **budget** so it shrinks when the
uplink struggles and grows back when it recovers, closing the loop into a
fully-adaptive cap.

## 2. Goals / Non-Goals

**Goals**
- A live `currentBudget` that backs off on a struggling uplink and recovers toward
  the configured max — an outer control loop above the browser's own congestion
  control.

**Non-Goals**
- Per-peer adaptation (still one budget for all our senders).
- Replacing the browser's congestion control — this just tightens/loosens the
  `maxBitrate` ceiling it works under.

## 3. Requirements

- **R1 — Back off.** When the uplink is weak (video `qualityLimitationReason ===
  'bandwidth'` or remote `fractionLost > 8 %`), `currentBudget ×= 0.75` (floored at
  the per-stream minimum).
- **R2 — Recover.** When healthy, `currentBudget ×= 1.2`, capped at the configured
  `videoBudget`.
- **R3 — Re-apply.** On any budget change, re-push the new per-stream caps to all
  senders. `targetBitrate()` divides `currentBudget` (not the static `videoBudget`)
  by the peer count.

## 4. Design & Architecture

- `client/src/scripts/webrtc.ts` — add `currentBudget` (starts at `videoBudget`);
  `targetBitrate()` uses it; the 5 s `getStats` monitor adjusts it AIMD-style and
  calls `applyBitrate()` when it changes.
- **Key decisions:**
  - *Multiplicative decrease / gentle multiplicative increase* — fast to relieve
    congestion, slow to re-probe (classic AIMD-ish), so it settles rather than
    oscillates.
  - *Outer loop over the browser's congestion control* — we only move the
    `maxBitrate` ceiling; the browser still adapts underneath, so the two cooperate
    (a lower ceiling = less to lose; a higher ceiling = room to grow when good).
  - *Reuse the existing weak signal + 5 s cadence* — no new polling; the warning
    and the budget share the same detector.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `currentBudget` + AIMD adjust in the stats monitor | `webrtc.ts` |

## 6. Testing & Verification

- `astro check` clean; **101/101** unit tests; coverage thresholds met; build OK.
- The browser-only adaptation is validated manually: throttle the uplink in dev
  tools → each tile's `targetBitrate` steps down within a few seconds, then climbs
  back when the throttle is lifted (`chrome://webrtc-internals`).

## 7. Deployment & Operations

- Client-only — ships with the Vercel autodeploy on `main`. No server change.

## 8. Risks / Open Items

- The fixed 0.75/1.2 factors are heuristic; if it oscillates in the field, widen the
  hysteresis (require N consecutive healthy checks before increasing).

## 9. References

- Closes [0031](../0031-adaptive-bitrate/spec.md) §8 (make the budget itself adaptive).
- Files: `client/src/scripts/webrtc.ts`.
