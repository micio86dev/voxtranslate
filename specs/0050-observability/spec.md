# 0050 — Observability: canonical logs, request IDs, structured logging

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | Alessandro Micelli |
| **Created** | 2026-06-13 |
| **Shipped** | 2026-06-13 |
| **Version** | — |
| **Commits** | _(this PR)_ |
| **Depends on** | [0027](../0027-load-testing-k6/spec.md), [0028](../0028-security-hardening/spec.md) |

## 1. Context & Problem

The server logged ad-hoc lines (`tracing::info!`) in pretty format with no request
correlation — hard to query, trace, or reason about under load. We want production
**observability**: one **canonical log line** per request (Stripe-style wide event),
**request-id propagation** for end-to-end traceability, and **structured JSON** logs a
log tool can index.

## 2. Goals / Non-Goals

**Goals**
- A single canonical line per HTTP request: method, path, status, latency, request id,
  client ip.
- A `request_id` span so every log during a request (incl. errors) carries it; echo it
  back as `x-request-id`.
- A canonical line per **WS session** (room, peer, duration).
- JSON logs when `LOG_FORMAT=json` (prod), pretty locally.

**Non-Goals**
- A metrics endpoint / Prometheus (no scraper in this setup; logs cover it).
- An external APM (Datadog/Honeycomb) — JSON logs can be shipped there later.

## 3. Requirements

- **R1 — Canonical HTTP line.** A `from_fn` middleware emits one
  `info` event (`target: "canonical"`) per request with `status` + `latency_ms`,
  inside a span carrying `request_id`/`method`/`path`/`ip`.
- **R2 — Request id.** Reuse `x-request-id` / `x-railway-request-id` if present, else
  mint a 12-char id; echo it as the `x-request-id` response header.
- **R3 — WS canonical line.** On session end, log `kind="ws"`, `room`, `peer`,
  `duration_secs`.
- **R4 — Structured logs.** `LOG_FORMAT=json` → flattened JSON (span fields inline);
  default → pretty. `canonical=info` is in the default filter so the lines aren't
  dropped by the target filter.

## 4. Design & Architecture

- `server/src/observability.rs` — `init_tracing()` (JSON vs pretty), `canonical_log`
  middleware (span + canonical line + `x-request-id`), `client_ip()` / `request_id()`.
- `server/src/lib.rs` — `mod observability`; `serve()` calls `init_tracing()`; the
  router swaps the suppressed `TraceLayer` for `middleware::from_fn(canonical_log)`;
  `handle_peer` records a `session_start` and emits the WS canonical line on leave.
- `server/Cargo.toml` — `tracing-subscriber` gains the `json` feature.
- **Key decisions:**
  - *Canonical line over per-span trace logs.* One wide, structured event per request
    is the highest-signal, lowest-noise observability primitive — easy to grep/aggregate
    (status rates, p50/p95 latency, top paths) from plain logs, no APM required.
  - *Span-scoped request id.* Putting the id on a span (not just the line) means any
    `error!`/`warn!` during the request inherits it → real traceability.
  - *Reuse the edge request id.* Railway sets `x-railway-request-id`; reusing it ties
    our logs to the platform's.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `init_tracing` (JSON/pretty) + canonical middleware + helpers | `observability.rs` |
| S1 | Wire init + middleware; WS session canonical line | `lib.rs` |
| S2 | `json` feature | `Cargo.toml` |

## 6. Testing & Verification

- `cargo build` + clippy clean.
- Functional: `LOG_FORMAT=json` server → each request logs a canonical JSON line
  (`target:"canonical"`, `status`, `latency_ms`, span `request_id`/`method`/`path`/`ip`)
  and echoes `x-request-id`; verified locally.

## 7. Deployment & Operations

- **Server change** → needs `railway up`. To get JSON logs in prod, set
  **`LOG_FORMAT=json`** on the Railway service (optional; pretty still works).
- Query examples (once JSON): filter `target=canonical` for per-request lines; group by
  `status` for error rates; `latency_ms` for percentiles; `request_id` to trace one
  request across lines.

## 8. Risks / Open Items

- The canonical line adds a tiny per-request overhead (a span + one event) — negligible.
- No log shipper configured; Railway's log viewer indexes them. A future step could
  forward JSON logs to a dedicated tool (Better Stack / Grafana Loki) + add metrics.

## 9. References

- Files: `server/src/observability.rs`, `server/src/lib.rs`, `server/Cargo.toml`.
- Pattern: Stripe "canonical log lines".
