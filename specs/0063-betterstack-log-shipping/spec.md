# 0063 — App-side Better Stack log shipping

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | VoxTranslate |
| **Created** | 2026-06-14 |
| **Shipped** | 2026-06-14 |
| **Version** | — |
| **Commits** | `2f41f12` |
| **Depends on** | [0050](../0050-observability/spec.md) |

## 1. Context & Problem

Issue #69 point 3 wants the structured JSON logs (spec 0050) forwarded to an aggregator
(**Better Stack Logs** — our single observability tool, see `infra/betterstack/`) for
retention, dashboards, and windowed error-rate / p95 alerting. The intended path was a
Railway **log drain**, but **Railway has no native log drains** — there is no dashboard
switch that forwards a service's stdout to an external URL. So the canonical lines must
be shipped **app-side**: the server itself POSTs them to the Better Stack source.

## 2. Goals / Non-Goals

**Goals**
- When `BETTERSTACK_SOURCE_TOKEN` is set, the server forwards its (filtered) log events
  to Better Stack Logs as NDJSON over HTTP, in addition to stdout.
- **Opt-in / off by default:** absent the token, zero behaviour change — no channel, no
  task, no extra serialisation (free tier has a volume cap; the owner enables it
  deliberately).
- **Never block request handling:** shipping is best-effort; the logging hot path must
  not wait on the network.

**Non-Goals**
- A forwarder service (Vector / Locomotive) — app-side is simpler on Railway.
- Creating the Better Stack Logs *source* (dashboard or Telemetry API — documented in
  `infra/betterstack/README.md`, not code here).
- Changing the stdout log format or the canonical-line schema (spec 0050) — we reuse it.
- Guaranteed delivery / on-disk durability — logs are best-effort; stdout + the cron
  remain the always-on signals.

## 3. Requirements

- **R1 — Opt-in.** *Given* `BETTERSTACK_SOURCE_TOKEN` is unset, *when* the server starts,
  *then* tracing is initialised exactly as before (no shipping layer, no background task).
- **R2 — Forwarding.** *Given* the token is set, *when* an event passes the log filter,
  *then* its JSON line is enqueued and a background task POSTs batches as NDJSON to the
  ingest URL with `Authorization: Bearer <token>`.
- **R3 — Non-blocking.** *Given* the ship channel is full, *when* a new line is produced,
  *then* it is dropped (best-effort), never blocking the request/logging path.
- **R4 — Configurable endpoint.** Ingest URL defaults to Better Stack's and can be
  overridden via `BETTERSTACK_INGEST_URL` (per-source host).

## 4. Design & Architecture

- **Components / files:**
  - `server/src/log_shipping.rs` — **new.** `layer::<S>() -> Option<Box<dyn Layer<S>>>`:
    `None` unless `BETTERSTACK_SOURCE_TOKEN` is set; otherwise builds a JSON `fmt` layer
    whose `MakeWriter` buffers each formatted event and, on drop, `try_send`s the line
    onto a bounded `tokio::mpsc`, and spawns `flush_loop` to batch + POST as NDJSON.
  - `server/src/observability.rs` — `init_tracing()` switches from the one-shot
    `fmt().init()` to a layered `registry().with(filter).with(stdout_layer).with(bs_layer)`
    so the optional shipping layer can be added alongside the existing stdout formatter.
  - `server/src/lib.rs` — `pub mod log_shipping;`.
- **Sequence (enabled):** event → filter → both the stdout fmt layer and the BS json
  layer render it → BS writer `try_send`s the line → `flush_loop` drains up to `MAX_BATCH`
  (or what's available), concatenates to NDJSON, POSTs to Better Stack → non-2xx logged to
  stderr (not via `tracing`, to avoid feedback).
- **Key decisions:**
  - **Reuse the `fmt().json()` formatter** with a custom `MakeWriter` instead of a
    hand-rolled `Layer` → the shipped lines are byte-identical to the JSON stdout format,
    no field-extraction code, span fields (`request_id`, …) already flattened in.
  - **Bounded channel + drop-on-full** → logging never applies network backpressure to
    request handling (R3). Coarse but safe; stdout still has every line.
  - **Off unless token present** → no cost/latency/volume impact by default (R1).
  - **stderr for the shipper's own errors** → a failed POST can't recursively generate
    more shipped logs.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `log_shipping.rs`: bounded channel, `ChannelMakeWriter`/`ChannelWriter`, `flush_loop`, `ndjson`, `layer()` (+ unit tests) | `server/src/log_shipping.rs` |
| S1 | `init_tracing()` → layered registry with the optional BS layer | `server/src/observability.rs` |
| S2 | `mod` declaration | `server/src/lib.rs` |
| S3 | Docs: corrected Better Stack Logs steps + env vars | `infra/betterstack/README.md` |

## 6. Testing & Verification

- **Unit (`log_shipping.rs`, no env / network / runtime):** the writer forwards a
  complete formatted line to the channel on drop; a full bounded channel drops extra
  lines without blocking or panicking; `ndjson` newline-terminates every line. (Per the
  coverage gotcha, no env-mutating tests in the lib binary — `layer()` itself is exercised
  only at runtime.)
- **Suite:** full `cargo test` green + `cargo clippy --all-targets` clean + `cargo fmt`.
- **Manual:** with a real source token, `BETTERSTACK_SOURCE_TOKEN=… cargo run` and confirm
  lines appear in the Better Stack Logs source; unset → identical stdout-only behaviour.

## 7. Deployment & Operations

- Server-only Rust change; **Railway deploys are manual** (`railway up` from `server/`).
- Env vars (set on Railway to enable): `BETTERSTACK_SOURCE_TOKEN` (required to turn on),
  `BETTERSTACK_INGEST_URL` (optional override; defaults to Better Stack's ingest host).
- Create the Better Stack Logs source first (HTTP / NDJSON) — see
  `infra/betterstack/README.md`. **Mind the free-tier volume cap** before enabling in prod.

## 8. Risks / Open Items

- Best-effort delivery: lines are dropped on channel overflow or POST failure (stdout +
  the GitHub cron remain the always-on signals). Acceptable for dashboards/retention.
- Free-tier log volume: every canonical line ships when enabled; if volume is a concern,
  tighten the env filter (e.g. ship only `canonical=info`) before turning it on.
- Closes the **code** half of #69 point 3; creating the source + Deepgram/Groq/Railway
  spend alerts remain manual account setup.

## 9. References

- Issue: #69 (observability follow-ups); builds on specs 0050 / 0058.
- Files: `server/src/log_shipping.rs`, `server/src/observability.rs`, `server/src/lib.rs`,
  `infra/betterstack/README.md`.
