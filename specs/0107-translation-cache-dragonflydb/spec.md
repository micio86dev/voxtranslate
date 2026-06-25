# 0107 — Translation cache (DragonflyDB, Standard tier)

| | |
|---|---|
| **Status** | In progress |
| **Owner** | VoxTranslate |
| **Created** | 2026-06-25 |
| **Shipped** | — |
| **Version** | — |
| **Commits** | — |
| **Depends on** | [0043](../0043-low-latency-capture/spec.md), [0069](../0069-bounded-translate-fanout/spec.md), [0093](../0093-premium-translation-engine/spec.md) |

> Source of truth for the SDD step. The staged implementation prompt and key-format
> rationale live in `server/CACHE.md`; the test cases, benchmark methodology, and
> per-tier scope live in `server/CACHE_STRATEGY.md`. This spec mirrors both — if they
> diverge, those two documents win and this spec is updated to match.

## 1. Context & Problem

The Standard tier (Deepgram + Groq) translates every final transcript by calling
Groq once per target language (`server/src/groq.rs::translate`, fanned out from
`server/src/translator.rs::translate_fanout`, spec 0069). In real conversation the
**same short phrases recur constantly** — greetings, confirmations, fillers
("ciao", "sì", "grazie", "ok perfetto") — and each recurrence pays the full Groq
round-trip (~200–400 ms) and token cost again, despite being deterministic for a
given `(text, src, tgt, glossary)`.

A Redis-compatible cache (**DragonflyDB**, already the plan for Railway) keyed on
the normalized phrase returns those repeats in **<1 ms** with no Groq call, cutting
both latency and spend on the hot path. The cache must be **opt-in** and
**fail-open**: when disabled or when DragonflyDB is unreachable, behavior is
byte-for-byte identical to today's pipeline.

Only the Standard tier is cacheable (see §2): the other tiers translate
speech-to-speech inside an opaque streaming WebSocket with no text intercept point.

## 2. Goals / Non-Goals

**Goals**
- An **opt-in** (`TRANSLATION_CACHE_ENABLED`, default off) translation cache for
  the **Standard tier only**, wrapping the existing Groq call without changing it.
- **Lazy + fail-open** connection: the backend starts and runs normally when
  `DRAGONFLY_PRIVATE_URL` is unset or DragonflyDB is down — the cache degrades to
  "always miss → call Groq", never an error.
- **Zero behavior change when off:** with the flag unset, `AppState`/`Translator`
  carry `cache: None` and the fan-out takes the identical pre-cache Groq path.
- **Correct keying under glossaries:** the key includes an order-independent
  glossary fingerprint, so a translation produced under one room's glossary is
  never served to a room with a different (or no) glossary.
- A **guarded internal benchmark endpoint** + script to measure real production
  latency and drive the keep/disable decision (≥20% mean improvement → keep).

**Non-Goals (tier scope — final, per `CACHE_STRATEGY.md`)**

| Tier | Provider | Cache | Reason |
|---|---|---|---|
| Standard | Deepgram + Groq | ✅ | Only tier with explicit intermediate text (post-STT, pre-Groq) |
| Enhanced | Soniox | ❌ | Speech-to-speech WebSocket — no text intercept point |
| Pro | OpenAI Realtime | ❌ | Speech-to-speech pipeline — no text intercept point |
| Premium | Gemini Live | ❌ | Speech-to-speech pipeline — no text intercept point |

Also out of scope: caching the **AI features** (report, sentiment, email,
correction — occasional, credit-gated, off the hot path); **long-phrase** caching
(over `TRANSLATION_CACHE_MAX_WORDS` bypasses entirely); cross-process invalidation
beyond TTL + `allkeys-lfu` eviction.

## 3. Requirements

- **R1 — Opt-in, off by default.** *Given* `TRANSLATION_CACHE_ENABLED` is unset or
  `false`, *when* the server builds `AppState`, *then* no DragonflyDB connection is
  attempted, `Translator.cache` is `None`, and every translation takes the exact
  pre-cache Groq path (byte-for-byte).
