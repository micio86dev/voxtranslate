# DEPLOY — Webinar media server on Hetzner (Phase 1)

Runbook for the webinar media path: **WHIP ingest (presenter) → LL-HLS (guests)**.
The control plane (Axum) stays on Railway; only the media server lives here. Deployable
configs are in [`infra/media/`](infra/media/).

Flow: presenter → `POST /webinar/{code}/whip` (WebRTC) → MediaMTX → LL-HLS →
guests read `/webinar/{code}/index.m3u8`. Publish is authorized by the control
plane (F1-2); read/playback are open (the code's entropy is the gate).

---

## 1. What to buy (spend little)

The box does almost no CPU work — STT/translate/TTS are external APIs (Phase 2), and
Phase 1 video is passthrough (no re-encode). The CDN, not the origin, fans out to
guests. So a small shared-vCPU instance is plenty.

| Item | Cost (excl. VAT, EU) | Notes |
|---|---|---|
| **Hetzner CX33** (4 vCPU, 8 GB, 80 GB) | **~€5.5–6.6/mo** | Recommended. 20 TB traffic + 1 IPv4. |
| **Hetzner CAX21** (ARM, 4 vCPU, 8 GB) | ~€8/mo | Native ARM; best price/perf when in stock (often unavailable). |
| Cloudflare R2 (HLS segments, later) | ~€0–1/mo | Egress-free. Phase 1 serves HLS from the origin. |
| Cloudflare CDN | €0 (free plan) | Absorbs guest bandwidth. |
| **Total infra** | **~€7–10/mo** | AI APIs are billed to the org, not here. |

**Do NOT buy CCX/CPX** (dedicated vCPU) — 2–3× the price and unnecessary. New Hetzner
accounts get ~€20 welcome credit. Verify the exact price on the pricing page at signup.

**TL;DR: create a Hetzner account → `voxtranslate-media` project → provision a CX33
(or CAX21) via the Terraform in `infra/media/`.**

## 2. Provision (Terraform)

```sh
ssh-keygen -t ed25519 -C vox-media -f ~/.ssh/vox_media     # once
cd infra/media
terraform init
terraform apply \
  -var="hcloud_token=<Security → API Tokens, Read/Write>" \
  -var="admin_ip=$(curl -s ifconfig.me)/32" \
  -var="server_type=cx33"          # or cax21 for ARM
```

This creates the box (Ubuntu 24.04) + a firewall: **22** (your IP only), **80/443**
(TLS + WHIP + HLS), and **8189/udp** (WebRTC media — essential). Note the output IP.

## 3. Secrets

Generate two independent 32-byte secrets (`openssl rand -hex 32`):

- `MEDIA_CALLER_SECRET` — authenticates MediaMTX to the control plane. Goes in
  `mediamtx.yml` (replaces `REPLACE_CALLER_SECRET`) **and** on the control plane.
- `MEDIAMTX_AUTH_SECRET` — HMAC key that signs publish tokens. **Control plane only.**

## 4. Deploy MediaMTX + Caddy

```sh
# On your machine: copy the configs up.
scp -i ~/.ssh/vox_media infra/media/{mediamtx.yml,Caddyfile,docker-compose.yml} \
    deploy@<IP>:~/media/

# On the box: install Docker, inject the caller secret, start.
ssh -i ~/.ssh/vox_media deploy@<IP>
curl -fsSL https://get.docker.com | sh
cd ~/media
sed -i "s/REPLACE_CALLER_SECRET/<MEDIA_CALLER_SECRET>/" mediamtx.yml
docker compose up -d && docker compose logs -f
```

Caddy fetches a Let's Encrypt cert for `ingest.voxtranslate.app` automatically
(needs port 80 + the DNS record from step 5).

## 5. DNS (read before touching Cloudflare)

- `ingest.voxtranslate.app` → **A record → box IP → DNS-only (grey cloud)**.
  WebRTC media (UDP/ICE) does **not** pass Cloudflare's HTTP proxy; behind the orange
  cloud the ICE candidates point at the wrong place and WebRTC never connects. This
  is also why `webrtcAdditionalHosts` in `mediamtx.yml` is the grey-cloud host.
- `api.voxtranslate.app` (the Axum control plane) stays **behind Cloudflare**, so
  MediaMTX's HTTPS auth call is injected with the CF origin header and passes the
  origin lock. Phase 1 serves HLS from `ingest…`; the R2/CDN `hls.voxtranslate.app`
  split comes later.

## 6. Wire the control plane (F1-2)

Set on Railway (webinars activate only when all are present):

```
MEDIA_INGEST_HOST=ingest.voxtranslate.app
MEDIA_HLS_HOST=ingest.voxtranslate.app
MEDIAMTX_AUTH_SECRET=<hex>
MEDIA_CALLER_SECRET=<same hex as the box>
```

MediaMTX POSTs each publish attempt to the control plane's **direct Railway origin**
(`https://voxtranslate-server-production.up.railway.app/internal/media-auth/<MEDIA_CALLER_SECRET>`,
in `mediamtx.yml`) — NOT `api.voxtranslate.app`. The API is behind Cloudflare, whose
bot managed-challenge returns a 403 JS page to MediaMTX's headless Go client; hitting
the origin directly bypasses it. `/internal/media-auth/*` is exempt from the server's
origin-lock for this, and the container needs the host CA bundle (mounted in the
compose file) to verify the origin's TLS cert. The control plane then verifies the
host's HMAC token + expiry + path.

## 7. Smoke test (closes Phase 1 → F1-6)

1. Create a webinar in the app, hit **Go live** (mints a tokenized publish URL).
2. The presenter publishes; a guest opens `/w/{code}` and sees video + hears audio via
   HLS within ~2–3 s. (For a manual test you can publish a pattern with ffmpeg to
   `https://ingest.voxtranslate.app/webinar/test/whip?token=<minted>` and play
   `.../webinar/test/index.m3u8` in any HLS player.)
3. Verify auth: a WHIP publish **without** a valid `?token=` is rejected; HLS read
   works with no token.

## 8. Toward Phase 2 (translation)

MediaMTX gives WHIP→HLS passthrough. Phase 2 taps the source PCM in the control
plane, runs STT→translate→TTS per active language, and injects the translated audio
renditions into the HLS master playlist (`EXT-X-MEDIA` per language) via a custom
packager — that part is not MediaMTX alone.
