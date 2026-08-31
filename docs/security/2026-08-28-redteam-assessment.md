# Security assessment — VoxTranslate

**Date**: 2026-08-28 · **Commit**: `2b51ddcd` (branch `develop`, server v1.40.2)
**Method**: static source audit + read-only local checks (`cargo audit`, `npm audit`, git-history secret scan)
**Scope**: `server/` (Rust, 52 619 LOC, 155 routes), `client/`, `dashboard/`, `website/`,
`voxtranslate-chrome-extension/`, `infra/`
**Out of scope**: no live production or staging traffic was generated (owner asleep, no
current authorization for a dynamic test against a live host). Deployed runtime
configuration, Railway/Cloudflare/Vercel settings, and third-party service posture were
**not** verified.

---

## Verdict

**No, you are not "100 % secure" — nobody is, and no assessment can produce that
claim.** What I can say is this: the codebase is in the top tier of what I see. Every
query is parameterized, every multi-tenant query is `WHERE id = $1 AND org_id = $2`,
JWT is pinned to HS256, the admin secret is compared in constant time before the body is
parsed, the Stripe webhook now checks signature *and* timestamp freshness, the client-IP
resolver takes the unforgeable last proxy hop, the markdown renderers escape before they
tag, and `cargo audit` reports **zero** vulnerabilities across 668 crates. The
2026-06-12 pass did real work and it held.

**The thing to fix first is one message type.** `ClientMessage::TranslateText` on the
main `/ws` lets an *anonymous* peer drive Groq translation calls that are never billed,
never tier-checked, and — the part that matters — **deliberately bypass the concurrency
semaphore** that protects every other translation path. The generic WS defences do apply
(64 KiB per frame, 100 msg/s per connection, socket closed on breach), so this is bounded
at roughly 100 unbilled calls/second per socket rather than unlimited. Still the only
place in the system where third-party money is spent with no user attached, and the only
one that can saturate Groq for paying users. Fix it first; everything else can wait for a
normal sprint.

The pattern behind the two top findings is the same and worth naming: **guards that were
designed once and not carried forward to the path added next.** `Chat` got a size cap;
`TranslateText`, added later for the same Groq sink, did not. The authenticated Enhanced
session got a credit gate; the anonymous webinar TTS mint did not. That is a review
habit, not a code defect — and it is the thing to change.

---

## Findings

### [HIGH] 1. Anonymous, unmetered Groq spend via the `translate_text` WS frame, bypassing the concurrency semaphore

> **Correction (2026-08-28, second pass).** The first version of this finding claimed the
> frame had "no size cap" and that "no per-message rate limit exists anywhere in the WS
> loop". **Both were wrong.** `MAX_FRAME_BYTES` (64 KiB) is enforced on every inbound
> frame at `lib.rs:1718`, and `WS_MSG_MAX`/`WS_MSG_WINDOW` (500 msg / 5 s ≈ 100 msg/s)
> is enforced per connection at `lib.rs:1721-1729` and **closes** an abusive socket. The
> original grep searched for `rate_limiter.allow` and missed a per-connection counter
> that uses neither. The finding stands on its remaining legs — no metering, no tier
> check, and a deliberate semaphore bypass — but it is **bounded**, and the severity
> below is restated accordingly.

- **Status**: CONFIRMED (static — full call path traced end to end)
- **Where**: `server/src/lib.rs:2231-2260`
- **Reachable by**: **anonymous** — `/ws` accepts guests (`authorize` returns `Option`, `lib.rs:1405-1413`)
- **Ceiling**: ~100 Groq calls/sec per connection, ≤64 KiB of text each

**Exploit path**

1. Open `wss://api.voxtranslate.app/ws?room=x&lang=en` with **no token**. Guests are
   admitted by design; `billed_user` is `None`.
2. Send, in a loop:
   `{"type":"translate_text","request_id":"1","text":"<very large string>","source":"en","target":"it"}`
3. Each frame reaches `lib.rs:2247` → `groq.translate(...)`.

**Why every existing control misses it**

