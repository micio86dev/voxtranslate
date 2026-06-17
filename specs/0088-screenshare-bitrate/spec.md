# 0088 — Separate (higher) bitrate for screen sharing

| | |
|---|---|
| **Status** | In progress |
| **Owner** | Micio Dev |
| **Created** | 2026-06-17 |
| **Shipped** | — |
| **Version** | — |
| **Depends on** | [0033](../0033-screen-share/spec.md), [0053](../0053-screen-share-pip/spec.md), [0031](../0031-adaptive-bitrate/spec.md) (per-peer budget) |

## 1. Context & Problem

Screen sharing reused the camera upload budget (`VIDEO_BUDGET_DESKTOP`, 2.4 Mbit/s,
split per peer). A face tolerates that; shared **text/UI does not** — it came
through grainy/soft. The owner asked for a **separate, env-tunable, higher** bitrate
that applies only while sharing.

## 2. Goals / Non-Goals

**Goals**
- A dedicated upload cap for screen sharing, higher than the camera budget, set by
  its own env var, applied only during a share and reverted on stop.
- Sharper shared content.

**Non-Goals**
- No change to the camera budget or the per-peer split / network adaptation
  (spec 0031/0032) — the new cap simply feeds the same machinery.
- Screen share stays desktop-only (one cap, no mobile/desktop split).

## 3. Design

- `webrtc.ts`: `setVideoBudget(budget)` replaces `videoBudget` + `currentBudget`
  and re-applies the per-stream cap immediately; network adaptation continues from
  the new value.
- `app.ts`:
  - `VIDEO_BUDGET_SCREEN = Number(PUBLIC_VIDEO_BUDGET_SCREEN) || 4_000_000`.
  - `startScreenShare`: set the composite track's `contentHint = 'detail'` (encoder
    favours sharpness over framerate for text), then
    `mesh.setVideoBudget(VIDEO_BUDGET_SCREEN)`.
  - `stopScreenShare`: `mesh.setVideoBudget(camera budget)` to revert.
- `client/.env.example`: documents `PUBLIC_VIDEO_BUDGET_SCREEN` (default 4 Mbit/s).

## 4. Testing & Verification

- `astro check` + build + vitest green. (The budget/bitrate path isn't unit-tested
  — the PC mock has no get/setParameters — same as the existing `applyBitrate`.)
- Manual (owner): share a screen with text → it's sharp, not grainy; raise
  `PUBLIC_VIDEO_BUDGET_SCREEN` if still soft.

## 5. Risks / Open Items

- The higher cap is still split per peer and still backs off under a weak uplink
  (spec 0032), so a poor network or a full room can lower the effective bitrate —
  raise the env value or accept the adaptation.

## 6. References

- Files: `client/src/scripts/app.ts`, `client/src/scripts/webrtc.ts`,
  `client/.env.example`.
