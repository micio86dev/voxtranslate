# China / Great-Firewall TURN profile — runbook

The default `GET /api/ice` returns **Cloudflare Realtime TURN** (great everywhere except
mainland China: anycast on ports 3478/5349 — **no `:443`** — and Cloudflare's edge is
throttled behind the Great Firewall). For participants inside China we serve a **separate
restricted profile**: a `turns://…:443` **TLS-on-443** relay that looks like ordinary
HTTPS and survives the DPI that resets plain TURN/UDP. See specs
[0026](../../specs/0026-turn-relay/spec.md) and the corridor PRs **#331** (media path) and
**#332** (Enhanced tier gate).

**How it activates (all additive — non-China behavior is unchanged):**
- The browser runs a reachability probe (`client/src/scripts/restricted-net.ts`). If a
  known-GFW-blocked canary **and** Cartesia are both unreachable, it calls
  `GET /api/ice?restricted=1` and forces `iceTransportPolicy: 'relay'`.
- The server returns the restricted profile **only** when `?restricted=1` **and**
  `TURN_TLS_*` is configured; otherwise it falls back to the default relay. So until you
  do step 1 below, `?restricted=1` is a safe no-op.

## 1. Configure the relay (Railway → `server`)

The restricted profile reads its own vars, **separate** from the default `TURN_*` block
(whose Cloudflare mode has no `:443` endpoint). Credential precedence: `TURN_TLS_SECRET`
(coturn HMAC) → `TURN_TLS_USERNAME` + `TURN_TLS_PASSWORD` (managed relay).

**Managed Asia PoP (chosen approach — reuse the existing Metered account):**

```
TURN_TLS_URLS=turns:global.relay.metered.ca:443?transport=tcp
TURN_TLS_USERNAME=<same value as the existing TURN_USERNAME>
TURN_TLS_PASSWORD=<same value as the existing TURN_PASSWORD>
```

The Metered account is already in the `server` service (as the — currently
Cloudflare-shadowed — `TURN_URLS`/`TURN_USERNAME`/`TURN_PASSWORD`). Reusing it here needs
**no new vendor**. The `turns:…:443?transport=tcp` endpoint is validated below.

> Metered's `global.relay.metered.ca` uses **Azure Traffic Manager** geo-routing, so the
> PoP a China user reaches is not guaranteed. If step 3 (from a China vantage) fails,
> switch `TURN_TLS_URLS` to a provider with a **pinnable HK/SG PoP**, or self-host coturn
> on an HK/SG VPS (see [`../coturn/`](../coturn/README.md)) with `turns:…:443?transport=tcp`.

## 2. Deploy

Merge **#331** first (adds the `TURN_TLS_*` restricted profile + `/api/ice?restricted=1`),
then **#332** (Enhanced tier gate). Railway auto-deploys `server` on green `main`. No
client env change — the probe + relay are automatic.

## 3. Validate

`infra/turn/validate-turn.mjs` is dependency-free (Node built-ins only) so it runs
anywhere, **including a mainland-China VPS** — the only way to confirm GFW survival.

```
TURN_HOST=global.relay.metered.ca TURN_PORT=443 \
TURN_USER='<TURN_TLS_USERNAME>' TURN_PASS='<TURN_TLS_PASSWORD>' \
node infra/turn/validate-turn.mjs
```

It performs a real **TURN Allocate** over TLS-on-443 (proving the relay + creds allocate a
relayed transport address), then checks whether `api.voxtranslate.app` is reachable and
whether `/api/ice?restricted=1` already serves a `turns:443` relay.

- **From a non-China vantage (done during development):** ✓ TLS on :443, ✓ Allocate
  success, ✓ API reachable. Before step 1/2 it reports `?restricted=1` does *not* yet
  serve `turns:443` (expected).
- **From a mainland-China vantage (must be run before relying on this):** the same run
  must still reach `✓ ALLOCATION SUCCESS`. If instead `api.voxtranslate.app NOT reachable`
  — that's the real blocker (see below), not the relay.

## 4. Open assumption — app reachability from China

If `api.voxtranslate.app` (Railway) is itself GFW-blocked without a VPN, WebRTC signaling
and the STT WebSocket never connect and **all of the above is moot**. This needs a real
China-side test (`curl -I https://api.voxtranslate.app/api/ice` from a China VPS, or the
validator's API check). If blocked, the fix is China-reachable ingress (Cloudflare proxy
on the API domain / a China CDN / ICP filing) — a larger track outside these PRs.

## Forward look

A mesh peer in China opens N−1 independent GFW-crossing connections (3 in a 4-way call).
For the Business layer, an **SFU in HK/SG** consolidates those into a single China-side
connection — architecturally far more robust for this corridor. Recommendation only.
