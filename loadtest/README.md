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

## VoIP — translated telephone calls (spec 0111)

Three scripts, deliberately separated by how dangerous they are.

| Script | Creates calls? | Moves credits? | Needs auth? |
|---|---|---|---|
| `voip-webhooks.js` | no | no | no |
| `voip-control.js` | no | no | yes (degrades to the auth-reject path without) |
| `voip-ledger.js` | **yes** | **yes** | yes, plus an explicit opt-in |

### `voip-webhooks.js` — the redelivery storm

The realistic worst case for a webhook endpoint is not many calls, it is **the same events
arriving many times, out of order, all at once**, which is what a provider does when it
recovers from a blip.

```bash
k6 run -e BASE_URL=http://localhost:3001 loadtest/voip-webhooks.js
```

It sends **unsigned** bodies on purpose, and every one must come back 401. What is being
measured is how cheaply the server says no: signature verification is the first thing a
flood hits, so if rejecting is expensive an attacker never needs a valid signature. A run
that reports any 2xx is a **security finding**, not a failing test.

### `voip-control.js` — the dialer's hot path

Quoting runs the whole policy gate plus a rate-deck lookup, and the dashboard fires one on
every pause in typing. It never places a call.

```bash
k6 run -e BASE_URL=… -e JWT=… -e ORG_ID=… loadtest/voip-control.js
```

402 is a **passing** response here: it is the policy gate refusing, which is the work being
measured. Thresholds follow spec 0111 §"PERFORMANCE TARGETS" — quote p95 < 500 ms,
history p95 < 300 ms.

### `voip-ledger.js` — one hot organisation

The only script that creates rows and moves credits, so it refuses to start without
`ALLOW_DIALING=true`. Run it against **staging with `VOIP_PROVIDER=mock`**, on a throwaway
organisation with a small balance.

```bash
k6 run -e BASE_URL=… -e JWT=… -e ORG_ID=… -e ALLOW_DIALING=true loadtest/voip-ledger.js
```

