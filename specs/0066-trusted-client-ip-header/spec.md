# 0066 — Trusted client-IP header (Cloudflare-ready)

| | |
|---|---|
| **Status** | In progress |
| **Owner** | VoxTranslate |
| **Created** | 2026-06-15 |
| **Shipped** | — |
| **Version** | — |
| **Commits** | `<sha>` |
| **Depends on** | [0064](../0064-high-traffic-abuse-hardening/spec.md), [0050](../0050-observability/spec.md) |

## 1. Context & Problem

`observability::client_ip` resolves the client IP from the **last** `x-forwarded-for`
hop — the entry appended by the trusted proxy (Railway's edge), which a client can't
forge (issue #117/#120). That is correct for the *current* topology
(`client → Railway → app`).

Issue **#111** puts a **Cloudflare WAF** in front of the origin, changing the chain
to `client → Cloudflare → Railway → app`. Now the proxy-appended last hop is
**Cloudflare's** IP, so every visitor would collapse onto a single per-IP key — the
per-IP limits (spec 0064: `/ws` connect, `/rooms`, `/metrics`) and the IP field in
logs would all degrade. Cloudflare provides the real client IP in `CF-Connecting-IP`
and overwrites any client-supplied value, so it is trustworthy **while behind
Cloudflare**. We need a way to opt into that source without weakening the direct
(non-Cloudflare) case.

## 2. Goals / Non-Goals

**Goals**
- An **opt-in** trusted client-IP header (env `CLIENT_IP_HEADER`) so that, behind
  Cloudflare, per-IP limits and logs key on the real client IP.
- **Zero behaviour change when unset** (the default): identical to the current
  last-`x-forwarded-for`-hop resolution.
- No regression to the existing IP unit tests; the resolution stays a pure,
  testable function.

**Non-Goals**
- Enforcing a Cloudflare-only origin lock (shared-secret header / IP allowlist) —
  documented in the runbook as a follow-up.
- Per-route IP-trust policies; one process-wide source is enough.

## 3. Requirements

- **R1 — Default unchanged.** *Given* `CLIENT_IP_HEADER` is unset, *when* `client_ip`
  runs, *then* it returns exactly what it does today (last XFF hop → `x-real-ip` → `-`).
- **R2 — Trusted header wins.** *Given* `CLIENT_IP_HEADER=cf-connecting-ip`, *when* the
  request carries that header, *then* its value is used (over the XFF last hop, which
  is now Cloudflare's IP).
- **R3 — Clean fallback.** *Given* the trusted header is configured but absent on a
  request, *then* resolution falls back to the existing XFF/`x-real-ip`/`-` chain.

## 4. Design & Architecture

- **File:** `server/src/observability.rs`.
- `client_ip(&HeaderMap)` keeps its signature (it's called from the `canonical_log`
  middleware — which has no `State` — and from the rate-limit handlers), and
  delegates to a pure `resolve_client_ip(trusted: Option<&str>, &HeaderMap)`.
- `TRUSTED_IP_HEADER: LazyLock<Option<String>>` reads `CLIENT_IP_HEADER` **once**
  (lower-cased, empty → `None`), matching the "config from env, read once" pattern.
- **Key decision — opt-in, not auto-detect.** Trusting `CF-Connecting-IP`
  unconditionally would re-open the spoofing hole #117 closed when the origin is
  reachable directly. Gating on an explicit env var means the owner only enables it
  at Cloudflare cutover (per the #111 runbook), and the default stays safe.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `resolve_client_ip(trusted, h)` pure fn + `TRUSTED_IP_HEADER` LazyLock; `client_ip` delegates | `server/src/observability.rs` |
| S1 | Unit test: default unchanged + trusted-header-wins + absent-fallback | `server/src/observability.rs` |
| S2 | Runbook step (`CLIENT_IP_HEADER=cf-connecting-ip` at cutover) | `docs/runbooks/111-cloudflare-waf.md` |

## 6. Testing & Verification

- **Unit:** `client_ip_uses_last_forwarded_hop` (unchanged, R1) and
  `trusted_header_wins_and_default_is_unchanged` (R1/R2/R3) — the pure fn is tested
  with `None` and `Some("cf-connecting-ip")` without touching process env.
- **Suite:** full `cargo test` green + `clippy -D warnings` + `fmt`.

## 7. Deployment & Operations

- New optional env **`CLIENT_IP_HEADER`** (server). Leave unset until Cloudflare
  (#111) is actually in front; then set `cf-connecting-ip`. Unsetting reverts to the
  unforgeable last-XFF-hop source. Railway env change redeploys the current image.

## 8. Risks / Open Items

- If the owner sets `CLIENT_IP_HEADER` *without* a proxy that overwrites the header,
  a client could forge it — mitigated by it being opt-in and documented as
  "Cloudflare-only" in the runbook.
- Origin-lock (so attackers can't bypass Cloudflare and hit Railway directly) is a
  separate follow-up (#111 runbook §5).

## 9. References

- Issue #111 (WAF), #114 (audit), #117/#120 (IP source). Builds on specs 0064/0050.
- Files: `server/src/observability.rs`, `docs/runbooks/111-cloudflare-waf.md`.
