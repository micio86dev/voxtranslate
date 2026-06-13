# 0044 — Video upload budget configurable via Vercel env

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-13 |
| **Shipped** | 2026-06-13 |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0030](../0030-mobile-bitrate-weak-network/spec.md), [0031](../0031-adaptive-bitrate/spec.md), [0032](../0032-adaptive-budget/spec.md) |

## 1. Context & Problem

The per-room video **upload budget** (total bit/s, split across peers and
network-adapted — specs 0030–0032) was hardcoded in the client: mobile 1.2 Mbit/s,
desktop 2.4 Mbit/s. The owner wanted those tunable from **Vercel** env vars (not in
code), keeping the current values. Note: this is **client** config — Railway hosts the
*server* and has nothing to do with it; the client is a static Astro/Vite build whose
only env mechanism is **build-time** `PUBLIC_*` vars.

## 2. Goals / Non-Goals

**Goals**
- Source the mobile/desktop budgets from Vercel build-time env, with the current
  values as defaults so nothing changes until the vars are set.

**Non-Goals**
- Runtime (no-redeploy) tuning — a static build bakes env at build time; changing a
  Vercel var requires a client redeploy.
- Changing the per-peer split or AIMD logic (0031/0032 unchanged).

## 3. Requirements

- **R1 — Env-sourced budgets.** `VIDEO_BUDGET_MOBILE` / `VIDEO_BUDGET_DESKTOP` read
  `import.meta.env.PUBLIC_VIDEO_BUDGET_MOBILE` / `PUBLIC_VIDEO_BUDGET_DESKTOP`.
- **R2 — Safe fallback.** Unset / empty / non-numeric → the prior values
  (1_200_000 / 2_400_000). Verified: with the env unset the bundle bakes `12e5`/`24e5`
  (identical behaviour).

## 4. Design & Architecture

- `client/src/scripts/app.ts` — two module consts:
  `Number(import.meta.env.PUBLIC_VIDEO_BUDGET_MOBILE) || 1_200_000` (and desktop);
  `new MeshManager(...)` uses `IS_MOBILE ? VIDEO_BUDGET_MOBILE : VIDEO_BUDGET_DESKTOP`.
- `client/.env.example` — documents the two vars (+ the existing `PUBLIC_WS_HOST`),
  all optional with their fallbacks.
- **Key decision:** *fallbacks equal the current values* — the change is a no-op until
  the vars are set on Vercel, so there's zero deploy-order risk and the existing
  behaviour is preserved exactly.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Read budgets from `PUBLIC_*` env, current-value fallbacks | `app.ts` |
| S1 | Document the env vars | `.env.example` |

## 6. Testing & Verification

- `astro check` clean; **101/101** unit tests; build OK.
- Bundle inspection: env **unset** → `12e5`/`24e5` baked (= 1.2 / 2.4 Mbit/s) → no
  behaviour change.

## 7. Deployment & Operations

- **Client-only** — ships via the Vercel autodeploy on `main`.
- **To actually move/tune the values:** add on the **Vercel** project (Settings →
  Environment Variables), then redeploy the client:
  - `PUBLIC_VIDEO_BUDGET_MOBILE=1200000`
  - `PUBLIC_VIDEO_BUDGET_DESKTOP=2400000`
  (These live on **Vercel**, not Railway. Build-time → a change needs a redeploy.)

## 8. Risks / Open Items

- Build-time only: not a live runtime knob. A true runtime control would mean sending
  the budget from the server over WS — deferred (more work, not requested).

## 9. References

- Files: `client/src/scripts/app.ts`, `client/.env.example`.
