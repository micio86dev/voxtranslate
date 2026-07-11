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