| Control | Applies to `Chat` | Applies to `TranslateText` |
|---|---|---|
| 64 KiB frame cap (`lib.rs:1718`) | yes | **yes** — applies to all inbound frames |
| 500 msg / 5 s per-connection budget (`lib.rs:1721`) | yes | **yes** — closes the socket on breach |
| 8 KB **semantic** size cap (`lib.rs:2003`) | yes | **no** — 8× more text per call reaches Groq |
| Credit / usage metering | yes (`handle_chat` carries `speaker_user_id`) | **no** — nothing is billed |
| Tier check (Enhanced-only feature) | n/a | **no** — any peer, including a guest, can send it |
| `translator.rs` concurrency semaphore | yes | **no — bypassed on purpose** (`lib.rs:2242-2244`: "call the raw Groq client here rather than the cached `translator`") |
| Dragonfly translation cache | yes | **no** — every call is a cache miss by construction |
| Bounded in-flight concurrency | via the semaphore | **no** — `tokio::spawn` per frame, nothing awaits a permit |

The `groq.chat` client also **retries on 429 with exponential backoff**
(`groq.rs:110-124`), so hitting Groq's rate limit amplifies the spend rather than
stopping it.

The per-IP `wsconnect` throttle (`lib.rs:1048`) limits **connections per minute**, not
messages on an established connection — one socket sustains the full 100 msg/s.

**Impact**, with the real ceiling: one anonymous socket sustains ~100 unbilled,
uncached Groq calls per second — ~360 000 per hour — each carrying up to 64 KiB. That is
material spend on its own. The sharper problem is the **semaphore bypass**: those calls
go out as unbounded concurrent HTTP requests, defeating the exact control that exists to
stop Groq saturation, so an abuser degrades or stalls translation for every paying user
on the Standard tier. Not a catastrophic-loss bug; a real cost-and-availability bug on
an anonymous path.

**Fix** — in the `TranslateText` arm, mirror what `Chat` already does:

In priority order — the first two are the finding, the rest are hygiene:

1. **Route it through the `translator` semaphore**, or give it its own. This is the
   important one: an unpermitted `tokio::spawn` on a network-triggered path is what lets
   one socket saturate Groq for everybody.
2. **Require `billed_user.is_some()`** and that the peer is actually on the Enhanced
   tier — this frame exists only for Enhanced listeners, so a guest sending it is already
   a protocol violation.
3. Cap `text` at 8 KB, matching the chat precedent (the 64 KiB frame cap is a transport
   guard, not a semantic one).
4. Meter it like every other paid translation.

---

### [HIGH] 2. Anonymous Cartesia TTS-token minting on the public webinar endpoint

- **Status**: CONFIRMED
- **Where**: `server/src/webinar/routes.rs:2173-2211`, minting at `server/src/api.rs:147-182`
- **Reachable by**: **anonymous**

**Exploit path**

1. `GET /api/webinars/public` — auth-free, returns up to 100 live/scheduled webinars with
   `code` **and** `tier` (`webinar/routes.rs:355-367`). Enhanced ones are self-identifying.
2. `GET /api/w/{code}/tts-session` — no auth, no credit gate.
3. Receive a real Cartesia access token, `expires_in: 3600`, grant `{"tts": true}`.
4. Use it **directly against `api.cartesia.ai`** for an hour, for anything — it is not
   bound to the webinar, the viewer, or a character budget.
5. Refresh at 20/min (the only limit) — ~28 800 tokens/day per code.

**Contrast**: the authenticated sibling `POST /api/sessions/enhanced/session`
(`api.rs:190`) is, in its own docstring, "Auth-gated (guests, pinned to Standard, get
401), credit-gated, and rate-limited." The public path has none of the three.

**Second defect in the same handler**: the rate-limit key is `wbr-tts:{code}` — **per
webinar, not per IP**. One attacker consuming the 20/min bucket denies TTS to every
legitimate viewer of that live webinar. Every other anonymous endpoint in the codebase
keys on `client_ip` (see `webinar/files.rs:36-43`), so this is an oversight, not a design
choice.

