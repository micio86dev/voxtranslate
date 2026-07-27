# infra/media — webinar media server (MediaMTX + Caddy)

WHIP ingest → LL-HLS for VoxTranslate webinars (Phase 1). The full runbook —
buying the box, DNS, smoke test — is in [`/DEPLOY-HETZNER.md`](../../DEPLOY-HETZNER.md).
This folder holds the deployable configs.

| File | Role |
|---|---|
| `main.tf` | Terraform: Hetzner box (cx32/cax21) + firewall (22, 80, 443, 8189/udp) |
| `mediamtx.yml` | WHIP ingest + LL-HLS + the F1-2 external-auth hook |
| `Caddyfile` | Automatic TLS; reverse-proxies WHIP (:8889) + HLS (:8888) behind :443 |
| `docker-compose.yml` | Runs MediaMTX + Caddy (host networking) |
| `ll-hls-probe.sh` | Diagnoses whether a live webinar is really serving LL-HLS |
| `guest-latency.js` | Console snippet: measures a guest's drift behind the live edge |

## Read replicas (guest playback scaling)

Guests do NOT have to read from the origin. A replica pulls each webinar over
RTSP on the private network and serves LL-HLS itself, so guest bandwidth stops
competing with the host's WHIP ingest and the origin's egress becomes one stream
copy **per replica** instead of one per viewer.

| File | Role |
|---|---|
| `replica/mediamtx.yml` | RTSP pull from the origin + LL-HLS out; read-only, no ingest |
| `replica/Caddyfile` | TLS for `hls.voxtranslate.app`, playback routes only |
| `replica/docker-compose.yml` | MediaMTX + Caddy, same pinned tags as the origin |

**Why not a CDN.** Low-Latency HLS playlists cannot be cached — the blocking
reload is a long-poll that no major CDN can cache or coalesce, so every playlist
request reaches the origin whatever the cache rules say. MediaMTX's documented
CDN path requires disabling the low-latency variant, which puts playback back at
~6 s. Separately, Cloudflare does not permit third-party live video through the
CDN on Free/Pro/Business plans — so the playback hostname stays **grey-cloud**.
See <https://mediamtx.org/docs/features/scaling>.

### Bring one up

```sh
terraform apply -var="hcloud_token=…" -var="admin_ip=$(curl -s ifconfig.me)/32"
# → replica_ipv4, origin_private_ip
```

1. **DNS**: `hls.voxtranslate.app` → the replica IP, **DNS-only (grey cloud)**.
2. **Ship the configs** to `/root/media/` on the replica and start it:
   ```sh
   scp replica/{mediamtx.yml,Caddyfile,docker-compose.yml} root@<replica-ip>:/root/media/
   ssh root@<replica-ip> 'cd /root/media && docker compose up -d'
   ```
3. **Point the control plane at it**: set `MEDIA_HLS_HOST=hls.voxtranslate.app`
   on Railway. `MEDIA_INGEST_HOST` stays on the origin — hosts always publish
   there. The two are separate env vars precisely for this split.
4. **Verify** with a live webinar:
   ```sh
   MEDIA_HLS_HOST=hls.voxtranslate.app ./ll-hls-probe.sh <code>
   ```
   `EXT-X-PART` must be present on the replica too. If it is missing, the RTSP
   pull is up but the low-latency variant is not — check `hlsVariant` on both boxes.

The origin keeps serving HLS on `ingest.voxtranslate.app`; nothing breaks if you
roll `MEDIA_HLS_HOST` back. That is the rollback.

## Host gets `401 "authentication error"` on WHIP publish

MediaMTX denies the publish when the control plane rejects its auth callback. Since
2026-07-27 the server logs the exact reason — look for `media-auth denied:` in the Railway
deploy logs. The four causes are distinguished there: caller-secret mismatch, missing
`MEDIA_*` env, unexpected action, bad/expired publish token.

If it is a **caller-secret mismatch**, check what the box is actually sending:

```sh
ssh root@<box> "rg -n '^authHTTPAddress:' /root/media/mediamtx.yml"
```

The path must end in the same value as `MEDIA_CALLER_SECRET` on Railway. To repair it —
the `sed` is anchored to `^authHTTPAddress:` on purpose, because the comment block above
it also contains `/internal/media-auth/`:

```sh
ssh root@<box>
cd /root/media
read -rs SECRET                          # paste MEDIA_CALLER_SECRET from Railway
sed -i "s|^authHTTPAddress:.*|authHTTPAddress: https://voxtranslate-server-production.up.railway.app/internal/media-auth/$SECRET|" mediamtx.yml
printf 'MEDIA_CALLER_SECRET=%s\n' "$SECRET" > .env && chmod 600 .env   # so CI can self-heal
docker compose restart mediamtx
```

Writing `.env` matters: CI recovers the secret from the running `mediamtx.yml` first and
falls back to `.env`. Without it, a future config mishap has nothing to restore from.

## Diagnosing playback latency

`hlsVariant: lowLatency` is a request, not a guarantee — LL-HLS degrades silently, with
no error anywhere. Always measure the SERVER before tuning the player:

```sh
./ll-hls-probe.sh <webinar-code>     # while the webinar is live
```

It fails loudly on the three things that put a floor under end-to-end latency:

1. **No `EXT-X-PART` / `CAN-BLOCK-RELOAD`** — plain HLS on the wire. The player then
   holds back 3 target durations (~6 s) and no client tuning can undo it.
2. **`Access-Control-Allow-Origin: *`** — invalid together with the credentials the
   player must send for MediaMTX's session cookie, so every blocking reload is rejected
   and hls.js quietly abandons low latency.
3. **Segments far longer than `hlsSegmentDuration`** — MediaMTX can only cut a segment on
   an IDR frame, so the real segment length is the WHIP publisher's keyframe interval.
   A browser encoder's multi-second GOP raises `EXT-X-TARGETDURATION` with it.

Once the server is clean, measure the client: paste `guest-latency.js` into the DevTools
console on `/w/<code>`, let it sample, then `voxLatency.stop()`. A drift that only grows,
with `playbackRate` pinned at 1.00, means the player is not catching up to the live edge.

## Secrets (never commit real values)

Two independent secrets, each 32 random bytes (`openssl rand -hex 32`):

- **`MEDIA_CALLER_SECRET`** — authenticates MediaMTX to the control plane. It
  replaces `REPLACE_CALLER_SECRET` in `mediamtx.yml` **and** must byte-match
  `MEDIA_CALLER_SECRET` on the control plane (Railway).
- **`MEDIAMTX_AUTH_SECRET`** — the HMAC key the control plane signs publish tokens
  with. Lives **only** on the control plane, never on the box.

Control-plane env for webinars to activate (all required):
`MEDIA_INGEST_HOST=ingest.voxtranslate.app`, `MEDIA_HLS_HOST=ingest.voxtranslate.app`
(Phase 1: HLS from origin), `MEDIAMTX_AUTH_SECRET`, `MEDIA_CALLER_SECRET`.

## Deploy (short form)

```sh
# 1) provision
terraform init
terraform apply -var="hcloud_token=…" -var="admin_ip=$(curl -s ifconfig.me)/32"

# 2) on the box: inject the caller secret into mediamtx.yml, then start
sed -i "s/REPLACE_CALLER_SECRET/$MEDIA_CALLER_SECRET/" mediamtx.yml
docker compose up -d

# 3) DNS: ingest.voxtranslate.app → box IP, DNS-only (grey cloud). NOT orange —
#    Cloudflare's proxy breaks WebRTC ICE.
```
