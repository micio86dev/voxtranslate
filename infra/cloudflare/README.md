# Cloudflare WAF in front of the Railway origin (issue #111)

Puts the API/WebSocket origin (`voxtranslate-server-production.up.railway.app`)
behind the **Cloudflare WAF + DDoS** instead of exposing it directly, then **locks
the origin** so it only accepts traffic that came through Cloudflare. Today the
Vercel firewall protects only the static frontend; a botnet can flood
`wss://<origin>/ws` or `/rooms` directly. (Spec **0078**.)

> **What's already done in code** (ships dormant, safe to deploy now):
> - `origin_lock` middleware — rejects any request except `/health` that lacks the
>   `x-origin-verify` secret header, *only* when `CF_ORIGIN_SECRET` is set
>   (`server/src/lib.rs`).
> - Real client IP behind Cloudflare via `CLIENT_IP_HEADER=cf-connecting-ip`
>   (spec 0066, `server/src/observability.rs`).
> - CSP `connect-src` already allows `https://api.voxtranslate.app` +
>   `wss://api.voxtranslate.app` (`client/vercel.json`).
>
> The zone `voxtranslate.app` is **already on Cloudflare** (nameservers
> `*.ns.cloudflare.com`), so no DNS migration is needed.

All steps below are **owner-side** (Cloudflare + Railway + Vercel dashboards).
**Do them in order** — the order avoids 403-ing live traffic.

---

## 1. DNS — proxied `api` subdomain → Railway

Cloudflare → **DNS** → **Add record**:

| Field | Value |
|-------|-------|
| Type | `CNAME` |
| Name | `api` |
| Target | `voxtranslate-server-production.up.railway.app` |
| Proxy status | **Proxied** (orange cloud) ✅ |

This routes `api.voxtranslate.app` through Cloudflare to the Railway origin.
Cloudflare proxies **WebSockets** automatically (WS is on by default).

**Then register the custom domain on Railway** (required — otherwise Railway sees
the unknown `Host: api.voxtranslate.app` and returns `404 Application not found`,
because it routes by Host). Either:

```
railway domain api.voxtranslate.app --service voxtranslate-server
```

or Railway → `voxtranslate-server` → **Settings → Networking → Custom Domain** → add
`api.voxtranslate.app`. The DNS CNAME already points at the service's railway domain,
so Railway routes the Host to this service; the Cloudflare proxy can stay on.

**SSL/TLS** → set encryption mode to **Full (strict)**. Cloudflare connects to the
CNAME target (`…up.railway.app`), for which Railway serves a valid cert, so strict
validation passes.

## 2. WAF — managed rules, rate limiting, DDoS

Cloudflare → **Security / WAF**:

1. **Managed rules**: enable the **Cloudflare Managed Ruleset** (and OWASP core if
   on a plan that includes it). Start in *Log* for a day if you want to watch for
   false positives, then switch to *Block*.
2. **Rate limiting rules** (the real anti-flood layer, complements the in-app
   per-IP limits from spec 0064):
   - `/ws` connect: e.g. **60 requests / 1 min / IP** → *Block* (managed-challenge
     for browsers). Path: `(http.request.uri.path eq "/ws")`.
   - `/rooms` + `/metrics`: e.g. **120 / 1 min / IP** → *Block*.
   - `/api/*`: a looser ceiling, e.g. **300 / 1 min / IP**.
3. **DDoS**: L3/4 + HTTP DDoS protection is **automatic** on all plans — no config.
   Optionally raise sensitivity to *High* under attack.

## 3. Origin lock — only Cloudflare may reach the origin

So an attacker can't skip Cloudflare by hitting the Railway URL directly:

1. Generate a long random secret (e.g. `openssl rand -hex 32`).
2. Cloudflare → **Rules → Transform Rules → Modify Request Header** → **Add**:
   - *If*: all incoming requests (or hostname `api.voxtranslate.app`).
   - *Then* set static header: `x-origin-verify` = `<the secret>`.
3. **Do NOT set `CF_ORIGIN_SECRET` on Railway yet** — wait until step 5, after the
   client is cut over. Setting it now would 403 the live client (still on the
   Railway domain, no header).

> The app **always exempts `/health`** (Railway's platform healthcheck hits the
> origin directly, bypassing Cloudflare) so the lock can't break deploys.

## 4. Tell Railway to trust Cloudflare's client IP

Railway → `voxtranslate-server` → **Variables**: set

```
CLIENT_IP_HEADER=cf-connecting-ip
```

so per-IP limits + logs key on the real visitor (not Cloudflare's edge IP). Only
correct **once behind Cloudflare** (spec 0066). Redeploys automatically.

## 5. Cut the client over to `api.voxtranslate.app`, then arm the lock

**Order matters — follow exactly:**

1. **Vercel** → project env → set `PUBLIC_WS_HOST=api.voxtranslate.app` → redeploy
   the client. Now the browser talks to the API **through Cloudflare** (which
   injects `x-origin-verify`). CSP already allows this host.
2. Verify a real call works end-to-end on the new domain (join, subtitles, chat).
3. **Now** set `CF_ORIGIN_SECRET=<the same secret>` on Railway. Direct hits to the
   Railway URL (no header) now get **403**; Cloudflare traffic (with the header)
   passes. `/health` stays open.
4. Update the two **external callers** that hit the origin directly, or they'll 403:
   - **Stripe webhook** URL → `https://api.voxtranslate.app/api/billing/webhook`.
   - **Better Stack** monitors (`infra/betterstack/monitors.json`) → point `/health`
     + `/metrics` at `api.voxtranslate.app`. (`/health` is exempt anyway, but move
     both for consistency; `/metrics` is `METRICS_TOKEN`-gated.)

## 6. Verify

- `curl https://api.voxtranslate.app/health` → `ok` (through Cloudflare; `server: cloudflare`).
- `curl https://voxtranslate-server-production.up.railway.app/rooms` → **403** (direct, locked).
- `curl https://voxtranslate-server-production.up.railway.app/health` → `ok` (exempt).
- A real call behind symmetric NAT / 4G connects; WS upgrades through Cloudflare.

## Rollback

- Unset `CF_ORIGIN_SECRET` on Railway → origin accepts direct traffic again.
- Revert `PUBLIC_WS_HOST` on Vercel to the Railway host + redeploy.
- (DNS record / WAF rules can stay; they're harmless without the lock.)

## Notes

- Complements the in-app rate limits (spec 0064) — defence in depth.
- The `x-origin-verify` shared-secret approach is used because Railway doesn't
  expose Cloudflare **Authenticated Origin Pulls** (mTLS) at the platform level.
- Rotate `CF_ORIGIN_SECRET` + the Transform Rule together if it ever leaks.
