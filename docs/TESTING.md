# Testing guide (plain-English)

A from-scratch explanation of the two kinds of testing you asked about — **load
testing** and **video-call testing** — what they are, why they matter, and exactly
how to run them here. No prior experience assumed.

---

## 1. Load testing — "how many people can it handle?"

### What it is
Normal tests check *correctness* ("does the button work?"). **Load testing** checks
*capacity*: it pretends to be **hundreds of users at once** and watches whether the
server stays fast and stable, or starts slowing down / erroring. The tool we use is
[**k6**](https://k6.io) — you write a small script describing what a fake user does,
and k6 spins up many of them ("VUs" = virtual users) in a ramp (e.g. 0 → 300 over a
minute) while measuring response times and errors.

### What we test (and what we deliberately don't)
The server's job here is narrow, so we only load the parts it actually owns:

| We load-test ✅ | We skip ❌ (and why) |
|---|---|
| **WebSocket signaling**: joining/leaving rooms, chat relay, emoji/mute fan-out to peers | **Video/audio**: it's **peer-to-peer** (browser-to-browser) — it never touches the server, so there's nothing server-side to load |
| **HTTP endpoints**: `/rooms`, `/api/ice`, `/health`, content, etc. | **Speech-to-text (Deepgram) + translation (Groq)**: they're **paid external APIs** — hammering them costs real money and tests *their* servers, not ours |

So a passing load test means *our* server can take the traffic; the external pieces
scale separately.

### How to run it
1. **Install k6**: `brew install k6` (macOS) or see k6's install docs.
2. **Start the server locally** in "guest mode" with throwaway keys (the tests never
   call the real APIs, so the keys are just placeholders):
   ```bash
   cd server
   DEEPGRAM_API_KEY=dummy GROQ_API_KEY=dummy PORT=3001 cargo run --release
   ```
   (`--release` matters — a debug build is much slower and skews the numbers.)
3. **Run the load tests** (in another terminal):
   ```bash
   k6 run -e BASE_URL=http://localhost:3001 loadtest/http.js        # HTTP endpoints
   k6 run -e WS_URL=ws://localhost:3001 -e ROOMS=20 loadtest/signaling.js  # WS rooms
   ```
4. **Read the output**: k6 prints pass/fail "thresholds" (e.g. *95% of requests under
   300 ms*, *errors under 1%*). Green = within budget. Also watch the server's
   CPU/RAM while it runs.

Full operator details + tunables (how many users, how long): [`loadtest/README.md`](../loadtest/README.md)
(spec [0027](../specs/0027-load-testing-k6/spec.md)).

---

## 2. Video-call testing — "do the calls actually connect and look right?"

Video calls are harder to test automatically than a normal web page, because a real
call needs **two people on two different networks** and a working **WebRTC** path
between them. Here's the layered approach, from cheap/automated to real-world.

### a) Automated tests (run in CI on every change)
- **Unit tests** (`webrtc.test.ts`) check the call "plumbing" logic — that the right
  offers/answers are created, tracks are added, etc. Fast, but they don't open a
  real network connection.
- **End-to-end tests** (`e2e/call.spec.ts`, Playwright) launch **two real browsers**
  and have them join the same room, exchange video, chat, react, and leave. This
  catches most UI/flow regressions.
  > ⚠️ **Important limitation:** the two browsers run on the **same machine**
  > (loopback), so there's **no NAT/firewall** between them. That's why the
  > "couldn't see my friend in Armenia" bug (a cross-network connectivity failure)
  > **could not show up in CI** — you only hit it across real, different networks.

### b) Manual cross-network testing (the only way to catch the hard bugs)
The real-world failures (restrictive home/mobile/corporate networks, cross-border
calls) only appear with **two devices on genuinely different networks** — e.g. your
laptop on Wi-Fi + a phone on cellular, or a friend abroad. Steps:
1. Both open the same room and start a call.
2. Confirm each **sees and hears** the other.
3. Test the awkward cases on purpose: phone on **mobile data** (not Wi-Fi), a
   **VPN**, or a restrictive network.

### c) Why TURN matters here (the Armenia fix)
When a direct browser-to-browser connection can't be made (common on mobile/cross-
border), WebRTC needs a **TURN relay** to bounce the media through. We added one
(spec [0026](../specs/0026-turn-relay/spec.md)). To verify it's working:
- `curl https://<server>/api/ice` → the response should list a `turn:`/`turns:`
  server with a `username` + `credential`.
- Paste those into the **Trickle ICE** tester
  (<https://webrtc.github.io/samples/src/content/peerconnection/trickle-ice/>) — if
  you see a candidate of type **`relay`**, the TURN relay is reachable. No `relay`
  line = the relay isn't set up right, and cross-network calls will keep failing.

### d) Diagnostics (planned follow-up)
A small in-call panel using the browser's `getStats()` API would *show* the live
connection type (direct vs relayed), round-trip time, and packet loss — so the next
time a call misbehaves, you can **see why** instead of guessing. Tracked in spec
0026 §8.

---

## TL;DR
- **Load test** = fake hundreds of users to check the *server's* capacity (signaling
  + HTTP only; media + external APIs are out of scope). Run with k6 against a local
  server.
- **Video calls** = unit + same-machine E2E catch most regressions automatically,
  but **real connectivity bugs need two devices on different networks**; TURN is the
  safety net, verified with Trickle ICE.