**Impact**: unbounded Cartesia spend + a trivial DoS on a live webinar's translated audio.

**Fix**: add a per-IP limit alongside the per-code one; charge the host org's credits for
minting (the webinar already deducts credits elsewhere — `webinar/routes.rs:882`); and
consider binding the token to a much shorter TTL with client-side refresh.

---

### [MEDIUM] 3. `members_only` is enforced on exactly one of six code-addressed endpoints

- **Status**: CONFIRMED
- **Where**: enforced **only** at `server/src/webinar/presence.rs:422-445`
- **Reachable by**: anonymous, with the join link

`rg -n 'members_only' server/src` returns enforcement in **one** place. Every other
public endpoint under `/api/w/{code}` ignores the flag:

| Endpoint | Handler | `members_only` respected |
|---|---|---|
| `GET /api/w/{code}/presence` | `presence.rs:375` | **yes** |
| `GET /api/w/{code}` | `routes.rs:2062` | no |
| `GET /api/w/{code}/transcript` | `routes.rs:1018` | **no — full spoken transcript** |
| `GET /api/w/{code}/chat` | `chat.rs list_chat` | no |
| `POST /api/w/{code}/chat` | `chat.rs:122` | no |
| `POST /api/w/{code}/files` | `files.rs:22` | no |
| `GET /api/w/{code}/tts-session` | `routes.rs:2173` | no |

**Impact**: a `members_only` webinar blocks an unauthenticated guest from the *live
presence stream* and nothing else. With the link alone they can read everything that was
said, read and post to the chat, and upload files. The feature does not do what its name
and the UI promise — which is worse than not having it, because the host believes the
content is gated.

**Fix**: one shared guard (`require_webinar_access(&w, &headers) -> Result<(), Response>`)
called at the top of every `{code}` handler, so the next endpoint added inherits it
instead of forgetting it.

---

### [MEDIUM] 4. TURN relay denylist covers only RFC1918 — loopback, link-local and all IPv6 are open

- **Status**: CONFIRMED
- **Where**: `infra/coturn/turnserver.conf:44-49`

The comment reads "Don't relay to private/internal ranges (SSRF protection)", but the
list is only `10/8`, `172.16/12`, `192.168/16`. Missing:

- `127.0.0.0/8` — services on the TURN host itself
- `169.254.0.0/16` — **cloud metadata (169.254.169.254)**
- `100.64.0.0/10` — CGNAT / some provider internals
- **all IPv6**: `::1`, `fc00::/7`, `fe80::/10`, `::ffff:0:0/96` (IPv4-mapped, which can
  re-enter the ranges above)

Anyone can obtain a credential from the anonymous `GET /api/ice` (`api.rs:430`) and
allocate a relay.

**What limits it today**: `no-tcp-relay` is set, which closes the classic HTTP
metadata-service path, and `total-quota=200` / `user-quota=6` / `max-bps` bound volume.
That is why this is MEDIUM and not HIGH — but the protection is incidental, and removing
`no-tcp-relay` for connectivity (a normal thing to try) would silently open it.

**Fix** — append:

```
denied-peer-ip=0.0.0.0-0.255.255.255
denied-peer-ip=127.0.0.0-127.255.255.255
denied-peer-ip=169.254.0.0-169.254.255.255
denied-peer-ip=100.64.0.0-100.127.255.255
denied-peer-ip=::1
denied-peer-ip=fc00::-fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff
denied-peer-ip=fe80::-febf:ffff:ffff:ffff:ffff:ffff:ffff:ffff
```

---

### [MEDIUM] 5. Astro version drift: `dashboard` and `website` two majors behind, 8 advisories

- **Status**: CONFIRMED (advisories) / reachability assessed as LOW
- **Where**: `dashboard/package.json`, `website/package.json`

| Package | Astro | `npm audit --omit=dev` |
|---|---|---|
| `client` | **7.1.3** | astro clean |
| `dashboard` | **5.18.2** | 6 high — astro is a **direct production** dep, `fixAvailable: true` |
| `website` | **5.18.2** | same family |

