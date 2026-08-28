# Security

Two reviews inform this file: a defensive security review on **2026-06-12**, and an
adversarial red-team pass on **2026-08-28**
([full report](docs/security/2026-08-28-redteam-assessment.md)). Between them they
cover XSS/DOM injection, SQL injection, authn/authz (IDOR), multi-tenancy, the admin
and WebSocket surfaces, SSRF, Stripe, CORS, rate-limiting and cost abuse, security
headers, DoS limits, secret handling, file upload, infrastructure exposure, and
CI/supply-chain. This file records the findings, what was fixed (spec
[0028](specs/0028-security-hardening/spec.md), then the 2026-08-28 pass), what's
tracked, and how to keep the app secure.

Neither review certifies the application as secure, and no review can: a source
audit sees the code that exists, not the configuration deployed, not the runtime,
and not tomorrow's commit.

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

## Fixed in the 2026-08-28 red-team pass

Full report with exploit paths and evidence:
[`docs/security/2026-08-28-redteam-assessment.md`](docs/security/2026-08-28-redteam-assessment.md).

| Sev | Issue | Fix |
|-----|-------|-----|
| 🟠 High | **`translate_text` WS frame spent Groq money anonymously** — the Enhanced listener's translation hop reached the raw Groq client, so a guest could send it: unbilled, no tier check, and outside the admission semaphore that spec 0069 added to stop unbounded concurrent Groq calls. The generic WS guards (64 KiB/frame, 500 msg/5 s) bounded it, but one socket could still starve the Standard fan-out. | Entitlement gate (authenticated **and** on a client-direct engine), an 8 KB cap matching the sibling `Chat` arm, and `Translator::translate_uncached` — same uncached call, now admitted through the shared semaphore. |
| 🟠 High | **Anonymous Cartesia token minting** — `GET /api/w/{code}/tts-session` handed any caller a real, hour-long TTS credential usable directly against Cartesia. Codes are listed by `/api/webinars/public`, and the rate limit keyed on the *webinar*, not the caller, so one client could farm tokens forever *and* deny TTS to that webinar's real viewers. | A tighter per-IP limit ahead of the per-code backstop, plus the members-only gate below. |
| 🟠 Medium | **`members_only` was enforced in exactly one place** (the presence WS). The other five `{code}` endpoints ignored it, so a guest with the link still read the full transcript, read and posted chat, uploaded files, and minted TTS tokens. | One `require_member_access` helper on every content/participation endpoint, plus `webinar::coverage` tests that fail when a new `{code}` endpoint lands without it. `public_get` stays open by design — a guest needs the metadata to be shown the sign-in gate — and has its own inverse test. |
| 🟠 Medium | **TURN relay denylist covered RFC1918 only** — loopback, link-local (`169.254.169.254`, cloud metadata), CGNAT and all of IPv6 were relayable. `no-tcp-relay` closed the HTTP metadata path, but as a side effect of another setting. | Denylist extended (`infra/coturn/turnserver.conf`). The IPv4-mapped IPv6 range is deliberately left out with the reasoning in the file: it may describe ordinary IPv4 peers on a dual-stack listener, in which case denying it takes calls down. Test before adding. |
| 🟠 Medium | **No HSTS on `dashboard` or `website`** (the app had it), and the website CSP was three directives on a page loading GTM + Meta Pixel. | HSTS on both. Full website CSP staged **Report-Only** beside the enforcing original — same rollout the app's CSP got in #117. |
| 🟡 Low | **`cdn.jsdelivr.net` allow-listed wholesale** in `script-src` — jsDelivr serves every npm package, so an injection could run anything and still pass the CSP, in the origin holding the JWT. | Scoped to the MediaPipe package path. The URL is still unpinned to a version — pinning needs one verified against the live CDN, so it stays open below. |
| 🟡 Low | **A ban did not mean the same thing everywhere** — the webinar module hand-rolled four `verify_jwt` reads and skipped the ban check, so a banned org member kept host access until their token expired. | `webinar::authed_user` is the module's only permitted `verify_jwt` caller, enforced by a coverage test. |
| 🟡 Low | **Chat `avatar_url` accepted any `https://` URL** — a scheme check, not a trust check — then broadcast it, making every participant's browser beacon to whoever chose it. | Restricted to Google account pictures and our own Supabase storage, matched on a dot boundary. |
| 🟡 Low | **`CLIENT_IP_HEADER` trusted without the origin lock** made every per-IP limit forgeable from the direct origin. | Startup check logs an error on the mismatched pair (`ip_trust`). Not fatal — the server cannot tell an isolated origin from an exposed one. |

## Tracked follow-ups (not yet done)

