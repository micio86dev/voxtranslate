# Cache Strategy — VoxTranslate Backend

End-to-end guide for implementing, testing, and validating the DragonflyDB
translation cache, following Spec-Driven Development (SDD) and
Test-Driven Development (TDD).

This document is the source of truth for:
- Scope and rationale per tier
- Spec requirements before implementation
- TDD test cases to write before code
- Production benchmark methodology
- Cost/latency report format and decision criteria

For the implementation prompt and architecture details, see `CACHE.md`.

---

## Tier scope — final

| Tier | Provider | Cache | Reason |
|---|---|---|---|
| Standard | Deepgram + Groq | ✅ | Only tier with explicit intermediate text (post-STT, pre-Groq) |
| Enhanced | Soniox | ❌ | Speech-to-speech WebSocket — no text intercept point |
| Pro | OpenAI Realtime | ❌ | Speech-to-speech pipeline — no text intercept point |
| Premium | Gemini Live | ❌ | Speech-to-speech pipeline — no text intercept point |

Pro and Premium were initially considered but confirmed by Claude Code to
be speech-to-speech engines with no wrappable text translation call.
Standard is the only cacheable tier.

---

## Cache key design

```
MD5(normalize(text) + "|" + lang_src + "|" + lang_tgt + "|" + glossary_fingerprint)
```

`glossary_fingerprint`:
- `MD5(sorted(glossary_terms).join(","))` — room has an active glossary
- `""` (empty string) — no glossary active

**Properties this design guarantees:**

| Property | How |
|---|---|
| No cross-glossary contamination | Different glossary sets → different fingerprints → different keys |
| Order-independence | Terms sorted before join — `["muone","fotone"]` == `["fotone","muone"]` |
| Shared cache for same glossary | Same term set → same fingerprint → cache hits shared across rooms |
| Zero regression for no-glossary rooms | Empty fingerprint → key structurally identical to pre-glossary format |
| No cross-language contamination | `lang_src` and `lang_tgt` in key |

---

## Development workflow

### Guiding principles

- **SDD first:** confirm spec (this document) before writing any implementation.
- **TDD always:** write failing tests before writing the code that makes them pass.
- **Read-only audit before implementation:** Claude Code performs a read-only
  pass, reports findings, then proceeds. No file modifications during audit.
- **One passing test suite before Railway deploy:** `cargo test` and
  `cargo clippy -- -D warnings` must both pass before any deploy.

### Implementation order

```
1. Read-only audit          identify Groq call site, glossary structure, config pattern
2. Cache module             src/cache/mod.rs — normalize, word_count,
                            glossary_fingerprint, cache_key, TranslationCache
3. Write TDD tests          all failing — see test cases below
4. Config + app state       add env vars, lazy optional connection
5. Groq call site wrap      cache lookup → HIT return / MISS → Groq → store
6. Benchmark endpoint       POST /internal/bench/translate (guarded)
7. Run tests                cargo test + cargo clippy — all green
8. Deploy to Railway        TRANSLATION_CACHE_ENABLED=false
9. Run bench_cache.sh       measure, decide, auto-toggle Railway env var
```

---

## Spec requirements

All points below must be confirmed satisfied before implementation begins.
Claude Code must check each one during the audit phase.

### Functional spec

- [ ] Cache is opt-in — `TRANSLATION_CACHE_ENABLED=false` (default) is
      identical to pre-cache behavior, byte for byte
- [ ] DragonflyDB connection is lazy — backend starts normally if
      `DRAGONFLY_PRIVATE_URL` is absent or DragonflyDB is down
- [ ] On any cache error: log warning at `WARN` level, fall through to
      Groq call — never fail the request
- [ ] Word count filter applied before any cache I/O — phrases longer than
      `TRANSLATION_CACHE_MAX_WORDS` always bypass the cache
- [ ] Cache key includes `lang_src`, `lang_tgt`, and `glossary_fingerprint`
- [ ] `glossary_fingerprint` is order-independent (sort before hash)
- [ ] Rooms with no glossary use `""` as fingerprint — no special-casing needed
- [ ] TTL applied on every write (`TRANSLATION_CACHE_TTL_SECONDS`, default 7 days)
- [ ] Eviction policy on DragonflyDB: `allkeys-lfu`
- [ ] No `.unwrap()` or `.expect()` anywhere in cache code paths