- **R2 — Lazy, fail-open connect.** *Given* `TRANSLATION_CACHE_ENABLED=true` but
  `DRAGONFLY_PRIVATE_URL` is missing **or** the connection fails, *when* the server
  starts, *then* it logs a `warn` (with reason) and continues **without** a cache —
  startup never fails and no request errors.
- **R3 — Cache HIT skips Groq.** *Given* an active cache holding the key for
  `(text, src, tgt, glossary)`, *when* a final transcript needs that translation,
  *then* the cached value is returned and **Groq is not called**.
- **R4 — Cache MISS stores.** *Given* an active cache with no entry for the key,
  *when* the translation is computed, *then* Groq is called as usual and the result
  is written with TTL `TRANSLATION_CACHE_TTL_SECONDS` (default 604800 = 7 days). A
  failed write is logged and swallowed — it never fails the translation.
- **R5 — Word-count bypass.** *Given* the source phrase has more than
  `TRANSLATION_CACHE_MAX_WORDS` words (default 8), *when* it is translated, *then*
  the cache is bypassed entirely (no read, no write) regardless of the flag.
- **R6 — Glossary-safe, order-independent key.** *Given* two rooms translate the
  same phrase/direction with **different** glossary term sets, *then* their keys
  **differ**; *given* the **same** term set in a **different order**, *then* the
  fingerprint (and key) is **identical** (sorted before hashing); *given* **no**
  glossary, *then* the fingerprint is `""` — a stable key identical across all
  no-glossary rooms, structurally equal to the pre-glossary format.
- **R7 — Observability.** *Then* the server logs `info` on a successful DragonflyDB
  connection, `warn` on connect failure / missing URL (with reason), and `debug` on
  cache HIT and MISS (key + tier); **no** translations or glossary terms appear in
  logs, and `BENCH_SECRET` is never logged.
- **R8 — No panics in cache paths.** *Then* the cache module contains no `.unwrap()`
  / `.expect()`; all Redis errors map to `None`/logged-and-ignored.
- **R9 — Guarded bench endpoint.** *Given* `BENCH_SECRET` is unset, *then*
  `POST /internal/bench/translate` returns **404**; *given* a wrong bearer token,
  *then* **401**; *given* the correct token and body `{text, src, tgt, glossary?}`,
  *then* it translates through the same cached path and returns
  `{ translation, latency_ms, cached }`. The endpoint is absent from public/OpenAPI
  docs.
- **R10 — Correctness + suite green.** *Then* `translate_fanout` returns the same
  `{ lang: text }` map shape; the full `cargo test` suite stays green; `clippy -D
  warnings` + `fmt --check` clean.

## 4. Design & Architecture

- **Files:**
  - NEW `server/src/cache/mod.rs` — pure key helpers + `TranslationCache` client.
  - NEW `server/src/bench.rs` — guarded internal benchmark endpoint.
  - `server/src/translator.rs` — cache fields + lookup/store around the Groq call.
  - `server/src/config.rs` — five new env-backed fields.
  - `server/src/lib.rs` — `mod cache; mod bench;`, lazy connect in `AppState::new`,
    `translation_cache` on `AppState`, route in `app()`.
  - `server/Cargo.toml` — `redis` + `md5` deps.
  - `server/CACHE.md`, `server/CACHE_STRATEGY.md`, `scripts/bench_cache.sh`,
    `scripts/.env.bench.example`, `.gitignore` — docs + benchmark assets.

- **Cache key (per `CACHE.md` / `CACHE_STRATEGY.md` — no tier prefix):**
  ```
  cache_key(text, src, tgt, glossary_fp)
    = MD5(normalize(text) + "|" + src + "|" + tgt + "|" + glossary_fp)
  ```
  - `normalize(text)` = lowercase + trim + collapse internal whitespace.
  - `glossary_fingerprint(terms: Option<&[String]>) -> String`:
    - `""` for `None` or empty → stable canonical key, no cross-room contamination.
    - `MD5(sorted(terms).join(","))` otherwise — **sorted** before hashing, so the
      fingerprint is order-independent.
  - Only the Standard tier is cacheable, so a single flat key space is used (no
    `std|` prefix). Cross-language isolation comes from `src`/`tgt` in the key.

