# 0058 — Prometheus `/metrics` endpoint

| | |
|---|---|
| **Status** | In progress |
| **Owner** | VoxTranslate |
| **Created** | 2026-06-14 |
| **Shipped** | — |
| **Version** | — |
| **Commits** | `<pending>` |
| **Depends on** | [0050](../0050-observability/spec.md) |

## 1. Context & Problem

Observability (spec 0050) gave us canonical JSON logs with request IDs, and the GitHub
uptime cron pings `/health`. Issue #69 asks for the remaining, scraper-facing piece: a
**Prometheus `/metrics`** endpoint so a monitor (Grafana/Prometheus, Better Stack, etc.) can
chart request rate, error rate, **p95 latency**, and live load — and alert on them — without
parsing logs. Today there is no numeric endpoint to scrape; the only signals are log lines and
a binary `/health`.

## 2. Goals / Non-Goals

**Goals**
- `GET /metrics` returns a valid Prometheus text exposition (v0.0.4).
- Cover the signals #69 names: HTTP **request totals by status class**, a **latency histogram**
  (for p95/p99 + error-rate alerting), and live **room/peer gauges** for this instance.
- Near-zero overhead: counters are recorded in the existing per-request middleware, not a new layer.

**Non-Goals**
- Per-route or per-user label cardinality (status-class + a latency histogram is enough; avoids a
  label explosion on a small relay).
- A metrics crate / registry dependency — a handful of atomics + a hand-rolled text render is plenty.
- Cross-instance aggregation — rooms are in-memory per instance, so gauges are per-instance by
  design (the app scales vertically; see #69 notes). A scraper aggregates across instances.
- Standing up the monitor/alerts themselves (account-level dashboards — the rest of #69, manual).

## 3. Requirements

- **R1 — Scrapeable exposition.** *Given* the server is up, *when* a scraper `GET`s `/metrics`,
  *then* it gets `200` with `Content-Type: text/plain; version=0.0.4` and a body parseable by
  Prometheus (every series has `# TYPE`).
- **R2 — Request signals.** *Then* the body exposes `voxtranslate_http_requests_total{status_class}`
  (counter, classes `2xx/3xx/4xx/5xx`) and `voxtranslate_http_request_duration_ms` (histogram with
  `_bucket{le}`, `_sum`, `_count`), recorded once per request by the canonical-log middleware.
- **R3 — Live load gauges.** *Then* the body exposes `voxtranslate_active_rooms` and
  `voxtranslate_active_peers` read live from the room registry at scrape time.
- **R4 — Safe to expose.** Only non-sensitive aggregates (no user data, no per-request detail), so
  the endpoint is served unauthenticated for the scraper.

## 4. Design & Architecture

- **Components / files:**
  - `server/src/metrics.rs` — **new.** Process-global `AtomicU64` counters (per status class, a
    latency sum/count, and cumulative histogram buckets) + `record_request(status, latency_ms)`.
    A pure `render(active_rooms, active_peers) -> String` builds the exposition; split from the
    global read (`snapshot()`) so the formatter is unit-tested without touching globals.
  - `server/src/observability.rs` — `canonical_log` already computes the final `status` +
    `latency_ms`; it now also calls `metrics::record_request(...)` there (no extra timing).
  - `server/src/rooms.rs` — `RoomManager::active_rooms()` / `active_peers()` (read-only counts).
  - `server/src/lib.rs` — `pub mod metrics;` + `GET /metrics` → `metrics_handler` (reads the two
    gauges from `state.rooms`, returns the rendered text with the Prometheus content type).
- **Key decisions:**
  - **Global atomics, not a registry crate** → counters are incremented on the hot path with a
    relaxed atomic add; no dependency, no lock. The handler reads a consistent-enough snapshot.
  - **Status-class + histogram, not per-route labels** → keeps cardinality flat and the exposition
    tiny, while still supporting error-rate and p95/p99 alerts (the stated need).
  - **Reuse the canonical-log middleware** → one place already owns "request finished with status X
    in Y ms", so metrics can't drift from the logs.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `metrics.rs`: atomics, `record_request`, pure `render` (+ unit tests) | `server/src/metrics.rs` |
| S1 | Record from the canonical-log middleware | `server/src/observability.rs` |
| S2 | `RoomManager` gauge methods | `server/src/rooms.rs` |
| S3 | `mod` + route + handler | `server/src/lib.rs` |
| S4 | Integration test for the live `/metrics` HTTP path | `server/tests/integration.rs` |

## 6. Testing & Verification

- **Unit (`metrics.rs`):** `render_from` emits valid exposition (status-class counters, histogram
  buckets incl. `+Inf == count`, `_sum`/`_count`, gauges, one `# TYPE` per metric); `record_request`
  classifies by status and fills cumulative buckets (delta-asserted, order-independent).
- **Integration (`integration.rs`):** a real server `GET /metrics` → `200`, `text/plain`
  content-type, body contains the histogram `# TYPE`, the `2xx` counter line, and `active_rooms 0` /
  `active_peers 0` (no room joined in that test).
- **Suite:** full `cargo test` (159) green + `cargo clippy --all-targets` clean.

## 7. Deployment & Operations

- Server-only Rust change. **Railway deploys are manual** (`railway up` from `server/`, see
  deploy notes) — so `/metrics` goes live on the next `railway up`, not on merge. No env vars or
  migrations. Once live, point a scraper (Prometheus / Better Stack / Grafana Agent) at
  `…railway.app/metrics`; alert on `rate(...http_requests_total{status_class="5xx"})` and a p95 over
  the latency histogram.

## 8. Risks / Open Items

- Counters are process-global and reset on restart (Prometheus counters are expected to reset; the
  scraper handles it via `rate()`/`increase()`).
- The endpoint is unauthenticated. Values are non-sensitive aggregates; if the host is ever made
  fully public and even room/peer counts are deemed sensitive, gate it behind a bearer token.
- Closes only the **code** part of #69; the external monitor + spend/quota alerts remain manual
  account setup.

## 9. References

- Issue: #69 (observability follow-ups); builds on spec 0050.
- Files: `server/src/metrics.rs`, `server/src/observability.rs`, `server/src/rooms.rs`,
  `server/src/lib.rs`, `server/tests/integration.rs`
