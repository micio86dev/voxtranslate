# Turn on log shipping + external spend/quota alerts (#109)

Two owner-side gaps from the audit (#114, follow-up to #69):
1. **Log shipping is deployed but OFF** — the Better Stack source token isn't set,
   so structured logs aren't being ingested/searchable.
2. **No spend/quota alerts** for Deepgram / Groq / Railway — a traffic spike could
   run up the bill or hit a rate limit with no warning.

The app already produces the data (JSON logs, spec 0050; Better Stack monitors, spec
0063). These steps just switch ingestion on and add external alarms.

---

## 1. Log shipping → Better Stack (app side)

The server ships logs **only when `BETTERSTACK_SOURCE_TOKEN` is set** (opt-in,
spec 0063). Create the source and set the token.

1. Better Stack → **Telemetry → Sources → Connect source → HTTP** (a *log* source,
   separate from the uptime monitors in [`infra/betterstack/`](../../infra/betterstack/README.md)).
2. Copy the **source token**.
3. Railway → **server** service → Variables:
   ```
   BETTERSTACK_SOURCE_TOKEN = <source token>
   # optional, only if Better Stack gives you a per-source ingest host:
   # BETTERSTACK_INGEST_URL = https://in.logs.betterstack.com
   ```
4. Redeploy (env change redeploys the current image). The canonical `info` lines
   (one per request + one per WS session) start flowing.

**Verify:** open the Better Stack *Live tail*, hit `/health` and start a call →
`http` / `ws` / `canonical` lines appear with `request_id`, `ip`, `status`,
`latency_ms`. (After #111 is live, the `ip` is the real client IP.)

## 2. Spend / quota alerts (external dashboards)

No code can read these bills — set the alarms in each provider:

| Provider | Where | Suggested alarm |
|---|---|---|
| **Deepgram** | Console → *Settings → Billing / Usage alerts* | email at, e.g., 50% / 80% / 100% of the monthly budget; a hard usage cap if offered |
| **Groq** | Console → *Billing / Limits* | spend threshold alert; note the **rate limits** and alert before them |
| **Railway** | Project → *Usage / Billing* | *Usage alerts* (Railway emails at configurable $ thresholds); set a soft + hard line |

Tips:
- Pick thresholds off a *normal-week* baseline so a real spike is obvious.
- Deepgram/Groq are the **true capacity ceiling** (one Deepgram stream per speaker,
  Groq fan-out per language) — these alerts are the early-warning the audit flagged.
- Route alerts to the same inbox as Better Stack so all signals land in one place.

## 3. (Optional) tie it together

Better Stack can also alert on **log patterns** — e.g. a spike in
`speech service unavailable` / `deepgram open failed` / the new `#123` channel-
saturation warnings is an early sign of hitting an external limit. Add a log alert on
those messages once shipping is on.

## Rollback

Unset `BETTERSTACK_SOURCE_TOKEN` → shipping turns off cleanly (logs still go to
stdout / Railway). Provider alerts are independent and can be removed in each
dashboard.