- **Glossary fingerprint at the call site:** the fan-out already computes the
  **direction-filtered** glossary pairs at `translator.rs:84-86`
  (`g.terms_for(&src,&tgt) -> Vec<(String,String)>`). Those pairs are flattened to
  a `Vec<String>` of `"src_term=tgt_term"` and passed as `Some(&v)` to
  `glossary_fingerprint` — so the fingerprint captures exactly what alters the Groq
  prompt, while keeping the documented `Option<&[String]>` signature. The bench
  endpoint passes its request's `glossary` array straight through.

- **`TranslationCache` (cache/mod.rs):** thin async wrapper over a `redis`
  `ConnectionManager` (features `tokio-comp`, `connection-manager` — auto-reconnect).
  - `connect(url) -> Result<Self, redis::RedisError>` — builds the client +
    manager; the only fallible step, surfaced to startup for the fail-open warn.
  - `get(&self, key) -> Option<String>` — `GET`; any error → `None` + log; `debug`
    on HIT/MISS (key only, never the value).
  - `set(&self, key, val, ttl_secs) -> Result<(), _>` — `SET key val EX ttl`; caller
    ignores the error (logged). No panics anywhere (R8).

- **Integration (translator.rs):** `Translator` gains
  `cache: Option<Arc<TranslationCache>>`, `cache_max_words: usize`,
  `cache_ttl_secs: u64`. `new`/`with_max_inflight` keep working with `cache = None`
  (existing call sites/tests untouched); a `with_cache(self, …)` builder wires the
  active path. Inside the existing spawned task (around L94), **only when** a cache
  is present and `word_count(text) <= cache_max_words`:
  ```rust
  let fp  = glossary_fingerprint(Some(&terms_as_strings)); // "" when terms empty
  let key = cache_key(&text, &src, &tgt, &fp);
  if let Some(hit) = cache.get(&key).await { /* return hit, skip Groq */ }
  let out = groq.translate(&text, &src, &tgt, &terms).await?; // unchanged
  let _ = cache.set(&key, &out, cache_ttl_secs).await;        // best-effort
  ```
  Otherwise the call is the byte-identical pre-cache `groq.translate(...)`. The
  admission permit (spec 0069) is still acquired around the whole block, so a HIT
  also returns the permit promptly. A `translate_one(text, src, tgt, glossary) ->
  (String, bool)` shares this path and reports HIT/MISS for the bench endpoint.

- **Wiring (lib.rs `AppState::new`, ~L200-209):** after building `groq`, lazily
  connect the cache and fold it into the `Translator`:
  ```rust
  let translation_cache = if config.cache_enabled {
      match config.dragonfly_url.as_deref() {
          Some(url) => match TranslationCache::connect(url).await {
              Ok(c)  => { tracing::info!("translation cache connected (DragonflyDB)"); Some(Arc::new(c)) }
              Err(e) => { tracing::warn!("translation cache unavailable: {e} — continuing without cache"); None }
          },
          None => { tracing::warn!("TRANSLATION_CACHE_ENABLED but DRAGONFLY_PRIVATE_URL unset — continuing without cache"); None }
      }
  } else { None };
  let translator = Translator::with_max_inflight(groq.clone(), translate_max)
      .with_cache(translation_cache.clone(), config.cache_max_words, config.cache_ttl_secs);
  ```
  `translation_cache` is also stored on `AppState` for the bench endpoint. Note
  `AppState::new` is currently sync; the lazy `connect().await` means it either
  becomes `async` or the connect runs on the existing `AppState::init` path —
  settled in S1 to match how callers build state.

