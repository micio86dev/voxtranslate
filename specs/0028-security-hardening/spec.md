# 0028 — Security Hardening Pass

| | |
|---|---|
| **Status** | ✅ Shipped (fixes); follow-ups tracked |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-12 |
| **Shipped** | 2026-06-12 |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0005](../0005-accounts-credits-billing/spec.md), [0026](../0026-turn-relay/spec.md) |

## 1. Context & Problem

The owner asked for a general security review (they're not a security expert) to
guard against XSS/account-takeover, abuse, and supply-chain risk. A defensive audit
(client + server + infra) found the codebase largely disciplined — parameterized
SQL, verified JWT/Stripe signatures, owner-scoped data access — with **one real
exploitable XSS** and several hardening gaps (open CORS, missing security headers,
unthrottled expensive endpoints, no supply-chain scanning). This spec fixes the
clear, high-value, low-risk items and tracks the rest. Full report: [`SECURITY.md`](../../SECURITY.md).

## 2. Goals / Non-Goals

**Goals**
- Close the exploitable XSS and the open-CORS finding.
- Throttle the cost/abuse surfaces (Groq/Resend/TURN), cap WS chat, add security
  headers, and start supply-chain scanning.
- Document every finding (fixed + tracked) and a maintenance mini-guide.

**Non-Goals**
- A professional third-party pentest.
- The tracked follow-ups (glossary authz, full CSP, JWT revocation, rate-limiter
  eviction, upload throttle, CI audit gate) — listed in `SECURITY.md`, deferred so
  this pass stays low-risk.

## 3. Requirements (fixes)

- **R1 — No peer-name XSS.** A peer display name renders as text, never HTML.
- **R2 — CORS allowlist.** With `ALLOWED_ORIGINS` set, only those origins are
  allowed; unset → permissive (dev only).
- **R3 — Throttle expensive endpoints.** Per-user limits on AI report/sentiment/
  email-draft and a tight cap on email-send; per-IP limit on `/api/ice`.
- **R4 — Security headers.** HSTS, nosniff, `X-Frame-Options: DENY`,
  `Referrer-Policy`, a `Permissions-Policy` that still allows camera/mic/display-
  capture, and a `frame-ancestors/object-src/base-uri` CSP (client); a nosniff
  layer (server).
- **R5 — Bounded chat.** WS `Chat` text > 8 KB is dropped before translation.
- **R6 — Safe links.** Legal-page markdown blocks `javascript:`/`data:` schemes.
- **R7 — TURN abuse bounds.** coturn quotas + RFC-1918 peer denylist.
- **R8 — Supply chain.** Dependabot for cargo/npm/actions.

## 4. Design & Architecture

- **Client:** `app.ts` (textContent), `content.ts` (scheme denylist),
  `vercel.json` (headers), `content.test.ts` (pins R6).
- **Server:** `lib.rs` (CORS from config + nosniff layer + 8 KB chat cap),
  `api.rs` (per-user/per-IP `RateLimiter.allow` guards on the AI/email/ice
  handlers), `Cargo.toml` (`tower-http` `set-header` feature).
- **Infra:** `infra/coturn/turnserver.conf` (quotas), `.github/dependabot.yml`.
- **Key decisions:**
  - *Denylist dangerous link schemes, not allowlist absolute ones* — an allowlist
    broke relative links (`/terms`); blocking `javascript:`/`data:`/`vbscript:`/
    `file:` keeps relative + http(s)/mailto intact.
  - *Minimal CSP now* — `frame-ancestors/object-src/base-uri` are safe everywhere; a
    full `script-src`/`connect-src` CSP needs preview testing (GSI/WS/Stripe), so
    it's a tracked follow-up rather than a risky guess.
  - *Reuse the existing `RateLimiter`* — same pattern as login/checkout; per-user
    keys where authenticated, per-IP (XFF) for the anonymous `/api/ice`.
  - *Dependabot, not a blocking audit gate* — surfaces fixes without risking the
    pipeline on the pre-existing dev-only npm advisories (Astro 6 is a major bump).

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | XSS → textContent; link-scheme denylist + test | `app.ts`, `content.ts`, `content.test.ts` |
| S1 | Security headers + nosniff | `vercel.json`, `lib.rs` |
| S2 | CORS allowlist | `lib.rs` |
| S3 | Rate limits (AI/email/ice) + chat cap | `api.rs`, `lib.rs` |
| S4 | coturn quotas + Dependabot | `infra/coturn/turnserver.conf`, `.github/dependabot.yml` |
| S5 | Report + maintenance guide | `SECURITY.md` |

## 6. Testing & Verification

- `cargo clippy --all-targets -- -D warnings` clean; `astro check` clean;
  **100/100** client unit tests (incl. the new link-scheme test); production build OK.
- The CORS allowlist is exercised only when `ALLOWED_ORIGINS` is set (prod);
  unset keeps dev behaviour unchanged.

## 7. Deployment & Operations

- **Client** ships with the Vercel autodeploy on `main` (headers take effect on
  deploy). **Server** needs a redeploy (`railway up`) for CORS/rate-limits/headers;
  set `ALLOWED_ORIGINS` to your real domains to activate the allowlist.

## 8. Risks / Open Items

- See `SECURITY.md` "Tracked follow-ups" — the items deliberately deferred to keep
  this pass low-risk.
- The minimal CSP doesn't yet constrain scripts/connections; the XSS fix + headers
  reduce the blast radius in the meantime.

## 9. References

- Full report + maintenance guide: [`SECURITY.md`](../../SECURITY.md).
- Files: `client/src/scripts/{app,content}.ts`, `client/vercel.json`,
  `server/src/{lib,api}.rs`, `server/Cargo.toml`,
  `infra/coturn/turnserver.conf`, `.github/dependabot.yml`.
