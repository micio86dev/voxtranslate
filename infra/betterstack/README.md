# Better Stack — external monitoring (issue #69)

VoxTranslate's **single** external monitoring/observability tool. Better Stack covers
both halves of issue #69 on one platform — **uptime monitoring** (point 1) and, as a
follow-up, **log shipping / dashboards** (point 3) — so we don't run two SaaS tools.
Its free tier also allows commercial use (unlike UptimeRobot's free plan).

This complements — does not replace — the free in-repo signals:
`.github/workflows/uptime.yml` (cron `/health` + client + error-rate + p95),
`server/railway.toml` (auto-restart), `/metrics` (spec 0058), JSON logs (spec 0050).
Better Stack adds 3-min checks, mobile push, and a public status page.

## 1. Uptime monitors (automated here)

The monitor set is declared in [`monitors.json`](./monitors.json) and applied by the
idempotent script — **the file is the source of truth**, not the dashboard.

Endpoints monitored:

| Monitor | URL | Expect |
|---|---|---|
| Server `/health` | `https://voxtranslate-server-production.up.railway.app/health` | `200` |
| Client | `https://app.voxtranslate.app/` | `200` |
| Server `/metrics` (optional liveness) | `…up.railway.app/metrics` | `200` |

### Get an API token

Better Stack → **Settings → API tokens** → create an **Uptime** API token.
It's a **secret** — pass it via the env var below; never commit it or paste it where
it lands in git. Rotate it if it's ever exposed.

### Run

```bash
# Preview what would change (no writes):
BETTERSTACK_API_TOKEN=<token> node infra/betterstack/setup-monitors.mjs --dry-run

# Apply (the /metrics monitor needs the origin-lock secret — see below):
CF_ORIGIN_SECRET=<secret> BETTERSTACK_API_TOKEN=<token> node infra/betterstack/setup-monitors.mjs
```

**Origin lock (#111 / spec 0078):** the origin now rejects any request except
`/health` that lacks the `x-origin-verify` header that Cloudflare injects. So the
`/metrics` monitor hits the origin **directly** (bypassing Cloudflare's bot mitigation)
**with that header** — `monitors.json` carries `"value": "$CF_ORIGIN_SECRET"`, which the
script resolves from the env var at run time (it's never committed). Set `CF_ORIGIN_SECRET`
to the same value as the Railway env var / Cloudflare Transform Rule. `/health` is exempt
from the lock, so its monitor needs no header. The GitHub cron
(`.github/workflows/uptime.yml`) does the same via the `CF_ORIGIN_SECRET` repo Actions
secret.

Idempotent: monitors are matched by URL, existing ones are PATCHed in place, missing
ones are created. **To change a monitor**, edit `monitors.json` and re-run — the script
reconciles the dashboard to the file (and prints exactly which fields changed).

### Alerts

`monitors.json` enables `email` + `push` (Better Stack mobile app) on the free plan.
`sms` / `call` are left `false` — they require a paid plan + a verified number; flip
them to `true` there. Optionally create a **status page** and an **escalation policy**
(e.g. push immediately, email after 2 consecutive failed checks) in the dashboard.

## 2. Log shipping (point 3 — follow-up, same platform)

To keep it one tool, ship the server's structured JSON logs to **Better Stack Logs**
instead of a separate aggregator.

> **Railway has no native log drains** — there's no dashboard switch that forwards a
> service's stdout to an external URL. So the canonical JSON lines must be shipped
> **app-side**: the Rust server itself POSTs them to the Better Stack source.

1. Create a Better Stack **Logs source** (type **HTTP**, format **NDJSON**): in the
   dashboard (Logs → Connect source) or via the Telemetry API
   (`POST https://telemetry.betterstack.com/api/v1/sources`). Copy the **source token**
   + **ingest URL**.
2. In the server, an env-gated `tracing` layer forwards the canonical lines
   (`target=canonical`, `request_id`, `status`, `latency_ms`) to that URL when
   `BETTERSTACK_SOURCE_TOKEN` / `BETTERSTACK_INGEST_URL` are set on Railway — giving
   retention + dashboards + windowed alerting on error rate / p95, the proper version
   of what the cron approximates. *(Shipping the actual log path is its own spec; see
   issue #69. Mind the free-tier volume cap before enabling it in prod.)*

A forwarder service (Vector / Locomotive) is the alternative to app-side shipping, but
on Railway it can't tap another service's stdout, so app-side is the simpler path.

## Notes

- Rooms are in-memory per instance → scale **vertically** (bigger Railway plan); the
  `/metrics` gauges are per-instance by design (see spec 0058 / issue #69).