- **Bench endpoint (bench.rs):** `POST /internal/bench/translate`,
  `Authorization: Bearer <BENCH_SECRET>`, body `{text, src, tgt, glossary?}`. A
  `BenchAuth` extractor mirrors `admin.rs::AdminAuth` + `constant_eq`: 404 when
  `bench_secret` is `None`, 401 on mismatch (R9). It calls
  `translator.translate_one(...)`, so it honors `TRANSLATION_CACHE_ENABLED` exactly
  like the live pipeline, timing the call for `latency_ms` and surfacing `cached`.

- **Key decisions:**
  - *Cache in `Translator`, not `Groq`:* scopes caching to the real-time
    translation path and keeps the AI features (which also call `Groq`) off it —
    mirrors the 0069 decision to bound `Translator`, not `Groq`.
  - *Glossary fingerprint in the key (vs. bypass-on-glossary):* glossary rooms still
    get cache benefit, isolated per term set; correctness holds because the
    fingerprint is derived from the exact `terms` that change the prompt, sorted for
    order-independence.
  - *No tier prefix:* Standard is the only cacheable tier (Enhanced/Pro/Premium have
    no intercept point), so a single flat key space is simplest; `src`/`tgt` already
    give cross-language isolation.
  - *`allkeys-lfu` eviction:* keeps frequent conversational phrases, evicts one-off
    long ones first — matches the short-phrase hit profile.

- **Env vars:**

  | Var | Default | Meaning |
  |---|---|---|
  | `TRANSLATION_CACHE_ENABLED` | `false` | Master switch (Standard tier) |
  | `DRAGONFLY_PRIVATE_URL` | — | DragonflyDB connection (Railway-provided) |
  | `TRANSLATION_CACHE_MAX_WORDS` | `8` | Bypass cache above N words |
  | `TRANSLATION_CACHE_TTL_SECONDS` | `604800` | Entry TTL (7 days) |
  | `BENCH_SECRET` | — | Bearer for the internal bench endpoint (404 if unset) |

## 5. Implementation

| Slice | What | Key files |
|-------|------|-----------|
| S0 | `normalize`, `word_count`, `glossary_fingerprint`, `cache_key`, `TranslationCache` (+ unit tests, written first). `redis`+`md5` deps | `server/src/cache/mod.rs`, `server/Cargo.toml` |
| S1 | 5 config fields + loaders; `AppState` `translation_cache`; lazy fail-open connect; inject into `Translator` | `server/src/config.rs`, `server/src/lib.rs` |
| S2 | `Translator` cache fields + `with_cache`; cache lookup/store around `groq.translate` (glossary fp from direction pairs); `translate_one`; guard tests | `server/src/translator.rs` |
| S3 | `BenchAuth` + `POST /internal/bench/translate` (optional glossary); route registration | `server/src/bench.rs`, `server/src/lib.rs` |
| S4 | `bench_cache.sh` (baseline → toggle → cached → glossary pass → cost/report) + `.env.bench.example`; `.gitignore`; reconcile CACHE.md/CACHE_STRATEGY.md if needed | `scripts/*`, `.gitignore`, `server/*.md` |

## 6. Testing & Verification

Tests are written **before** implementation (TDD), per `CACHE_STRATEGY.md`'s case list.

- **Unit (deterministic, no network/DB):**
  - `normalize` — `"  Ciao Come Stai?  " → "ciao come stai?"`, `"hello   world" →
    "hello world"`, `"" → ""`, `"UPPER" → "upper"` (R6).
  - `word_count` — `""→0`, `"ciao"→1`, `"ciao come stai"→3`, `"  spaces  here  "→2` (R5).
  - `glossary_fingerprint` — `None→""`, `Some(&[])→""`; order-independent
    (`["muone","fotone"] == ["fotone","muone"]`); different sets → different fp;
    non-empty fp is not `""` (R6).
  - `cache_key` — no-glossary vs glossary vs different-glossary keys all differ;
    different `tgt` → different key; normalized input → same key; same terms
    different order → same key (R6).