Advisories include XSS in `define:vars`, XSS via spread attribute names, XSS on
`transition:*` directives, reflected XSS via slot names, server-island parameter replay,
and Host-header SSRF in the prerendered error page.

**Honest reachability**: both build `output: 'static'` (`astro.config.*:19` / `:22`).
There are no server islands, no spread props in `dashboard`, and the three `define:vars`
sites in `website` pass build-time constants (locale lists, analytics IDs) — nothing
attacker-controlled. **I could not construct a working exploit against either site as
currently configured.** The finding is the drift itself: you are one `output: 'server'`
away from these becoming live, and the fix is available today.

**Note**: `SECURITY.md` currently says these are "3 dev-only npm highs
(esbuild/vite/astro) until the Astro 6 major upgrade". That is now stale on both counts —
`client` already runs Astro 7, and in `dashboard` astro is a **production** dependency.

**Also outstanding, unfixable from your side**: `client` pulls `sharp` (libvips CVEs) and
`@huggingface/transformers`/`kokoro-js` with `fixAvailable: false`. Build-time only, but
worth a note in the file so it is a decision rather than a surprise.

---

### [MEDIUM] 6. No HSTS on `dashboard` and `website`

- **Status**: CONFIRMED
- **Where**: `dashboard/public/_headers`, `website/public/_headers`

`client/vercel.json:12` sets
`Strict-Transport-Security: max-age=63072000; includeSubDomains; preload`. Neither
`_headers` file has it, so the two Cloudflare Pages surfaces are exposed to SSL-strip on
a first, untrusted-network visit — including the **dashboard**, which is the
highest-value target in the system (org data, member management, billing).

`website`'s CSP is also minimal — `object-src`, `base-uri`, `frame-ancestors` only, with
no `default-src` or `script-src`, while the page loads Facebook Pixel and GA.

**Fix**: add the same HSTS line to both `_headers`; give `website` a `default-src`/
`script-src` baseline matching `dashboard`'s.

---

### [LOW] 7. `cdn.jsdelivr.net` in the client `script-src` is an effective CSP bypass

- **Where**: `client/vercel.json:23`

jsDelivr serves **arbitrary** npm and GitHub packages. Allow-listing it means any HTML
injection can load any script it wants and still satisfy the CSP. Combined with the
already-tracked `'unsafe-inline'`, `script-src` currently provides little real
protection.

**Fix**: pin the specific jsDelivr path you need via SRI and a narrower host, or
self-host the asset. Then the `'unsafe-inline'` → nonces work (already tracked in
`SECURITY.md`) actually buys something.

---

### [LOW] 8. Per-IP rate limiting depends on two env vars being set *together*

- **Status**: PLAUSIBLE — needs a production config check I could not perform
- **Where**: `server/src/observability.rs:64-106`, `server/src/lib.rs:806-830`

`resolve_client_ip` prefers `CLIENT_IP_HEADER` (intended: `cf-connecting-ip`) and
otherwise correctly takes the **last** `x-forwarded-for` hop. Cloudflare overwrites
`cf-connecting-ip`, so it is unforgeable *while proxied*.

The coupling: if `CLIENT_IP_HEADER=cf-connecting-ip` is set but `CF_ORIGIN_SECRET`
(the origin lock, `lib.rs:806`) is **not**, then anyone who finds the direct Railway
origin URL can send a forged `cf-connecting-ip` and **bypass every per-IP rate limit in
the application** — `/api/ice`, the extension-code throttle, WS connects, uploads.
`/health` and `/version` are deliberately exempt from the origin lock, which makes the
direct origin easy to confirm.

**Action**: verify in Railway that both are set in production and staging, or neither.
Worth a boot-time assertion — refuse to start when `CLIENT_IP_HEADER` is set without
`CF_ORIGIN_SECRET`, so the unsafe combination cannot be deployed by accident.

---

### [LOW] 9. Ban enforcement is inconsistent across WebSocket entry points

