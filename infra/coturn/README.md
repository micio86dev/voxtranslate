# Self-hosted TURN (coturn) for VoxTranslate — runbook

WebRTC is peer-to-peer, but a **direct** connection fails whenever both peers sit
behind restrictive NATs (symmetric NAT, CGNAT — common on mobile and **cross-border**
calls). STUN alone can't fix that; you need a **TURN relay**. Without it, the call's
`RTCPeerConnection` goes to `failed` and the peer's tile is dropped — exactly the
"I couldn't see my friend in Armenia" symptom (spec [0026](../../specs/0026-turn-relay/spec.md)).

The app is **provider-agnostic**: the server's `GET /api/ice` returns STUN always,
plus TURN **only when `TURN_URLS` + `TURN_SECRET` are set**, minting time-limited
credentials with coturn's REST-API convention. So the client never holds a long-lived
TURN secret. This runbook self-hosts coturn; a managed TURN (Metered / Cloudflare /
Twilio) is a lower-ops alternative if the networking below bites.

> ⚠️ **Honest caveat.** TURN on a PaaS is finicky. We run coturn on **Railway**
> (same account, no new vendor) over **TCP/TLS only** — Railway doesn't do UDP
> well, and TCP/TLS on a single port is the most firewall-friendly path anyway.
> Trade-off: relayed (fallback) calls take TCP's head-of-line latency on bad
> networks; the majority of calls are direct UDP P2P and are unaffected. This
> config is a *starting point* — verify a `relay` candidate appears (see "Verify")
> before trusting it. **If Railway's relay-port reachability fights you, point
> `TURN_URLS`/creds at a free managed relay (Metered Open Relay / Cloudflare TURN)
> instead — zero code change**, the `/api/ice` plumbing is provider-agnostic.

## 1. Deploy coturn on Railway (TCP/TLS, same account)

Create a **new service** in the same Railway project from the Docker image
`coturn/coturn:4.6`. Mount this repo's `infra/coturn/turnserver.conf` at
`/etc/coturn/turnserver.conf` (or paste it as the command/config), and set the
service variables:

```
TURN_SECRET      = <openssl rand -hex 32>     # the SAME value you set on the app server
TURN_REALM       = <your coturn domain/host>
TURN_EXTERNAL_IP = <the service's public IP>  # see note
```

- **Expose a TCP port.** In the service's **Settings → Networking**, add a **TCP
  Proxy** for coturn's listening port `3478`. Railway returns a `host:port` — that's
  your TURN endpoint. (No UDP proxy: Railway doesn't expose UDP, so we go TCP-only.)
- **`external-ip`.** coturn must advertise the address clients reach it on. Resolve
  the Railway TCP-proxy host to its IP for `TURN_EXTERNAL_IP`, or use coturn's
  `external-ip=<private>/<public>` form if Railway NATs the container.
- **TLS (`turns:`) is optional** and needs a cert *at coturn*; start with plain
  `turn:...?transport=tcp` and add TLS only if a strict firewall blocks it.

## 2. Point the app server at it

On the **voxtranslate-server** (Railway) service, set:

```
TURN_URLS   = turn:<railway-coturn-host>:<tcp-port>?transport=tcp
TURN_SECRET = <the SAME value as coturn's TURN_SECRET>
TURN_TTL_SECS = 3600        # optional, default 3600
```

Redeploy (`railway up`). `GET /api/ice` now returns the TURN server with a fresh
`username`/`credential` per request; the client fetches it before each call.

## 3. Verify

- `curl https://<server>/api/ice` → JSON with a `turn:`/`turns:` entry that has a
  `username` like `1718200000:vox` and a base64 `credential`.
- Paste the server + minted creds into the **Trickle ICE** test
  (<https://webrtc.github.io/samples/src/content/peerconnection/trickle-ice/>) and
  confirm you get a candidate of type **`relay`**. No `relay` candidate ⇒ the relay
  ports / `external-ip` aren't reachable (revisit §1).
- Real test: two devices on **different networks** (e.g. phone on cellular + a peer
  abroad). A relayed call should now connect where it previously dropped.

## Security notes

- `static-auth-secret` is server-to-server only; clients only ever see the derived,
  expiring credential.
- The `denied-peer-ip` ranges block relaying to internal addresses (SSRF).
- Rotate `TURN_SECRET` by setting it on both coturn and the app server together.