### Security spec

- [ ] `/internal/bench/translate` returns 404 if `BENCH_SECRET` env var not set
- [ ] `/internal/bench/translate` returns 401 if `Authorization` header wrong
- [ ] Benchmark endpoint not listed in public API docs or OpenAPI spec
- [ ] `BENCH_SECRET` never logged

### Observability spec

- [ ] `tracing::info!` on successful DragonflyDB connection at startup
- [ ] `tracing::warn!` on connection failure or missing URL — with reason
- [ ] `tracing::debug!` on cache HIT (key, tier)
- [ ] `tracing::debug!` on cache MISS (key, tier)
- [ ] No sensitive data (translations, glossary terms) in log output

---

## TDD test cases

Write these tests before any implementation. They must all fail first.

```rust
// --- normalize() ---
assert_eq!(normalize("  Ciao Come Stai?  "), "ciao come stai?");
assert_eq!(normalize("hello   world"),        "hello world");
assert_eq!(normalize(""),                     "");
assert_eq!(normalize("UPPER"),                "upper");

// --- word_count() ---
assert_eq!(word_count(""),                    0);
assert_eq!(word_count("ciao"),                1);
assert_eq!(word_count("ciao come stai"),      3);
assert_eq!(word_count("  spaces  here  "),    2);

// --- glossary_fingerprint() ---

// No glossary → empty string
assert_eq!(glossary_fingerprint(None),         "");
assert_eq!(glossary_fingerprint(Some(&[])),    "");

// Order-independence
let fp_a = glossary_fingerprint(Some(&["muone".into(), "fotone".into()]));
let fp_b = glossary_fingerprint(Some(&["fotone".into(), "muone".into()]));
assert_eq!(fp_a, fp_b, "same terms different order must produce same fingerprint");

// Different glossaries → different fingerprints
let fp_mkt = glossary_fingerprint(Some(&["marketing".into(), "funnel".into()]));
assert_ne!(fp_a, fp_mkt);

// Non-empty fingerprint is not empty string
assert!(!fp_a.is_empty());

// --- cache_key() — isolation ---

// No glossary key differs from glossary key for same phrase
let k_plain   = cache_key("ciao come stai", "it", "en", "");
let k_physics = cache_key("ciao come stai", "it", "en", &fp_a);
let k_mkt     = cache_key("ciao come stai", "it", "en", &fp_mkt);
assert_ne!(k_plain,   k_physics);
assert_ne!(k_physics, k_mkt);
assert_ne!(k_plain,   k_mkt);

// Different target language → different key
let k_fr = cache_key("ciao come stai", "it", "fr", "");
assert_ne!(k_plain, k_fr);

// Normalization applied in key (uppercase == lowercase)
assert_eq!(
    cache_key("CIAO COME STAI", "it", "en", ""),
    cache_key("ciao come stai", "it", "en", ""),
    "cache_key must normalize text before hashing"
);

// Same glossary terms in different order → same key
let k_physics_reversed = cache_key("ciao come stai", "it", "en", &fp_b);
assert_eq!(k_physics, k_physics_reversed);

// --- Integration guard (no live deps) ---
// When TRANSLATION_CACHE_ENABLED=false:
//   translation_cache in AppState is None
//   Groq call is always made (use mock)
// When DragonflyDB unavailable:
//   translation_cache in AppState is None
//   Groq call is always made, no error returned
// When phrase word_count > TRANSLATION_CACHE_MAX_WORDS:
//   cache lookup never called regardless of flag
```

---

## Production benchmark methodology

### Protocol

| Step | Detail |
|---|---|
| Warmup | Discard first 2 requests (cold TCP, process warmup) |
| Baseline | N=20 requests, `TRANSLATION_CACHE_ENABLED=false` |
| Cache warmup | 1 request with cache enabled — populates DragonflyDB |
| Cached run | N=20 requests, all should be HITs from request 1 onward |
| Glossary pass | N=10 requests with `"glossary": ["termine1","termine2"]` — verify isolation |
| Metrics | mean latency, p95 latency, cache hit rate (from `"cached"` response field) |

### Decision threshold