| Sev | Issue | Where | Note |
|-----|-------|-------|------|
| 🟠 Med | **Glossary endpoints lack room-membership authz** — any logged-in user can read/overwrite/delete a room's glossary by code. | `api.rs glossary_{get,save,delete,import}` | Needs `user_id` on the room `Peer` (threaded from the WS join) to map an HTTP user to room membership; data isn't sensitive + rooms ephemeral, so deferred. |
| 🟡 Low | **Upload is membership-gated, not JWT-gated** (by design — same trust model as guest chat). | `files.rs` | ✅ Now throttled per-uploader + a 15 s PDF-extraction timeout (spec 0029); the membership-gating is intentional. |
| 🟡 Low | **CSP is minimal** (`frame-ancestors/object-src/base-uri` only). | `vercel.json` | ✅ Now a **full enforcing CSP** (`default-src 'self'` + per-directive allowlists for GSI/Supabase/Railway-WS/MediaPipe) after a source audit (spec 0075); staged via Report-Only since #117. Remaining tightening: drop `script-src 'unsafe-inline'` once Astro emits CSP nonces/hashes. |
| 🟡 Low | **Stateless JWT**: 7-day lifetime, no revocation. | `auth.rs`, `middleware.rs` | The ban half is ✅ **done**: enforced at the WS join, on the REST API via `AuthUser` (#117), at the extension token exchange, and — since 2026-08-28 — across the webinar module through `webinar::authed_user`, with a coverage test forbidding a bare `verify_jwt` there. **Revocation is still open**: add a `jti` denylist or shorten the lifetime if account-takeover risk rises. |
| 🟡 Low | **Rate limiter trusts the spoofable `X-Forwarded-For`** for the IP key. | `auth.rs`, `observability.rs` | ✅ **Done** (#117). `resolve_client_ip` takes the **last** XFF hop — the one the trusted proxy appends, which a client cannot forge — or a configured edge header (`CLIENT_IP_HEADER=cf-connecting-ip`). Eviction added in spec 0029. The 2026-08-28 audit added a startup check: that header is only unforgeable while the origin lock (`CF_ORIGIN_SECRET`) is also on, so a mismatched pair now logs an error at boot. |
| 🟡 Low | **Signed-URL TTL is 7 days** for chat-file links broadcast in plaintext. | `config.rs` | Shorten the default (it's env-tunable) — weigh against transcript links dying. |
| 🟠 Med | **Astro 5.18.2 on `dashboard` + `website`** — 8 advisories (XSS, Host-header SSRF), fix available; `client` is already on 7.1.3. | `dashboard/`, `website/` | Not currently exploitable: both build `output: 'static'`, have no server islands, and their `define:vars` values are build-time constants. The drift is the risk — one `output: 'server'` away from live. Upgrade 5 → 7 on both, with a build + visual check. |
| 🟡 Low | **MediaPipe loaded from an unpinned jsDelivr URL** — `@mediapipe/selfie_segmentation` with no version, executed in the app origin. | `client/src/scripts/virtual-background.ts` | CSP is now scoped to that package path, but a compromised or merely newer upstream still runs. Pin a version verified against the live CDN, and add SRI to the loader script. |
| 🟡 Low | **`esc()` is copy-pasted into ~10 dashboard files.** | `dashboard/src/pages/**` | All ten copies are byte-identical and correct today. The risk is structural: the eleventh is the one that gets it wrong, and nothing would catch it. Export one from `src/lib/`. |
| 🟡 Low | **`members_only` is a sign-in check, not an org-membership check.** | `webinar/mod.rs` | Matches the presence WS's original behaviour and the flag's UI wording, so this is current-intent, not a gap — but the name invites the stronger reading. Decide which one it should mean before a customer assumes the other. |
| 🟠 Med | **A *blocking* dep-audit gate** (vs the informational one). | `.github/workflows/ci.yml` | Dependabot (0028) + a non-blocking `cargo audit`/`npm audit` job (0029). `cargo audit` is **clean** (0 vulns / 668 crates), so the Rust side could block today. The npm side cannot: `client` is on Astro 7.1.3, but `dashboard` and `website` still sit on **5.18.2**, where astro is a *production* dependency carrying 8 advisories with a fix available. The earlier note here called these "dev-only" — that was wrong. Reachability is low (both build `output: 'static'` with build-time-constant `define:vars`), which is why it is Med and not High. |
| 🟡 Low | **Stripe webhook timestamp age not checked** (replay neutralized by idempotency, so impact nil). | `stripe_handler.rs` | ✅ **Done.** A signed-timestamp tolerance is enforced before the HMAC check (`stripe_handler.rs`), matching Stripe's own default window. |

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
