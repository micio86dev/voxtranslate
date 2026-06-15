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

# Bounded hot-path channels under sustained fan-out (spec 0065, #123)
k6 run -e WS_URL=ws://localhost:3001 -e ROOMS=20 loadtest/slow-consumer.js
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

## 5. Bounded channels (`slow-consumer.js`, spec 0065 / #123)

Each room runs a few **talkers** flooding chat/emoji/whiteboard and several
**leechers** that only listen. The point is to confirm the per-peer outbound
channel (`out_tx`, bounded to `OUT_CHANNEL_CAP`) keeps server memory **flat** —
the bounded channels must not accumulate under pressure. While it runs, sample
the server's resident memory; it should plateau, not climb:

```bash
while true; do ps -o rss= -p $(pgrep -f voxtranslate-server) | \
  awk '{printf "RSS %.1f MB\n", $1/1024}'; sleep 2; done
```

### Reproducing a true slow consumer

k6's WS client always drains its socket, so it can't reproduce a reader whose
kernel receive window fills and back-pressures the server's `pump_to_ws`. That
close-on-stall path (overflow → clean teardown) is unit-tested
(`rooms::tests::peertx_overflow_keeps_peer_and_signals_close`). To exercise it
live, use a raw client that **pauses its socket** right after joining while
talkers flood the room — the server's bounded `out_tx` fills to the cap and the
connection is closed cleanly (a `warn` log: *"outbound channel saturated … #123"*),
while RSS stays bounded. With Node's `ws`:

```js
// node slow-reader.js  (npm i ws). Opens N stalled readers in one room.
const WebSocket = require('ws');
for (let i = 0; i < Number(process.argv[2] || 5); i++) {
  const s = new WebSocket(`ws://localhost:3001/ws?room=load-0&lang=en&id=leech${i}&public=false`);
  s.on('open', () => s._socket.pause()); // stop reading → fill the server's out_tx
}
setInterval(() => {}, 1 << 30); // keep the process (and the stalled sockets) alive
```

Run a talker flood (`slow-consumer.js` above) into `room=load-0` at the same time.

## Notes

- Run against a **local** instance (or a dedicated staging box) — never prod: the
  ramp would disturb real users and, with real keys, bill Deepgram/Groq.
- `/api/ice` is included so the new ICE endpoint (spec 0026) is in the sweep.
