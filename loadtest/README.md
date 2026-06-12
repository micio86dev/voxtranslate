# Load testing (k6) — runbook

Load tests for the **VoxTranslate server** (Rust/Axum): the WebSocket **signaling +
room fan-out** and the **HTTP** endpoints. Spec [0027](../specs/0027-load-testing-k6/spec.md).

## What this does and does NOT test

WebRTC audio/video is **peer-to-peer** — it never touches the server, so k6 can't
(and shouldn't) load it. Deepgram (STT) and Groq (translation) are **external paid
APIs**: load-testing them costs money and measures *their* capacity, not ours. So
these tests deliberately **avoid both**:

| Tested (server-handled) | Excluded (on purpose) |
|---|---|
| WS room join / leave, presence | WebRTC media (P2P, not server) |
| Chat **relay & broadcast** (same-lang → no translation) | Deepgram STT (no `start`/audio frames sent) |
| Emoji / mute / hand-raise fan-out | Groq translation (same-lang rooms) |
| HTTP: `/health`, `/rooms`, `/api/ice`, `/api/content/i18n`, `/api/billing/packages` | Stripe / billing writes |

> The signaling script keeps every VU in **lang=en** rooms and never sends `start`
> or audio, so no external API is called. To also stress translation, run with
> mixed langs — but that **does** hit Groq; mock it or accept the cost.

## 1. Install k6

```bash
brew install k6      # macOS
# or: https://grafana.com/docs/k6/latest/set-up/install-k6/
```

## 2. Boot the server locally in guest mode

Dummy external keys (never used by these tests) and **no `DATABASE_URL`** so it runs
billing-off / guest (WS join needs no auth token):

```bash
cd server
DEEPGRAM_API_KEY=dummy GROQ_API_KEY=dummy PORT=3001 cargo run --release
```

> `--release` matters — a debug build skews latency numbers badly.

## 3. Run the tests

```bash
# HTTP endpoints (ramps to 200 VUs)
k6 run -e BASE_URL=http://localhost:3001 loadtest/http.js

# WS signaling + fan-out (ramps to 300 VUs across 20 rooms)
k6 run -e WS_URL=ws://localhost:3001 -e ROOMS=20 -e HOLD_SEC=20 loadtest/signaling.js
```

Tunables (env): `BASE_URL` / `WS_URL`, `ROOMS` (VUs per room ≈ peak VUs / ROOMS),
`HOLD_SEC` (how long each peer stays). Edit the `stages` in each script to change
the ramp / peak.

## 4. Read the results

- **HTTP** — thresholds: `http_req_duration p95<300ms / p99<800ms`, `http_errors
  rate<1%`. A failing threshold exits non-zero (CI-friendly). Note: `/api/billing/
  packages` returns **503** in guest mode — counted as OK, not an error.
- **Signaling** — thresholds: `ws_connecting p95<500ms`, `ws_session_errors<1`.
  Watch `ws_room_joined`, `ws_app_messages`, `ws_sessions`, and CPU/RAM of the
  server process (the relay fan-out is the hot path).

## Notes

- Run against a **local** instance (or a dedicated staging box) — never prod: the
  ramp would disturb real users and, with real keys, bill Deepgram/Groq.
- `/api/ice` is included so the new ICE endpoint (spec 0026) is in the sweep.
