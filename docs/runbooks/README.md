# Owner ops runbooks

Step-by-step guides for the **manual, account/infra-side** actions that the app
code can't perform on its own (the `ops` label = "manual deploy / infra action for
the owner"). Each maps to an open issue. The app side is already in place; these
are the dashboard/provisioning steps plus the exact env vars to set.

| Runbook | Issue | What it unblocks |
|---|---|---|
| [Cloudflare WAF / anti-DDoS](./111-cloudflare-waf.md) | [#111](https://github.com/micio86dev/voxtranslate/issues/111) | A WAF in front of the Railway origin (security gap #1) |
| [Managed geo-distributed TURN](./112-managed-turn.md) | [#112](https://github.com/micio86dev/voxtranslate/issues/112) | Recover the ~15% cross-border calls that drop without a relay |
| [Pin the Railway region](./113-railway-region.md) | [#113](https://github.com/micio86dev/voxtranslate/issues/113) | Lower subtitle latency (server near users + Deepgram/Groq) |
| [Log shipping + spend alerts](./109-logshipping-spend-alerts.md) | [#109](https://github.com/micio86dev/voxtranslate/issues/109) | Turn on Better Stack log shipping + external cost alarms |

> **Deploy reminder.** The server runs on **Railway and deploys manually**
> (`railway up` from `server/`). Setting a Railway service variable triggers a
> redeploy of the current image; a *code* change still needs `railway up`. The
> client auto-deploys on push to `main` (Vercel).

Related existing runbooks: [`infra/coturn/`](../../infra/coturn/README.md)
(self-hosted TURN) and [`infra/betterstack/`](../../infra/betterstack/README.md)
(uptime monitors + log-shipping source).
