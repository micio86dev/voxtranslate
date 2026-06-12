# Security

A defensive security review of VoxTranslate (client + server + infra) was run on
2026-06-12 across XSS/DOM injection, SQL injection, authn/authz (IDOR), the admin
and WebSocket surfaces, Stripe, CORS, rate-limiting, security headers, DoS limits,
secret handling, file upload, and CI/supply-chain. This file records the findings,
what was fixed (spec [0028](specs/0028-security-hardening/spec.md)), what's tracked,
and how to keep the app secure.

> Found a vulnerability? Email the owner (see the repo profile). Please don't open
> a public issue for anything exploitable.

## What's already solid

- **No SQL injection** — every query is parameterized (sqlx `bind()`/macros); the
  few `format!`-built queries interpolate only fixed column constants.
- **JWT** signature + expiry are verified with the algorithm pinned to HS256 (no
  `alg=none`/confusion). Google ID tokens verify `aud == client_id`.
- **Stripe webhook** verifies the HMAC-SHA256 signature *before* trusting the event,
  with constant-time compare; crediting is idempotent on the Stripe `event_id`.
- **Admin** endpoints use a constant-time shared-secret extractor that runs before
  body parsing, and every action writes an `admin_audit` row.
- **IDOR-safe**: transcript/report/sentiment/email/bookmark/billing/usage endpoints
  are participant- or owner-scoped, not merely "logged-in".
- **WS `from` can't be spoofed** (server stamps the connection id); relay is
  room-scoped.
- **Secrets** are never logged and never serialized into responses (`skip_serializing`,
  server-only keys; only the *derived* TURN credential is emitted).
- **File upload**: private bucket + signed URLs + forced-safe content-types, and the
  storage key is sanitized (path-traversal tested) — no stored-HTML XSS from our
  origin.

## Fixed in this pass (spec 0028)

| Sev | Issue | Fix |
|-----|-------|-----|
| 🔴 High | **Stored/relayed XSS** — a peer's display name went to `innerHTML` (`app.ts`), so a name like `<img onerror=…>` ran JS in every participant's tab and could exfiltrate the localStorage JWT. | Render the name with `textContent`. |
| 🔴 High | **CORS `permissive()` on all routes**; the `ALLOWED_ORIGINS` config was dead code, so any site could call the API. | Build the CORS layer from `allowed_origins` (allowlist in prod; permissive only when unset for dev). |
| 🟠 Med-High | **Expensive endpoints unthrottled** — AI report/sentiment/email-draft (Groq), email-send (Resend), and `/api/ice` (TURN cred minting) had no rate limit (cost/abuse). | Per-user limits on the AI/email handlers; tight per-user cap on email-send; per-IP limit on `/api/ice`. |
| 🟠 Medium | **No security headers** (clickjacking, MIME-sniff, no HSTS). | `vercel.json`: HSTS, `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`, `Referrer-Policy`, a scoped `Permissions-Policy` (keeps camera/mic/display-capture for calls), and a `frame-ancestors/object-src/base-uri` CSP. Server: a `nosniff` response-header layer. |
| 🟠 Medium | **Unbounded WS chat text** → Groq translation cost/DoS. | Cap chat at 8 KB before the moderation/translation fan-out. |
| 🟠 Medium | **No supply-chain scanning.** | `.github/dependabot.yml` for cargo + npm + github-actions (weekly update PRs). |
| 🟡 Low | **`javascript:` scheme** allowed in legal-page markdown links. | Block dangerous schemes (`javascript:`/`data:`/`vbscript:`/`file:`) in the link renderer (relative + http(s)/mailto stay intact). |
| 🟡 Low | **TURN abuse** via the anonymous `/api/ice`. | coturn `total-quota`/`user-quota`/`max-bps` + RFC-1918 peer denylist (bounds the damage of a leaked credential) + the per-IP rate limit above. |

## Tracked follow-ups (not yet done)

| Sev | Issue | Where | Note |
|-----|-------|-------|------|
| 🟠 Med | **Glossary endpoints lack room-membership authz** — any logged-in user can read/overwrite/delete a room's glossary by code. | `api.rs glossary_{get,save,delete,import}` | Needs a "is this user in this room" check; data isn't sensitive + rooms are ephemeral, so deferred. |
| 🟠 Med | **Upload is membership-gated, not JWT-gated**, and unthrottled (Deepgram/Groq spend). 32 MiB is buffered in RAM before the size check. | `files.rs` | Add a per-room/IP rate limit + a `tokio::time::timeout` around PDF extraction. |
| 🟡 Low | **CSP is minimal** (`frame-ancestors/object-src/base-uri` only). | `vercel.json` | A full `script-src`/`connect-src` CSP needs testing on a Vercel preview so it doesn't break GSI/WS/Stripe. |
| 🟡 Low | **Stateless JWT**: 7-day lifetime, no revocation; a ban is enforced at WS join but not on the REST API. | `auth.rs`, `middleware.rs` | Add a `jti` denylist or shorten the lifetime if account-takeover risk rises. |
| 🟡 Low | **Rate limiter never evicts** (slow memory growth) and trusts the spoofable `X-Forwarded-For`. | `rate_limit.rs`, `auth.rs` | Switch to a TTL cache; use the trusted-proxy hop for the IP. |
| 🟡 Low | **Signed-URL TTL is 7 days** for chat-file links broadcast in plaintext. | `config.rs` | Shorten the default (it's env-tunable) — weigh against transcript links dying. |
| 🟡 Low | **No `cargo audit`/`npm audit` CI gate** (Dependabot added, but no blocking scan). | `.github/workflows/ci.yml` | Add a non-blocking audit job; the existing 3 npm highs are dev/build-time (esbuild/vite/astro), not in the shipped static output — fixing needs the Astro 6 major upgrade. |
| 🟡 Low | **Stripe webhook timestamp age not checked** (replay neutralized by idempotency, so impact nil). | `stripe_handler.rs` | Add a freshness window for defense-in-depth. |

## Security mini-guide (keeping it safe, for a non-expert owner)

- **Set `ALLOWED_ORIGINS`** on the server to your real domains (e.g.
  `https://voxtranslate.app`). Without it, CORS stays open (dev mode).
- **Rotate secrets** if a value ever leaks: `JWT_SECRET`, `ADMIN_API_SECRET`,
  `STRIPE_*`, `TURN_SECRET`, `DEEPGRAM_API_KEY`, `GROQ_API_KEY`, `RESEND_API_KEY`.
  They live only in the host's env — never commit them.
- **Watch Dependabot PRs** weekly; merge the security ones promptly. For a deep
  check run `cargo audit` (server) and `npm audit` (client) locally.
- **The golden rule for user content**: render any user/peer string with
  `textContent`, never `innerHTML`. The one XSS we found was exactly this slip.
- **Don't widen TURN/`/api/ice`**: keep coturn's quotas + peer-IP denylist so a
  leaked credential can't become an open relay.
- **Headers**: the security headers ship via `client/vercel.json`; if you add a
  feature that loads a new third-party script/connects to a new host, update the
  CSP/`Permissions-Policy` rather than removing them.
- **Admin**: treat `ADMIN_API_SECRET` like a root password; it's the only gate on
  `/api/admin/*` (ban, credits, GDPR delete).

## Scope notes

WebRTC **media is peer-to-peer** (the server never sees it). TURN relay credentials
are minted server-side and expire. This review covered the server, client, and
infra in the repo; it is not a substitute for a professional third-party pentest.