`is_banned` is checked at the `/ws` join (`lib.rs:1162`), at the extension token exchange
(`extension.rs:240`), and on every REST request via the `AuthUser` extractor
(`middleware.rs:41`). It is **not** checked where the code calls `verify_jwt` directly:

- `webinar/stt.rs:164` — a banned org member can still open the host STT ingest
- `webinar/presence.rs` host verification — same

Low impact (org membership is still required), but a ban should mean one thing
everywhere. **Fix**: fold the ban lookup into a shared `verify_session_jwt` helper.

---

### [LOW] 10. Chat `avatar_url` accepted on an `https://` prefix alone

- **Where**: `server/src/webinar/chat.rs:147-153`

Any `https://` URL is stored and broadcast to every viewer, who then load it as an image
— handing an arbitrary third party the IP, User-Agent and timing of every participant.
**Fix**: restrict to your own storage/CDN hosts, or proxy it.

---

### [LOW] 11. `esc()` is copy-pasted into ~10 dashboard files

Ten byte-identical private copies of the XSS-escaping helper (`insights.astro:126`,
`teams.astro:46`, `history.astro:80`, `search.astro:47`, `webinars.astro:106`,
`activity.astro:51`, `meetings.astro:136`, `members/analytics.astro:68`,
`history/detail.astro:58`, `projects/detail.astro`). All correct **today**. Not a
vulnerability — a structural risk: the eleventh copy is the one that gets it wrong, and
nothing will catch it. **Fix**: one export in `src/lib/`.

---

## Checked and clean

| Class | Evidence |
|---|---|
| **SQL injection** | All queries parameterized. The 12 `format!`-built queries interpolate only compile-time constants (`PRUNE_AFTER`, `SELECT_COLS`) or a `table: &str` whose only two call sites pass string literals (`ai/jobs.rs:275,283`). No user data reaches SQL text. |
| **Multi-tenancy / IDOR** | Every nested business resource uses `WHERE id = $1 AND org_id = $2` **and** `require_role` first (`business/projects.rs:100-175` verified line by line, bind order correct). Every authed business handler references `require_role`/`require_call_role`/`user.user_id` — zero exceptions across `business/`. |
| **Privilege escalation** | `change_role` requires `OWNER` and excludes `role <> 'owner'` from the UPDATE, so ownership cannot be seized (`business/members.rs:354-380`). Audited to `admin_audit`. |
| **Command injection / eval** | No `Command::new`, no `std::process` outside two `exit(1)` calls, no `eval`/`new Function` in shipped client code. |
| **XSS — app client** | Every `innerHTML` carries either a static icon, `''`, or escaped content. `mdToHtml` (`client/src/scripts/report-md.ts`) escapes `& < >` **before** emitting any tag and supports no links or raw HTML. |
| **XSS — dashboard** | All 40+ `innerHTML` sites interpolate through `esc()` (escapes `& < > "`); unescaped interpolations are server-generated UUIDs only. `renderMarkdown` (`insights.astro:134`) is escape-first. |
| **Authentication** | HS256 pinned, signature + expiry verified, Google `aud` checked. Extension pairing is proper PKCE: S256 enforced, challenge shape validated (43 chars, base64url), `kind` claim prevents presenting a session token as a code, verifier length bounded per RFC 7636, per-IP throttled. |
| **Admin surface** | Constant-time secret compare in a `FromRequestParts` extractor that runs **before** body deserialization; every action writes an `admin_audit` row with the unspoofable source IP. |
| **Stripe webhook** | HMAC-SHA256 verified before the event is trusted, constant-time; crediting idempotent on `event_id`; **timestamp freshness now enforced** (`stripe_handler.rs:372-421`) — this was an open item in `SECURITY.md` and is closed. |
| **File upload** | Extension allow-list, server-derived content-type (client value ignored), UUID object key (user filename never reaches the path), size cap, `DefaultBodyLimit::max(8 MiB)`, per-IP **and** per-webinar rate limits. |
| **WS identity spoofing** | `from` is stamped from the server-side connection id; `relay_to_peer` searches only `room.peers` for the connection's own room — no cross-room relay. `?engine=premium` is overridden server-side for guests (`lib.rs:1423`). |
| **Webinar STT ingest** | JWT verified, org membership required (cross-tenant → 404), lifecycle state checked, and a single-flight guard acquired **before** the upgrade so reconnect races can't multiply Qwen+Groq cost (`webinar/stt.rs:153-203`). |
| **Assistant WebSockets** | `voice_assistant` and `help_assistant` both call `resolve_ws_auth` + `require_role`, and both sit behind process-wide semaphores. They ship dark (404) when unconfigured. |
| **"Talk to Anyone"** | JWT required (no guest tier), metered, credit-exhaustion handled, and explicitly guards against re-sending `start` to get an unmetered session (`talk/mod.rs:409-419`). |
| **Join-code entropy** | 58-symbol alphabet × 10 chars ≈ 58 bits, from `rand::rng()` (CSPRNG), with collision retry. Not enumerable. |
| **Secrets** | No `.env` tracked (only `.env.example`). Git-history scan for `sk_live_`, `AIza`, `gsk_`, `xoxb-`, `ghp_`, `BEGIN PRIVATE KEY`, `DASHSCOPE_API_KEY=sk-` across **all** refs: every hit is a placeholder or documentation. **Nothing leaked.** |
| **Rust supply chain** | `cargo audit`: **0 vulnerabilities** across 668 crates. Three advisories only — `rustybuzz` unmaintained, `event-listener` unsound, `chacha20` yanked. |
| **SSRF (application)** | No endpoint fetches a user-supplied URL. All outbound hosts are config-derived. |
| **CORS** | Explicit origin allow-list built from config; `permissive()` only when unset (dev). `allow_credentials(true)` is safe here because the list is never `*`. |
| **Origin lock** | `CF_ORIGIN_SECRET` header check is the outermost layer, constant-time, with a documented and correct exemption list. |
| **DoS bounds** | Global WS connection ceiling (2 000) + per-IP connect throttle + **per-connection 64 KiB frame cap and 500 msg / 5 s budget that closes abusive sockets** (`lib.rs:1706-1729`); per-language audio buffers drop rather than back-pressure; PDF extraction timeout; `lopdf` pinned past RUSTSEC-2026-0187. |
| **Infra** | `docker-compose.yml` exposes no database or admin port, no `privileged`, no docker socket mount. coturn: `no-cli`, `fingerprint`, `no-tcp-relay`, quotas, `stale-nonce`. |

