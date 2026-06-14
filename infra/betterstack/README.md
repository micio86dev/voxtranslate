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
| Client | `https://voxtranslate.app/` | `200` |
| Server `/metrics` (optional liveness) | `…up.railway.app/metrics` | `200` |

### Get an API token

Better Stack → **Settings → API tokens** → create an **Uptime** API token.
It's a **secret** — pass it via the env var below; never commit it or paste it where
it lands in git. Rotate it if it's ever exposed.

### Run

```bash
# Preview what would change (no writes):
BETTERSTACK_API_TOKEN=<token> node infra/betterstack/setup-monitors.mjs --dry-run

# Apply:
BETTERSTACK_API_TOKEN=<token> node infra/betterstack/setup-monitors.mjs
```

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
instead of a separate aggregator:

1. Better Stack → **Logs → Connect source** → type **HTTP / Vector** → copy the source
   token + ingest URL.
2. On Railway, add a **log drain** (or a small Vector sidecar) pointing at that URL so
   the canonical JSON lines (`target=canonical`, `request_id`, `status`, `latency_ms`)
   land in Better Stack, giving retention + dashboards + windowed alerting on error
   rate / p95 — the proper version of what the cron approximates.

This part is dashboard/Railway setup (not an API call), so it isn't scripted here.

## Notes

- Rooms are in-memory per instance → scale **vertically** (bigger Railway plan); the
  `/metrics` gauges are per-instance by design (see spec 0058 / issue #69).
