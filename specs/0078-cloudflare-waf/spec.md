# 0078 — Cloudflare WAF in front of the origin + origin lock

| | |
|---|---|
| **Status** | Code shipped (dormant) — WAF activation owner-side (runbook) |
| **Owner** | VoxTranslate |
| **Created** | 2026-06-16 |
| **Shipped** | 2026-06-16 (code) |
| **Version** | — |
| **Commits** | `5f92571` (#181) |
| **Depends on** | [0064](../0064-high-traffic-abuse-hardening/spec.md), [0066](../0066-trusted-client-ip-header/spec.md), [0075](../0075-csp-enforce/spec.md) |

## 1. Context & Problem

The Railway origin (`voxtranslate-server-production.up.railway.app`) is **exposed
directly** — clients hit `wss://<origin>/ws` and `/rooms` straight at it. Vercel's
firewall protects only the static frontend, so a botnet can flood the origin's WS /
`/rooms` / `/metrics` and bypass every edge protection, exhausting the single Railway
instance (issue #111 — the audit's "security gap #1"). The in-app per-IP limits
(spec 0064) help but run *on* the box being flooded.

The zone `voxtranslate.app` is already on Cloudflare (nameservers `*.ns.cloudflare.com`),
so we can put the API behind the **Cloudflare WAF + DDoS** via a proxied subdomain and
then **lock the origin** so it only accepts Cloudflare-proxied traffic.

This is mostly an owner-side dashboard task (DNS/WAF/rules), but it needs **code
support**: an origin-lock guard, the trusted-IP header (already shipped, spec 0066),
and a CSP that allows the new API host. This spec ships that code (dormant) and the
operator runbook.

## 2. Goals / Non-Goals

**Goals**
- A server **origin lock**: reject any request except `/health` that lacks the
  Cloudflare-injected `x-origin-verify` secret — *only* when `CF_ORIGIN_SECRET` is
  set (dormant otherwise). Blocks direct-to-origin flooding that skips the WAF.
- CSP allows `api.voxtranslate.app` (https + wss) so the client can move behind
  Cloudflare without a CSP break.
- A complete, ordered **runbook** for the Cloudflare + Railway + Vercel steps.

**Non-Goals**
- The actual Cloudflare config (DNS/WAF/rate-rules/Transform Rule) — owner-side,
  documented in `infra/cloudflare/README.md`.
- Cloudflare **Authenticated Origin Pulls** (mTLS) — Railway doesn't expose origin
  client-cert verification, so we use a shared-secret header instead.
- Multi-region / moving the frontend behind Cloudflare (apex stays Vercel-direct).

## 3. Requirements

- **R1 — Origin lock (armed).** With `CF_ORIGIN_SECRET` set, a request without a
  matching `x-origin-verify` header gets `403` — *except* `/health`.
  - *Given* the secret is set, *when* a request hits any path but `/health` without
    the header, *then* `403`; *with* the header, it proceeds.
- **R2 — Healthcheck never blocked.** `/health` is always allowed (Railway's
  platform healthcheck hits the origin directly, bypassing Cloudflare).
- **R3 — Dormant by default.** With `CF_ORIGIN_SECRET` unset, the guard is a
  pass-through (no behaviour change) — safe to ship before Cloudflare is configured.
- **R4 — CSP ready.** `connect-src` allows `https://api.voxtranslate.app` +
  `wss://api.voxtranslate.app` alongside the current Railway host (additive, so the
  cutover doesn't break the live client).

## 4. Design & Architecture

- **`server/src/lib.rs`:** `AppState.cf_origin_secret: Option<String>` (from
  `CF_ORIGIN_SECRET`); `origin_lock` middleware layered **outermost** via
  `from_fn_with_state` so bad requests are rejected before any work; pure
  `origin_header_ok(headers, secret)` helper (unit-tested). `/health` exempt by path.
- **`client/vercel.json`:** add the `api.voxtranslate.app` origins to `connect-src`.
- **`server/.env.example`:** document `CF_ORIGIN_SECRET` + `CLIENT_IP_HEADER`.
- **`infra/cloudflare/README.md`:** the ordered runbook (proxied DNS, WAF managed
  ruleset, rate-limit rules on `/ws`/`/rooms`/`/api/*`, DDoS, Transform Rule that
  injects the secret, cutover order, external-caller updates, verify, rollback).
- **Key decisions:**
  - *Shared-secret header, not mTLS* — Railway can't verify origin client certs;
    a Cloudflare Transform Rule injecting `x-origin-verify` is the pragmatic lock.
  - *`/health` exempt* — non-negotiable; blocking it fails every Railway deploy.
  - *Arm the lock last* — set `CF_ORIGIN_SECRET` only after the client is cut over
    to `api.voxtranslate.app`, or live traffic (still on the Railway host, no
    header) would 403.
  - *Additive CSP* — keep the Railway host during transition; drop it in a later
    cleanup once fully on the API domain.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `cf_origin_secret` on `AppState` + env read | `server/src/lib.rs` |
| S1 | `origin_lock` middleware (outermost, `/health` exempt) + pure `origin_header_ok` + test | `server/src/lib.rs` |
| S2 | CSP `connect-src` allows the api host | `client/vercel.json` |
| S3 | Env docs | `server/.env.example` |
| S4 | Operator runbook | `infra/cloudflare/README.md` |

## 6. Testing & Verification

- **Unit:** `origin_header_ok` matches only the exact secret; missing/wrong header
  rejected. `cargo clippy --all-targets` clean, `cargo fmt`.
- **Post-config (owner, per runbook):** `curl …railway.app/rooms` → 403 (direct);
  `…railway.app/health` → ok (exempt); `api.voxtranslate.app/health` → ok via
  Cloudflare; a real call connects with WS upgraded through Cloudflare.

## 7. Deployment & Operations

- Code ships **dormant** (no env vars ⇒ unchanged). Activation = the
  `infra/cloudflare/README.md` runbook + setting `CF_ORIGIN_SECRET` /
  `CLIENT_IP_HEADER` on Railway and `PUBLIC_WS_HOST` on Vercel.
- Cost/quota: Cloudflare proxy + WAF on the existing zone (no new spend on the
  free/pro tier beyond current).

## 8. Risks / Open Items

- **Mis-ordered activation** — arming `CF_ORIGIN_SECRET` before the client cutover
  403s live traffic. Mitigated by the runbook's explicit order + `/health` exemption.
- **External direct callers** (Stripe webhook, Better Stack monitors) must move to
  the API domain or be exempt — called out in the runbook.
- Still a single origin instance; the WAF caps flood reaching it but capacity is
  unchanged (audit #114 §1).

## 9. References

- Issue: #111 (audit master #114 §5).
- Files: `server/src/lib.rs`, `client/vercel.json`, `server/.env.example`,
  `infra/cloudflare/README.md`.
- External: [Cloudflare WAF](https://developers.cloudflare.com/waf/),
  [Transform Rules — set request header](https://developers.cloudflare.com/rules/transform/request-header-modification/).