- **Integration guards (mock/stub Groq, no live DB):**
  - cache `None` (flag off) → Groq path taken (R1).
  - `word_count > max_words` → cache lookup never called (R5).
  - bench endpoint: 404 without `BENCH_SECRET`, 401 wrong token (R9).
- **Fail-open (R2):** `connect` error mapping covered by unit test; startup-warn
  path exercised by the wiring (bad URL → `None`, no panic).
- **Regression (must stay green):** `fanout_includes_source_and_skips_same_lang`
  and the 0069 semaphore tests (map shape + bound unchanged, R10).
- **Suite:** `cargo test` green + `clippy -D warnings` + `fmt --check`.
- **Manual smoke (optional):** local DragonflyDB via `docker run`, then `curl` the
  bench endpoint twice and observe `cached:false` → `cached:true`.

## 7. Deployment & Operations

- **Server-only; Railway deploy is manual** (`railway up` from `server/`).
- Provision a **DragonflyDB** service on Railway (`DFLY_proactor_threads=4`,
  `DFLY_maxmemory=256mb`, `DFLY_maxmemory_policy=allkeys-lfu`,
  `DFLY_lfu_decay_time=5`, `DFLY_lfu_log_factor=5`); Railway injects
  `DRAGONFLY_PRIVATE_URL`. — **owner infra step.**
- Roll out flag-dark: deploy with `TRANSLATION_CACHE_ENABLED=false`, then run
  `scripts/bench_cache.sh` (needs `BENCH_SECRET`, `VOXTRANSLATE_API_URL`, Railway
  API token/IDs, and `GROQ_PRICE_PER_M_INPUT/OUTPUT` for the cost estimate). The
  script measures baseline vs cached (and a glossary-isolation pass), writes
  `scripts/bench_results/report_<ts>.md`, toggles the flag via the Railway GraphQL
  API (`variableUpsert` + `serviceInstanceRedeploy`, poll `/health`), and **keeps
  the cache enabled only if mean improvement ≥ 20%**, else flips it back off. Both
  outcomes exit 0; exit 1 only on script/API errors.
- Rollback: set `TRANSLATION_CACHE_ENABLED=false` — zero runtime overhead, no
  connection attempted; the DragonflyDB service can stay deployed.

## 8. Risks / Open Items

- **Pro/Premium remain N/A:** speech-to-speech, no text intercept point. If a
  text-translation path is ever added, revisit; today they are documented out in
  `CACHE.md`/`CACHE_STRATEGY.md`.
- **Glossary fingerprint input:** the fan-out fingerprints the direction-filtered
  `"src=tgt"` pairs (what actually alters the prompt); the bench endpoint
  fingerprints a raw term list. Both are deterministic and order-independent, but
  they are different inputs — bench keys are for latency/isolation measurement, not
  for matching live-room keys.
- **Bench endpoint exposure:** guarded by `BENCH_SECRET` and 404 when unset, but it
  is an internal translation path — keep the secret out of logs and the endpoint
  out of public docs.
- **TTL staleness:** a glossary change yields a new fingerprint (new key), so stale
  glossary translations can't be served; non-glossary entries age out at 7 days.

## 9. References

- Docs: `server/CACHE.md` (architecture + implementation prompt),
  `server/CACHE_STRATEGY.md` (scope, TDD cases, benchmark methodology).
- Specs: [0069](../0069-bounded-translate-fanout/spec.md) (Translator/fan-out),
  [0093](../0093-premium-translation-engine/spec.md) +
  [0100](../0100-pro-gemini-live-translate/spec.md) +
  [0101](../0101-soniox-enhanced-tier/spec.md) (streaming tiers = no intercept point).
- Files: `server/src/translator.rs`, `server/src/groq.rs`, `server/src/config.rs`,
  `server/src/lib.rs`.
- External: [DragonflyDB](https://www.dragonflydb.io/docs),
  [`redis` crate](https://docs.rs/redis), [Railway GraphQL API](https://docs.railway.app/reference/public-api).
