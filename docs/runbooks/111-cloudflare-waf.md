# Cloudflare WAF / anti-DDoS in front of the Railway origin (#111)

**Why.** The Railway origin is reachable directly, with no L3/4 DDoS absorption and
no edge WAF. App-level rate limiting (spec 0064: global WS-connection cap, per-IP
connect/`/rooms`/`/metrics` throttles, per-socket frame-size + message-rate caps)
bounds a *single* abusive socket but **cannot absorb a volumetric network flood** —
that needs a platform in front. Cloudflare's free tier gives you a WAF, managed
rulesets, rate-limiting, and Attack Mode.

**App side is ready.** Once Cloudflare is in front, set `CLIENT_IP_HEADER=cf-connecting-ip`
on the server so per-IP limits + logs key on the *real* client IP (see step 4) —
this is wired in spec 0066. Nothing else in the code changes.

---

## 1. Put the apex/api hostname behind Cloudflare (proxied DNS)

1. Add the site `voxtranslate.app` to Cloudflare (free plan) and point the
   registrar's nameservers at Cloudflare.
2. The **client** is on Vercel and the **server** on Railway. Only the **server**
   hostname needs the WAF (the client already sits behind Vercel's edge). Create a
   proxied (`🟧 orange-cloud`) DNS record for the API host, e.g.
   `api.voxtranslate.app → CNAME → voxtranslate-server-production.up.railway.app`.
3. Point the client's `PUBLIC_*` API base + `ALLOWED_ORIGINS` at the new
   `api.voxtranslate.app` host (keep the existing Railway host in `ALLOWED_ORIGINS`
   during cutover). SSL/TLS mode: **Full (strict)**.

> WebSockets: Cloudflare proxies WS by default on all plans — no extra setting. The
> `/ws` upgrade passes straight through.

## 2. Enable the WAF

- **Managed Rules** → turn on the *Cloudflare Managed Ruleset* (and *OWASP Core* if
  desired, in log-only first to catch false positives).
- **Bots** → enable the free *Bot Fight Mode*.
- **DDoS** → HTTP DDoS protection is on by default; leave the sensitivity at default.

## 3. Rate limiting (edge — complements the app caps)

Add a **Rate limiting rule** (free tier allows one):
- Match: `http.request.uri.path eq "/rooms"` (and/or the WS upgrade path) — adjust
  to your highest-volume public GETs.
- Threshold: e.g. **60 requests / 1 min per IP** → *Block* for 1 min. This mirrors
  the app's `HTTP_PUBLIC_MAX_PER_MIN` (60) so the edge sheds the load *before* it
  reaches the origin. Keep the app cap as defense-in-depth.

Keep **Attack Mode** (`Under Attack`) one click away for incident response; it
challenges every visitor and is the fastest volumetric-flood mitigation.

## 4. Make the origin key on the real client IP

With Cloudflare proxying, the request chain is `client → Cloudflare → Railway → app`,
so the origin's *last* `X-Forwarded-For` hop becomes **Cloudflare's** IP — which
would make every visitor share one per-IP key. Cloudflare sets `CF-Connecting-IP`
to the true client IP (and overwrites any client-supplied value, so it can't be
forged *while behind Cloudflare*). Set on the Railway **server** service:

```
CLIENT_IP_HEADER = cf-connecting-ip
```

The server then prefers that header for per-IP limits and logs (spec 0066). **Only
set this once Cloudflare is actually in front** — if the origin is reachable
directly, a client could forge `CF-Connecting-IP`. Leave it unset until cutover.

## 5. Lock the origin to Cloudflare (close the bypass)

A WAF only helps if attackers can't hit Railway directly. Options (best-effort on a
PaaS):
- **Shared-secret header.** In Cloudflare → *Rules → Transform/Managed Headers*, add
  a secret header (e.g. `X-Origin-Auth: <random>`) and have the app reject requests
  without it. (Small follow-up if you want it enforced in code — track separately.)
- **Cloudflare IP allowlist** at the origin if Railway ever exposes an IP filter.
- At minimum, stop advertising the raw `*.up.railway.app` host (use the
  `api.voxtranslate.app` host everywhere and drop the Railway host from
  `ALLOWED_ORIGINS` after cutover).

## 6. Verify

- `curl -I https://api.voxtranslate.app/health` → `200`, response headers include
  `cf-ray` / `server: cloudflare`.
- Open a call end-to-end (WS connects, subtitles flow).
- Server logs (Better Stack, #109) show real client IPs (not a single Cloudflare IP)
  once `CLIENT_IP_HEADER` is set.
- Trip a rate limit from one IP → Cloudflare `429`/block before the origin.

## Rollback

Set the DNS record back to **DNS-only** (grey cloud) to bypass Cloudflare instantly,
and **unset `CLIENT_IP_HEADER`** on the server (so it reverts to the unforgeable
last-`X-Forwarded-For`-hop source).