Note what it does and does not prove. The *correctness* of the credit reservation under
contention is proven deterministically by `two_concurrent_holds_cannot_both_take_the_last_credits`
in `server/src/voip/reservation.rs`. A load test cannot prove a race is absent — it can only
fail to trigger one, and treating that as evidence is how races reach production. This
measures **throughput** of `SELECT … FOR UPDATE` on one hot organisation row, and whether
the ledger stays coherent while contended. It deliberately does not clean up: the rows it
leaves are the evidence to inspect afterwards (see the script's `teardown`).

### What is NOT load-tested, and why

- **Media.** The audio path is a provider WebSocket carrying real RTP. Synthesising it at
  scale measures our socket handling, which is worth knowing, but doing it against a live
  provider costs money per stream. A synthetic media harness needs the mock provider's
  socket end, and is the next piece of work here.
- **Anything that dials a real carrier.** The 1,000-concurrent-call figure in the spec is a
  **planning scenario**. It is not, and must not become, a thing to reproduce with paid
  PSTN calls.
- **Provider-side capacity.** Our control plane handling N calls says nothing about whether
  the Telnyx account may place them. Record the account's concurrency and channel limits
  separately — see `docs/voip-telnyx-setup.md` §4.

> **Never state "this deployment supports N calls" without a recorded run behind it.**
> That sentence is a commitment, and the numbers here are the only thing that can back it.

## Live Connect — two-phone translated conversations (spec 0121)

`live-connect.js` opens N conversations of two phones each on `/ws/live`, every phone sending
a 100 ms PCM16 24 kHz frame every 100 ms, and reports connect time, time to the first
translated audio, error frames, unexpected closes and `live_busy` refusals. It **creates
sessions and bills their owners**, so it refuses a non-loopback `BASE_URL` and must only be
pointed at a local server whose upstream is the mock — never at real Qwen credentials, never at
staging or production.

```bash
# 1. The local stack: the real server (debug build, local Postgres only) + a Node mock of the
#    Qwen realtime upstream. Needs `docker compose up -d --wait postgres` (pgvector, :55432),
#    `psql` on PATH and a built server (`cd server && cargo build`, or add --build).
node client/e2e/support/live-server.mjs            # server :3101, mock :3102; Ctrl-C stops both

# 2. Accounts. One owner may hold 3 open sessions, so N sessions need ceil(N/3) accounts.
JWTS=$(node client/e2e/support/live-seed.mjs users 6 50)     # 6 accounts, $50 each

# 3. Run: 12 conversations = 24 sockets for 60 s.
k6 run -e BASE_URL=http://127.0.0.1:3101 -e JWTS="$JWTS" -e SESSIONS=12 -e HOLD_SEC=60 loadtest/live-connect.js
```

`live-seed.mjs` writes users straight into the local database and signs HS256 tokens with the
stack's `JWT_SECRET` (the same thing the e2e does, see `client/e2e/support/live-seed.mjs`). It
refuses any database that is not on loopback.

| Metric | Meaning |
|---|---|
| `ws_connecting`, `live_ws_connect_ms` | upgrade time |
| `live_first_translated_ms` | first frame sent to first `translated_audio` received |
| `live_translated_audio_frames` | audio chunks delivered; the threshold fails the run if none arrive |
| `live_sockets_without_audio` | sockets that never heard the other side: a silent direction (see capacity note below) |
| `live_busy_refusals` | sessions refused with `live_busy` at create/join: the capacity gate working, **not** a failure |
| `live_session_create_failed{code}` | any other refusal (`too_many_sessions` means you need more accounts) |
| `live_error_frames{code}` | `error` frames mid-conversation (`live_busy`, `engine_unavailable`, ...); the threshold fails on any |
| `live_unexpected_close` | sockets that ended before `HOLD_SEC` for a reason the script did not cause |
| `live_session_ended_frames` | `session_state` frames: the server closed the conversation |

**Capacity facts to compare the run against** (from the audit; `docs/live-connect.md`,
"Operations"): the Standard engine is capped at **32 upstream sessions per process**
(`QWEN_REALTIME_MAX_SESSIONS`); one conversation needs two (one per speaking direction), so about
**16 simultaneous conversations** fit. The `live_busy` gate refuses a new session at create/join
when fewer than two upstream sessions are free, but this script creates every session *before* any
socket speaks, so the gate sees an idle engine and does not trip; past ~16 conversations the
expected symptom is `live_sockets_without_audio` (a direction that is never translated), not
`live_busy_refusals`. **That expectation has not been run**: the only recorded run is the 6-socket
smoke below. These are design limits, not measured throughput: record a run before quoting a number.

What the client cannot see, read from the server afterwards:

```bash
# the server-side speech -> translated-audio histogram (teardown also prints it)
curl -s http://127.0.0.1:3101/metrics | grep voxtranslate_live_translation_ms_

# outbound-queue overflows (a slow consumer dropped), idle timeouts, capacity refusals
psql "$LIVE_E2E_DATABASE_URL" -c "SELECT kind, count(*) FROM live_session_events
  WHERE ts > now() - interval '10 minutes' GROUP BY kind ORDER BY 2 DESC"
```

With the mock upstream the numbers measure **our** socket handling, room fan-out, metering and
database writes. They say nothing about the provider's latency or capacity, which is the larger
part of what a user feels: that needs a real device run (`docs/live-connect-device-matrix.md`).

**Smoke run, 2026-11-19** (local debug build, mock upstream, 3 conversations = 6 sockets, 20 s):
upgrade p95 3.7 ms, first translated audio 102-104 ms after the first frame, 18 translated
chunks delivered, 1199 frames sent, 0 error frames, 0 unexpected closes, 0 refusals. Server-side
histogram: 6 observations, 12 ms in total. A debug build on one laptop against an instant mock is
a smoke test, not a capacity figure.

## Notes

- Run against a **local** instance (or a dedicated staging box) — never prod: the
  ramp would disturb real users and, with real keys, bill Deepgram/Groq.
- `/api/ice` is included so the new ICE endpoint (spec 0026) is in the sweep.
