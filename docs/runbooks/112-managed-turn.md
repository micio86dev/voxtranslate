# Managed geo-distributed TURN relay (#112)

**Why.** WebRTC is peer-to-peer, but a *direct* connection fails when both peers sit
behind restrictive NATs (symmetric NAT, CGNAT — common on mobile and **cross-border**
calls). STUN alone can't fix it; you need a **TURN relay** or the call goes to
`failed` and the peer tile drops (the "couldn't see my friend abroad" symptom). The
audit estimates ~15% of cross-border calls are lost without one. A **managed,
geo-distributed** relay puts an anycast edge near each peer — lower relayed latency
than a single self-hosted box.

**App side is ready — zero code change.** `GET /api/ice` is provider-agnostic: it
returns STUN always, and TURN **only when the env below is set**, minting
time-limited credentials so the client never holds a long-lived secret (specs
0026 / 0059). Self-hosting is documented separately in
[`infra/coturn/`](../../infra/coturn/README.md); this runbook uses a managed relay.

---

## 1. Pick a provider

Any standards-compliant TURN works. Geo-distributed, low-ops options:

| Provider | Auth model | Notes |
|---|---|---|
| **Cloudflare Calls TURN** | static username/password (per-app key) | anycast, generous free tier, pairs with #111 |
| **Metered (Open Relay)** | static username/password | free tier; global PoPs |
| **Twilio Network Traversal** | ephemeral (REST) | pay-as-you-go; very reliable |

The app supports **two** credential models (it picks automatically, preferring the
shared secret):
- **Static** (managed relays): `TURN_USERNAME` + `TURN_PASSWORD`.
- **REST-secret** (coturn-style): `TURN_SECRET` (the server mints time-limited creds).

## 2. Set the server env (Railway → server service → Variables)

```
TURN_URLS      = turn:turn.example.com:3478?transport=udp,turns:turn.example.com:5349?transport=tcp
TURN_USERNAME  = <managed username>          # static model …
TURN_PASSWORD  = <managed password>
# — OR, for a coturn-style REST secret instead of username/password:
# TURN_SECRET  = <shared hmac secret>
TURN_TTL_SECS  = 3600                         # credential lifetime (default 3600)
```

- `TURN_URLS` is comma-separated; list **UDP first** (lowest latency), then a
  **TCP/TLS (`turns:`)** entry as the firewall-friendly fallback.
- Provide **either** `TURN_USERNAME`+`TURN_PASSWORD` **or** `TURN_SECRET` — not both
  needed. With none set, TURN stays **off** and `/api/ice` returns STUN only.
- Setting variables triggers a Railway redeploy of the current image (no `railway up`
  needed for env-only changes).

## 3. Verify

```bash
curl -s https://<api-host>/api/ice | jq
```
Expect an `iceServers` array containing your `turn:`/`turns:` URLs with a
`username` + `credential`. Then, in a real call from a hard-NAT network (or with the
browser forced to relay), confirm an ICE candidate of type **`relay`** appears and
the call connects. Chrome: `chrome://webrtc-internals` → the candidate-pair shows
`relay`.

## 4. Tuning

- Keep `TURN_TTL_SECS` modest (e.g. 3600) so leaked creds expire quickly.
- If you also deploy Cloudflare (#111), Cloudflare Calls TURN keeps both on one
  vendor.

## Rollback

Unset `TURN_URLS` (or the credential vars) → `/api/ice` reverts to STUN-only and the
app behaves exactly as before. No code or client change.