| Improvement | Action |
|---|---|
| ≥ 20% mean latency reduction | Set `TRANSLATION_CACHE_ENABLED=true` on Railway (keep) |
| < 20% | Set `TRANSLATION_CACHE_ENABLED=false` on Railway (disable) |

Both outcomes are valid — exit code 0 in both cases.
Exit code 1 only on script errors (timeout, Railway API failure, etc.).

### Railway automation

`scripts/bench_cache.sh` uses the Railway GraphQL API to toggle the env var
and trigger redeploy without manual intervention:

- Mutation: `variableUpsert` (set `TRANSLATION_CACHE_ENABLED`)
- Mutation: `serviceInstanceRedeploy`
- Poll: `GET /health` until 200, max 120s timeout

Required env vars (documented in `scripts/.env.bench.example`):

```bash
RAILWAY_API_TOKEN=          # Railway account token
RAILWAY_PROJECT_ID=         # Project ID (required by the variableUpsert mutation)
RAILWAY_SERVICE_ID=         # Backend service ID (from Railway dashboard URL)
RAILWAY_ENVIRONMENT_ID=     # Usually "production"
VOXTRANSLATE_API_URL=       # https://your-backend.up.railway.app
BENCH_SECRET=               # Must match BENCH_SECRET set on Railway service
GROQ_PRICE_PER_M_INPUT=     # e.g. 0.05  (update when pricing changes)
GROQ_PRICE_PER_M_OUTPUT=    # e.g. 0.10
```

---

## Benchmark report format

`scripts/bench_cache.sh` prints to stdout and also writes
`scripts/bench_results/report_<ISO_timestamp>.md` (gitignored).

```markdown
# VoxTranslate Cache Benchmark Report
Generated : <ISO timestamp>
Environment: production (Railway)
Commit    : <git rev-parse --short HEAD>

---

## Standard Tier (Deepgram + Groq)

### Latency

|               | Mean (ms) | p95 (ms) | Hit rate |
|---|---|---|---|
| No cache      | XXX.X     | XXX.X    | —        |
| With cache    | XXX.X     | XXX.X    | XX%      |
| + Glossary    | XXX.X     | XXX.X    | XX%      |

Improvement: XX.X%

### Cost estimate (Groq)

| Metric | Value |
|---|---|
| Avg input tokens / phrase | N |
| Avg output tokens / phrase | N |
| Cost / 1k requests (no cache) | $X.XX |
| Cost / 1k requests (with cache, XX% hit rate) | $X.XX |
| Saving / 1k requests | $X.XX |
| Estimated saving / month (at 10k req/day) | $XX.XX |

### Glossary isolation check

| Scenario | Key collision detected |
|---|---|
| No glossary vs physics glossary | ❌ None (correct) |
| Physics glossary vs marketing glossary | ❌ None (correct) |
| Same glossary, different term order | ✅ Same key (correct) |

---

## Tiers not benchmarked

| Tier | Reason |
|---|---|
| Enhanced (Soniox) | Speech-to-speech — no text intercept point |
| Pro (OpenAI Realtime) | Speech-to-speech — no text intercept point |
| Premium (Gemini Live) | Speech-to-speech — no text intercept point |

---

## Final Railway env state

| Variable | Value |
|---|---|
| `TRANSLATION_CACHE_ENABLED` | true / false |
| `TRANSLATION_CACHE_MAX_WORDS` | 8 |
| `TRANSLATION_CACHE_TTL_SECONDS` | 604800 |
| `DRAGONFLY_PRIVATE_URL` | (set by Railway) |

## Decision

✅ Cache ENABLED — improvement XX.X% exceeds 20% threshold.
⚠️  Cache DISABLED — improvement XX.X% below 20% threshold.
```

---

## Files produced by this feature

```
CACHE.md                            Architecture + Claude Code prompt
CACHE_STRATEGY.md                   This file (SDD guide + benchmark spec)
src/cache/mod.rs                    Shared cache module
scripts/bench_cache.sh              Benchmark + Railway auto-toggle
scripts/.env.bench.example          Env var reference (committed, no secrets)
scripts/bench_results/              Generated reports — gitignored
```

Add to `.gitignore`:
```
scripts/bench_results/
scripts/.env.bench
```