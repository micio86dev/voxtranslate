# 0064 — High-traffic abuse hardening (WS/HTTP flood)

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | VoxTranslate |
| **Created** | 2026-06-14 |
| **Shipped** | 2026-06-14 |
| **Version** | — |
| **Commits** | `da1c994` |
| **Depends on** | [0028](../0028-security-hardening/spec.md), [0029](../0029-security-followups/spec.md) |

## 1. Context & Problem

A production-readiness audit (issue #114) found the application code disciplined but the
**availability surface weak under flood**: the highest-volume endpoints had **no rate
limiting** — `/ws` (connect + per-message), the audio binary stream, `/rooms`, `/metrics`.
A botnet (or a single abusive socket) could open unlimited WS connections — each spawning
tasks + an unbounded channel — or spam frames/oversized payloads, exhausting memory/FDs on
the single Railway instance. The existing `RateLimiter` (spec 0029) was wired only to
auth/billing/file/AI HTTP routes. This spec closes the WS/HTTP **flood** gap in code.

(Out of scope here, tracked separately: a WAF/anti-DDoS in front of the origin — #111;
the `X-Forwarded-For`-spoofable IP key; glossary-IDOR authz; bounded hot-path channels.)

## 2. Goals / Non-Goals

**Goals**
- A hard **global cap on concurrent WS connections** per instance (the robust, IP-independent
  flood ceiling), env-tunable to the Railway plan.
- **Per-IP WS connect** rate limiting and **per-IP throttling of `/rooms` and `/metrics`**.
- **Per-connection** message-rate cap and **max frame size** (binary audio + text) — bound a
  single abusive socket without affecting legitimate use.
- Optional **bearer-token gate on `/metrics`** so operational aggregates aren't world-readable.
- No regression to normal calls; all limits are generous vs. real client behaviour.

**Non-Goals**
- WAF / L3-4 DDoS mitigation (platform-level, #111).
- Fixing the spoofable `X-Forwarded-For` IP source (needs Railway-specific verification) — the
  per-IP limits are best-effort; the **global cap** is the robust defense.
- Glossary room-membership authz, bounded hot-path channels (separate follow-ups).

## 3. Requirements

- **R1 — Connection ceiling.** *Given* `ws_conns >= MAX_WS_CONNECTIONS`, *when* a new `/ws`
  upgrade is handled, *then* it is rejected with a `server_busy` error frame and closed; the
  live-connection counter is always balanced (RAII guard) across every return path.
- **R2 — Connect throttle.** *Given* one IP opens `> WS_CONNECT_MAX/min` sockets, *then* further
  upgrades get `429` before upgrading.
- **R3 — Per-socket abuse caps.** *Then* a connection sending `> WS_MSG_MAX` messages per window
  is closed; any frame (binary or text) larger than `MAX_FRAME_BYTES` is dropped.
- **R4 — Public-endpoint throttle.** *Then* `/rooms` and `/metrics` are per-IP rate-limited.
- **R5 — Metrics gate.** *Given* `METRICS_TOKEN` is set, *when* `/metrics` is requested without a
  matching `Authorization: Bearer`, *then* `401`; unset → open as before (R4 still applies).

## 4. Design & Architecture

- **Files:** all in `server/src/lib.rs` (router + WS handler + receive loop + state), reusing
  `rate_limit::RateLimiter` and `observability::client_ip`.
- **State (`AppState`):** add `ws_conns: Arc<AtomicUsize>` (live count), `max_ws_conns: usize`
  (env `MAX_WS_CONNECTIONS`, default 2000), `metrics_token: Option<String>` (env `METRICS_TOKEN`),
  all read once in `AppState::new`.
- **`ConnGuard`:** `acquire(&counter) -> (guard, count)` does `fetch_add`; `Drop` does `fetch_sub`
  — so the count is decremented on *every* `handle_peer` exit (auth reject, room-full, normal).
- **`ws_handler`:** add `HeaderMap`; per-IP connect throttle (`wsconnect:{ip}`) → `429`.
- **`handle_peer`:** acquire the guard right after the socket split; if count exceeds the cap,
  send `server_busy` + close. In the receive loop, before dispatch: drop frames over
  `MAX_FRAME_BYTES`, and a per-connection fixed-window counter closes the socket past `WS_MSG_MAX`.
- **`rooms_handler` / `metrics_handler`:** add `HeaderMap`; per-IP throttle; metrics adds the
  optional bearer-token check first.
- **Constants:** `MAX_FRAME_BYTES = 64 KiB`, `WS_CONNECT_MAX = 40/min`, `HTTP_PUBLIC_MAX = 60/min`,
  `WS_MSG_MAX = 500 / 5 s` (≈100/s sustained — far above audio ~10/s + signaling/drawing bursts).
- **Key decisions:** global atomic cap is the *robust* ceiling (works regardless of IP spoofing);
  per-IP limits are best-effort defense-in-depth; size/rate caps are set generously to never trip
  on real calls; metrics token is opt-in so existing scrapers don't break.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `AppState` fields + env reads + `ConnGuard` + consts | `server/src/lib.rs` |
| S1 | `ws_handler` per-IP connect throttle | `server/src/lib.rs` |
| S2 | `handle_peer` global cap guard + receive-loop frame-size + message-rate caps | `server/src/lib.rs` |
| S3 | `/rooms` + `/metrics` throttle + `/metrics` bearer gate | `server/src/lib.rs` |
| S4 | Integration tests (metrics gate/throttle, rooms throttle) | `server/tests/integration.rs` |

## 6. Testing & Verification

- **Integration:** `/metrics` with `METRICS_TOKEN` set → `401` without the header, `200` with it;
  `/rooms` returns `429` past the per-IP limit; `/metrics` still serves the exposition unthrottled
  under the limit.
- **Unit:** `RateLimiter` already covered (spec 0029); `ConnGuard` balance asserted (acquire N,
  drop, count returns to 0).
- **Suite + load:** full `cargo test` green + `clippy` + `fmt`; the k6 load test (issue #114) is
  run against the hardened build to confirm the limits don't degrade legitimate throughput.

## 7. Deployment & Operations

- Server-only; **Railway deploy is manual** (`railway up`). New optional env (all default to the
  constants above): `MAX_WS_CONNECTIONS` (size to the Railway plan), `WS_CONNECT_MAX_PER_MIN` and
  `HTTP_PUBLIC_MAX_PER_MIN` (per-IP budgets — raise behind a trusted proxy or for load testing),
  and `METRICS_TOKEN` (set it + point the scraper/Better Stack at `/metrics` with the bearer to
  lock the endpoint down).

## 8. Risks / Open Items

- Per-IP limits trust `X-Forwarded-For` (spoofable behind Railway) — best-effort; the global cap
  is the real defense. Fixing the IP source is a tracked follow-up.
- Still needs a **WAF/anti-DDoS** in front of the origin for volumetric L3-4 attacks (#111) — code
  rate-limiting can't absorb a true network flood alone.
- Follow-ups: glossary-IDOR authz, bounded hot-path channels, JWT ban-check on REST (issue #114).

## 9. References

- Issue #114 (audit), #111 (WAF). Builds on specs 0028/0029. Files: `server/src/lib.rs`,
  `server/src/rate_limit.rs`, `server/tests/integration.rs`.
