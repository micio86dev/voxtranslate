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

> ⚠️ **Honest caveat.** TURN on a PaaS is finicky: the relay needs the **public IP
> advertised** (`external-ip`) and a **publicly-reachable UDP port range**. This
> config is a validated *starting point*, not a turnkey artifact — test it (see
> "Verify" below) before trusting it in prod. **Railway is a poor fit** (no real
> UDP); use **Fly.io** (UDP support + dedicated IPv4) or a plain VPS.

## 1. Deploy coturn on Fly.io

```bash
fly launch --no-deploy --image coturn/coturn:4.6 --name vox-turn
fly ips allocate-v4               # a DEDICATED IPv4 — note it; it's your external-ip
fly secrets set TURN_SECRET=$(openssl rand -hex 32) \
               TURN_REALM=turn.<your-domain> \
               TURN_EXTERNAL_IP=<the dedicated IPv4 from above>
```

Mount `turnserver.conf` (env placeholders are expanded by coturn ≥4.6) and expose
the ports in `fly.toml`:

```toml
app = "vox-turn"
[build]
  image = "coturn/coturn:4.6"

# STUN/TURN signaling
[[services]]
  protocol = "udp"
  internal_port = 3478
  [[services.ports]]
    port = 3478
[[services]]
  protocol = "tcp"
  internal_port = 3478
  [[services.ports]]
    port = 3478
# turns: over TLS/TCP — the most firewall-friendly path, ONE port (recommended primary)
[[services]]
  protocol = "tcp"
  internal_port = 5349
  [[services.ports]]
    port = 5349
# Relay UDP range — must match min-port/max-port in turnserver.conf. Mapping a
# wide range on a PaaS is awkward; keep it narrow (here 49160–49200).
[[services]]
  protocol = "udp"
  internal_port = 49160          # repeat per port, or prefer turns:/TCP above
```

> If exposing the UDP relay range is painful, lean on **`turns:` over TCP/TLS on
> 5349 (or 443)** — a single port that traverses strict firewalls. Slightly higher
> latency than UDP relay, but it reliably connects.

```bash
fly deploy
```

## 2. Point the app server at it

On the **voxtranslate-server** (Railway) service, set:

```
TURN_URLS = turns:turn.<your-domain>:5349?transport=tcp,turn:turn.<your-domain>:3478?transport=udp
TURN_SECRET = <the SAME value as coturn's TURN_SECRET>
TURN_TTL_SECS = 3600        # optional, default 3600
```

Redeploy. `GET /api/ice` now returns the TURN server with a fresh
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
