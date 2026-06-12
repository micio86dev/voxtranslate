# 0027 — Load Testing the Server with k6

| | |
|---|---|
| **Status** | ✅ Shipped (scripts + runbook) |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-12 |
| **Shipped** | 2026-06-12 |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0002](../0002-video-calls-translated-chat/spec.md), [0005](../0005-accounts-credits-billing/spec.md) |

## 1. Context & Problem

We want to know how much the **server** can take before adding users or load.
But the server's job is narrow on purpose: WebRTC **media is peer-to-peer** (never
hits the server), and **STT (Deepgram)** + **translation (Groq)** are **external
paid APIs**. So a naive "load test the whole pipeline" would (a) be impossible for
the media and (b) just bill external vendors and measure *their* capacity. The
useful, free, repeatable target is what the server actually owns: the **WebSocket
signaling / room fan-out** and the **HTTP endpoints**.

## 2. Goals / Non-Goals

**Goals**
- A k6 suite that loads the **WS signaling + room broadcast** and the **HTTP** API,
  against a **local** server, **without** calling Deepgram/Groq (zero external cost).
- Clear thresholds so a run is pass/fail (CI-friendly) and a runbook to operate it.

**Non-Goals**
- Load-testing WebRTC media (P2P — not server traffic).
- Load-testing the real STT/translation pipeline (external, paid; out of scope —
  mock or accept cost if ever needed).
- Running against production (would disturb real users + bill vendors).

## 3. Requirements

- **R1 — HTTP load.** `loadtest/http.js` ramps VUs against `/health`, `/rooms`,
  `/api/ice`, `/api/content/i18n`, `/api/billing/packages` (the last is 503 in guest
  mode = OK). Thresholds: `http_req_duration p95<300ms / p99<800ms`, errors `<1%`.
- **R2 — Signaling load.** `loadtest/signaling.js` ramps VUs that each join a `/ws`
  room, then chat / emoji / mute / leave. VUs share `ROOMS` rooms so messages
  actually **fan out** to peers. Thresholds: `ws_connecting p95<500ms`,
  `ws_session_errors<1`.
- **R3 — No external calls.** All VUs use `lang=en` (same-lang rooms ⇒ no Groq
  translation) and never send `start`/audio frames (⇒ no Deepgram). The server boots
  with dummy keys.
- **R4 — Operable.** A runbook covers install, booting the server in guest mode
  (`--release`), running, tuning (`ROOMS`, `HOLD_SEC`, stages), and reading results.

## 4. Design & Architecture

- **Files (`loadtest/`):**
  - `http.js` — `ramping-vus` over the no-external GET endpoints; a `Rate`
    (`http_errors`) + duration thresholds.
  - `signaling.js` — `ramping-vus`; each VU `ws.connect`s to
    `/ws?room=load-N&lang=en&id=…&public=false` (guest), then `setInterval`s for
    chat/emoji/mute and `setTimeout`s a leave. Custom counters
    (`ws_room_joined`, `ws_app_messages`, `ws_session_errors`).
  - `README.md` — the runbook.
- **Key decisions:**
  - *Test only what we own* — signaling + HTTP, not media/STT/translation; otherwise
    the numbers are about Vercel/Deepgram/Groq, not us.
  - *Same-lang rooms to dodge Groq* — chat still exercises the relay/broadcast hot
    path (fan-out to peers) without triggering a translation call.
  - *Guest mode (no `DATABASE_URL`)* — WS join needs no JWT, so VUs connect freely;
    dummy `DEEPGRAM`/`GROQ` keys just satisfy boot-time `require()`.
  - *Local-only* — explicit non-goal to hit prod (real users + vendor bills).

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | HTTP endpoint ramp + thresholds | `loadtest/http.js` |
| S1 | WS signaling/fan-out ramp + counters | `loadtest/signaling.js` |
| S2 | Runbook (boot, run, tune, read) | `loadtest/README.md` |

## 6. Testing & Verification

- The scripts are standalone k6 (not part of the app build); they don't affect
  `cargo`/`astro` CI.
- Manual: boot the server in guest mode, run both scripts, confirm thresholds pass
  and the server process stays healthy under the ramp.
- ⚠️ Not executed in this PR — k6 isn't installed in the authoring environment; run
  locally per the runbook.

## 7. Deployment & Operations

- Dev/ops tooling only — nothing ships to prod. Run on a laptop or a dedicated
  staging box.

## 8. Risks / Open Items

- **Loopback ceiling** — on one machine, k6 + the server + the OS socket limits
  share resources; for high VU counts run k6 from a second box (or raise `ulimit
  -n`). The numbers are a *relative* capacity signal, not an absolute SLA.
- **Translation path uncovered** — exercising Groq needs mixed-lang rooms + a mock
  (or budget). A `loadtest/translation.js` with a stubbed Groq is a natural
  follow-up if that path becomes a concern.
- **No automated regression gate yet** — wiring a small nightly k6 smoke (low VUs)
  into CI could catch signaling regressions; deferred.

## 9. References

- Protocol: `server/src/protocol.rs` (`WsParams`, `ClientMessage`), `/ws` +
  `/rooms` + `/api/*` routes in `server/src/lib.rs`.
- Files: `loadtest/http.js`, `loadtest/signaling.js`, `loadtest/README.md`.
