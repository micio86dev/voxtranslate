# 0069 — Bounded translation fan-out (admission semaphore on Groq)

| | |
|---|---|
| **Status** | ✅ Shipped |
| **Owner** | VoxTranslate |
| **Created** | 2026-06-15 |
| **Shipped** | 2026-06-15 |
| **Version** | — |
| **Commits** | `155f536` |
| **Depends on** | [0064](../0064-high-traffic-abuse-hardening/spec.md), [0065](../0065-bounded-hot-path-channels/spec.md) |

## 1. Context & Problem

The high-traffic audit (issue **#114**) flagged the real-time **Groq translation
fan-out** as having "no backpressure / unbounded spawn": every final transcript
fans out one `tokio::spawn` per target language with no cap on how many Groq
calls run at once (`server/src/translator.rs`, called from `deepgram.rs`,
`lib.rs`, `files.rs`).

Spec **0065** bounded the per-session hot-path channels (memory is now flat under
a slow consumer) and explicitly deferred the *concurrency* cap as a follow-up:

> "An admission **semaphore** limiting concurrent Deepgram/Groq fan-out —
> desirable but a distinct change; tracked under #114/#123." (0065 §2, §8)

The k6 load test (#114) confirmed the server relays 300–400 concurrent peers with
sub-ms connect and ~30–50 MB RAM — so the server itself is not the near-term
ceiling. The remaining cost/availability risk is **outbound**: a spike of
simultaneous speakers, each fanning out to several languages, can open an
**unbounded** number of parallel Groq requests → blow the Groq rate limit (429s
cascade) and inflate spend with no admission control. This spec adds that bound.

## 2. Goals / Non-Goals

**Goals**
- A **process-wide cap** on concurrent in-flight Groq *translation* calls, shared
  across every room and speaker, so a traffic spike cannot fan out an unbounded
  number of simultaneous Groq requests (cost + rate-limit backpressure).
- The cap is **env-tunable** (`GROQ_TRANSLATE_MAX_CONCURRENCY`) with a sane
  default sized well above healthy load — zero latency impact in normal use.
- **Zero regression** to translation correctness: the fan-out still returns the
  same `{ lang: text }` map; only *when* a call runs is gated, never *whether*.

**Non-Goals**
- Bounding the **AI features** (report, sentiment, email draft, correction,
  suggestions). Those call `Groq` directly, are export-time / occasional, and are
  already credit-gated; a single shared cap would risk head-of-line blocking
  between batch AI and the latency-critical real-time path. The cap is therefore
  scoped to `Translator`, not `Groq`.
- A **per-room** or **per-speaker** quota — the cap is global admission control,
  not fairness. (A separate follow-up if one tenant ever needs isolation.)
- A Deepgram-side admission cap (1 STT stream per speaking peer is already
  bounded by the peer count and the per-session audio channel of spec 0065).
- Horizontal scale / shared room registry (#114, separate).

## 3. Requirements

- **R1 — Bounded concurrency.** *Given* `GROQ_TRANSLATE_MAX_CONCURRENCY = N`,
  *when* more than `N` translation calls are ready at once across all rooms,
  *then* at most `N` are in flight to Groq simultaneously; the rest **await** a
  permit (FIFO) rather than launching immediately.
- **R2 — Global, shared bound.** *Given* the `Translator` is cloned into every
  call site (deepgram/lib/files), *when* any of them fans out, *then* they all
  draw from the **same** semaphore — the cap is process-wide, not per-clone.
- **R3 — No permit leak.** *Given* a fan-out completes (success, Groq error, or
  same-lang skip), *when* `translate_fanout` returns, *then* every acquired
  permit has been released — concurrency capacity is fully restored for the next
  utterance.
- **R4 — Tunable with safe default.** *Given* no override, *then* the cap is
  `DEFAULT_MAX_INFLIGHT = 64`; *given* a misconfigured `0`, *then* it is floored
  to `1` so the pipeline still makes progress.
- **R5 — Correctness unchanged.** *Then* `translate_fanout` returns the same map
  shape (source included, same-lang skipped, failed calls omitted); the full
  `cargo test` suite stays green; `clippy -D warnings` + `fmt` clean.

## 4. Design & Architecture

- **Files:** `server/src/translator.rs` (semaphore + acquire), `server/src/lib.rs`
  (`AppState::new` reads the env cap), `server/.env.example` (document the knob).

- **`Translator` (translator.rs):** gains an `Arc<Semaphore>` (`tokio::sync`) plus
  the configured `max_inflight: usize` (for inspection). `#[derive(Clone)]` keeps
  the `Arc`, so all clones share one semaphore → the bound is global.
  - `new(groq)` → `with_max_inflight(groq, DEFAULT_MAX_INFLIGHT)`.
  - `with_max_inflight(groq, n)` → `Semaphore::new(n.max(1))` (floor of 1, R4).
  - `max_inflight()` → the configured cap.

- **Fan-out gate:** inside each spawned task, **acquire an owned permit before the
  Groq call** and hold it for the duration:
  ```rust
  let translated = match sem.acquire_owned().await {
      Ok(_permit) => groq.translate(&text, &src, &tgt, &terms).await,
      Err(_) => Err("translator semaphore closed".to_string()),
  };
  ```
  The permit drops when the call returns → capacity restored (R3). The semaphore
  is never closed in production, so the `Err` arm is only hit on shutdown — a
  failed translation (omitted from the map), never an unbounded call.

- **Wiring (lib.rs):** `AppState::new` reads
  `env_u32("GROQ_TRANSLATE_MAX_CONCURRENCY", DEFAULT_MAX_INFLIGHT)` and builds the
  `Translator` via `with_max_inflight` — mirrors the existing per-IP limit knobs
  (`WS_CONNECT_MAX_PER_MIN`, `HTTP_PUBLIC_MAX_PER_MIN`) from spec 0064.

- **Key decisions:**
  - *Semaphore in `Translator`, not `Groq`:* scopes the bound to the
    high-frequency real-time path and keeps batch AI features off the same gate,
    so a burst of report/sentiment work can never starve live translation
    (latency is the absolute priority for the real-time pipeline).
  - *Acquire inside the spawned task (not before spawn):* the spawns stay cheap
    and parallel; only the actual Groq call is gated. Per-utterance fan-out is
    tiny (≤ targets), so the spawn count itself is naturally bounded by the rate
    of final transcripts; the meaningful, costly dimension — concurrent Groq HTTP
    calls — is what the permit caps.
  - *Default 64:* a 4-way room fans out to ≤3 targets/utterance, so 64 covers
    ~20 simultaneous speakers before any throttle — far above healthy load, yet a
    hard ceiling on a spike.

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `Translator` gains `Arc<Semaphore>` + `max_inflight`; `new`/`with_max_inflight`/`max_inflight()`; `DEFAULT_MAX_INFLIGHT = 64` | `server/src/translator.rs` |
| S1 | Fan-out acquires an owned permit around each `groq.translate` call | `server/src/translator.rs` |
| S2 | `AppState::new` reads `GROQ_TRANSLATE_MAX_CONCURRENCY` and builds via `with_max_inflight` | `server/src/lib.rs` |
| S3 | Document the knob | `server/.env.example` |
| S4 | Unit tests: default cap, explicit cap + floor-to-1, fan-out parks when full | `server/src/translator.rs` |

## 6. Testing & Verification

- **Unit (deterministic, no network):**
  - `new_uses_default_concurrency_cap` — `new()` wires `DEFAULT_MAX_INFLIGHT`
    permits (R4); the semaphore reports that many available at rest.
  - `with_max_inflight_caps_and_floors_to_one` — an explicit cap is respected and
    `0` is floored to `1` (R4).
  - `fanout_parks_on_admission_semaphore_when_full` — with the only permit held,
    a fan-out future cannot resolve within 50 ms because the spawned task parks on
    `acquire_owned` and never reaches Groq (R1) — proves the call is gated by the
    permit, with no network involved.
- **Regression (existing, must stay green):** `fanout_includes_source_and_skips_same_lang`
  (map shape, R5), plus the full suite (`cargo test --lib` → 145 passed locally).
- **Suite:** `cargo test` green + `clippy -D warnings` + `fmt --check`.

## 7. Deployment & Operations

- Server-only; **Railway deploy is manual** (`railway up` from `server/`).
- New **optional** env var `GROQ_TRANSLATE_MAX_CONCURRENCY` (default 64). No
  migration, no client change. Lower it to tighten Groq spend under a known spike;
  raise it if a load test shows legitimate bursts approaching the cap.
- Complements the external **Groq usage / rate-limit alert** (owner action, #109):
  this caps the *cause*, the alert catches the *symptom*.

## 8. Risks / Open Items

- The cap is **global**, not per-tenant: a single very busy room shares the pool
  with everyone. Acceptable until multi-tenant isolation is needed (follow-up).
- Spawns still happen before the permit is acquired; the parked-task count is
  bounded in practice by the final-transcript rate, not by a hard limit. If that
  ever matters, gate admission *before* the spawn loop. Not needed at current
  scale (mesh ≤ 4/room).
- Follow-ups still open from #114: external spend/quota alerts (#109), TURN
  (#112), Railway region (#113), Cloudflare WAF (#111) — all owner-side.

## 9. References

- Issues: #114 (audit master), #123 / spec 0065 (named this follow-up).
- Files: `server/src/translator.rs`, `server/src/lib.rs`, `server/.env.example`.
- External: [Groq rate limits](https://console.groq.com/docs/rate-limits),
  [`tokio::sync::Semaphore`](https://docs.rs/tokio/latest/tokio/sync/struct.Semaphore.html).