---

## Not covered

- **Runtime configuration** — which env vars are actually set in Railway/Vercel/Cloudflare
  production and staging. Finding 8 in particular cannot be resolved from source.
- **Live dynamic testing** — no request was sent to production or staging. Every finding
  is traced statically; the two HIGHs have complete call paths but were not fired.
- **Third-party posture** — Stripe, Groq, Alibaba Model Studio, Cartesia, Supabase,
  Deepgram account security, key scoping, and dashboard 2FA.
- **Authenticated business logic under concurrency** — credit spend and seat counts were
  read, not race-tested. `deduct_org_credits_tx` uses a transaction, which is the right
  shape, but only a concurrent test proves it.
- **The Chrome extension's own runtime** (permissions model, content-script isolation)
  beyond its server-side pairing flow.
- **Social, phishing, and physical vectors.**

---

## Residual risk after every fix above

Stateless 7-day JWTs with no revocation list remain the standing exposure: a stolen token
is valid until it expires, and the token lives in `localStorage` where any successful XSS
can read it. The CSP that would blunt that still carries `'unsafe-inline'`. Those three
facts compound — they are individually tracked and collectively the thing most worth
fixing next quarter.

Beyond that: this is a large, fast-moving surface (155 routes, five deploy targets, four
AI vendors). The realistic risk is not a flaw that exists today but the next expensive
path shipped without the guard the last one got — which is exactly what findings 1 and 2
already are. A checklist on the PR template for any handler that spends money or accepts
anonymous input would catch more than another audit will.
