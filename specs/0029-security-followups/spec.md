# 0029 — Security Hardening Follow-ups

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-12 |
| **Shipped** | 2026-06-12 |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0028](../0028-security-hardening/spec.md) |

## 1. Context & Problem

The security pass ([0028](../0028-security-hardening/spec.md)) fixed the
high-impact items and listed the rest as tracked follow-ups in
[`SECURITY.md`](../../SECURITY.md). This spec clears the **safe, server-side**
ones that can be verified with `cargo`/CI — leaving only those that genuinely
need runtime/preview testing or are larger features (full CSP, ICE-restart, a
`getStats` panel, JWT revocation) deferred.

## 2. Goals / Non-Goals

**Goals**
- Bound the upload and PDF-extraction DoS surface; throttle uploads.
- Stop the rate-limiter's unbounded memory growth.
- Surface dependency advisories in CI (non-blocking).

**Non-Goals**
- Full enforcing CSP (needs a Vercel-preview test — could break GSI/WS/Stripe).
- ICE-restart resilience + a `getStats` diagnostics panel (need real cross-network
  testing / are a UI feature) — tracked in [0026](../0026-turn-relay/spec.md) §8.
- Glossary room-membership authz (needs `user_id` on the room `Peer`, threaded
  from the WS join — a bigger change).
- JWT revocation, signed-URL TTL, `X-Forwarded-For` trust — deliberate trade-offs
  left to the owner (`SECURITY.md`).

## 3. Requirements

- **R1 — Upload throttle.** A peer's uploads are capped (10/min per
  `room:peer`) — each one drives Deepgram/Groq.
- **R2 — PDF timeout.** PDF text extraction is bounded by a 15 s timeout so a
  decompression-bomb PDF can't pin the request.
- **R3 — Rate-limiter memory bound.** Stale keys are pruned once the map grows
  past 10 k entries (per-uuid / per-IP keys no longer accumulate forever).
- **R4 — CI advisory scan.** A non-blocking `cargo audit` + `npm audit` job runs in
  CI (informational; Dependabot remains the real gate).

## 4. Design & Architecture

- `server/src/files.rs` — `state.rate_limiter.allow("upload:{room}:{peer}", …)`
  after the membership check; `tokio::time::timeout(15s, spawn_blocking(extract))`
  for PDFs (the blocking task may run on, but the request returns).
- `server/src/rate_limit.rs` — in `allow()`, when `hits.len() > 10_000`,
  `retain` only entries whose window hasn't elapsed.
- `.github/workflows/ci.yml` — a `security-audit` job (`cargo-audit` via
  `taiki-e/install-action`; `npm audit --audit-level=high`), both
  `continue-on-error: true` so the pipeline never fails on the pre-existing
  dev-only npm advisories.
- **Key decisions:**
  - *Timeout, not cancellation* — `spawn_blocking` can't be killed, but the
    timeout returns control to the request; the worst case is one busy thread, not
    a stalled request.
  - *Prune-on-growth, not a new dep* — keeps `RateLimiter` dependency-free vs.
    pulling in `moka`; pruning is O(n) but only past a 10 k threshold.
  - *Audit informational, not gating* — a hard gate would red-X CI on the 3
    dev-only npm highs (esbuild/vite/astro) until the Astro 6 major upgrade.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | Upload throttle + PDF timeout | `files.rs` |
| S1 | Rate-limiter eviction | `rate_limit.rs` |
| S2 | Non-blocking CI audit job | `.github/workflows/ci.yml` |
| S3 | Update the `SECURITY.md` follow-up ledger | `SECURITY.md` |

## 6. Testing & Verification

- `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` clean; server
  builds; the existing upload/rate-limit tests still pass (a single upload is well
  under the 10/min cap; eviction triggers only past 10 k keys).

## 7. Deployment & Operations

- Server-only — takes effect on the next `railway up`. No env, no migration. The CI
  job runs on every push/PR.

## 8. Risks / Open Items

- Remaining items stay in [`SECURITY.md`](../../SECURITY.md) "Tracked follow-ups":
  full CSP, ICE-restart + `getStats` panel, glossary authz, JWT revocation,
  signed-URL TTL, `X-Forwarded-For` trust.

## 9. References

- Follow-up ledger: [`SECURITY.md`](../../SECURITY.md).
- Files: `server/src/{files,rate_limit}.rs`, `.github/workflows/ci.yml`.
