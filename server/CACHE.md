# Translation Cache — VoxTranslate Backend

DragonflyDB-backed translation memory for the Standard tier (Deepgram + Groq).
Caches short translated phrases post-STT to reduce Groq API calls and improve
perceived latency in real-time voice sessions.

---

## Architecture

```
Deepgram STT (streaming)
        │
        ▼ is_final: true
   normalize text
        │
        ▼
   word_count <= MAX_WORDS?
        │
   YES  │  NO
        ▼   ──────────────────────────────────────────┐
   build cache key (text + src + tgt + glossary_fp)   │
        │                                             │
        ▼                                             │
   MD5 key lookup (DragonflyDB, <1ms)                 │
        │                                             │
   HIT  │  MISS                                       │
        ▼       ▼                                     │
  return cached  Groq API (~200–400ms)                │
  translation    │                                    │
        │        ▼                                    │
        │   store in cache                            │
        │        │                                    │
        └────────┴────────────────────────────────────┘
                 │
                 ▼
          TTS (browser-side)
```

---

## Cache key format

```
MD5(normalize(text) + "|" + lang_src + "|" + lang_tgt + "|" + glossary_fingerprint)
```

Where `glossary_fingerprint` is:
- `MD5(sorted(glossary_terms).join(","))` if the room has an active glossary
- `""` (empty string) if no glossary is active

**Examples:**

```
# No glossary — fingerprint segment is empty string
MD5("ciao come stai|it|en|")  →  "hi how are you"

# Room with glossary ["muone", "fotone"] — sorted, joined, hashed
glossary_fingerprint = MD5("fotone,muone")
MD5("ciao come stai|it|en|<fingerprint>")  →  different key, isolated result
```

**Why this design:**

- Rooms without a glossary produce a deterministic key identical in structure
  to the pre-glossary format — no cache invalidation needed on rollout.
- Rooms with different glossaries are fully isolated — a physics-domain
  glossary can never contaminate a marketing-domain room's cached result.
- Rooms sharing the same glossary terms (same set, any order) share cached
  results — sorted join before hashing ensures order-independence.

---

## Scope

| Tier | Cache applied | Reason |
|---|---|---|
| Standard (Deepgram + Groq) | ✅ Yes | Post-STT text is wrappable; only tier with text intercept point |
| Enhanced (Soniox) | ❌ No | Speech-to-speech WebSocket — no text intercept point |
| Pro (OpenAI Realtime) | ❌ No | Speech-to-speech pipeline — no text intercept point |
| Premium (Gemini Live) | ❌ No | Speech-to-speech pipeline — no text intercept point |

Standard is the only tier with an explicit intermediate text representation
that can be intercepted between STT output and the translation call.

---

## Environment Variables

| Variable | Default | Description |
|---|---|---|
| `TRANSLATION_CACHE_ENABLED` | `false` | Master switch — set to `true` to enable |
| `TRANSLATION_CACHE_MAX_WORDS` | `8` | Skip cache for phrases longer than N words |
| `TRANSLATION_CACHE_TTL_SECONDS` | `604800` | Cache TTL (default: 7 days) |
| `DRAGONFLY_PRIVATE_URL` | — | Set automatically by Railway after adding DragonflyDB service |
| `BENCH_SECRET` | — | Bearer token guarding `POST /internal/bench/translate`; the endpoint returns 404 when unset. Never logged. |

**DragonflyDB service variables** (set on the DragonflyDB Railway service itself):

| Variable | Value | Description |
|---|---|---|
| `DFLY_proactor_threads` | `4` | Required on Railway to avoid thread limit issues |
| `DFLY_maxmemory` | `256mb` | Max memory before eviction kicks in |
| `DFLY_maxmemory_policy` | `allkeys-lfu` | Evict least-frequently-used keys |
| `DFLY_lfu_decay_time` | `5` | LFU frequency decay (higher = slower decay) |
| `DFLY_lfu_log_factor` | `5` | LFU sensitivity at low frequencies |

---

## Rollback

To disable the cache completely and return to the pre-cache behavior:

```bash
TRANSLATION_CACHE_ENABLED=false
```

With this flag off:
- No DragonflyDB connection is attempted at startup
- `DRAGONFLY_PRIVATE_URL` does not need to exist
- The translation pipeline is identical to the original implementation
- DragonflyDB service can remain deployed without affecting the backend

---

## Why LFU?

`allkeys-lfu` (Least Frequently Used) keeps the most commonly translated short
phrases ("hi how are you", "one moment please", "thank you") and evicts
one-off long phrases. This is ideal for real-time voice translation where a
small set of conversational phrases accounts for the majority of traffic.

---

## Claude Code Implementation Prompt

Copy and paste the prompt below into Claude Code to implement this feature.

---

```
## Context

This is the VoxTranslate Rust/Axum backend. We need to add an optional
DragonflyDB translation cache for the Standard tier (Deepgram + Groq).
DragonflyDB is Redis-compatible, so use the `redis` crate.

Read CACHE.md and CACHE_STRATEGY.md in the project root before starting.
Follow the architecture, key format, and glossary fingerprint design
defined there exactly.

---

## Scope

Only the Standard tier (Deepgram + Groq) is in scope. Pro (OpenAI Realtime)
and Premium (Gemini Live) are speech-to-speech pipelines with no text
intercept point — do not attempt to cache them.

---

## Rules — read before touching any file

1. DO NOT modify any existing translation logic unless strictly necessary
   to insert the cache lookup/store calls.
2. DO NOT change any existing function signatures unless unavoidable.
3. DO NOT remove or alter existing error handling.
4. The cache must be entirely opt-in via `TRANSLATION_CACHE_ENABLED`.
   When false or absent, behavior must be byte-for-byte identical to
   the current implementation.
5. The DragonflyDB connection must be lazy and optional (`Option<Arc<TranslationCache>>`).
   The backend must start and run normally if `DRAGONFLY_PRIVATE_URL` is not set.
6. All new code must follow the existing code style and error handling patterns.
7. All new public items must have doc comments.
8. No `.unwrap()` or `.expect()` in cache code paths.
9. Do not add any Railway-specific configuration files.

---

## Task

### 1. Audit (read-only first pass)

Before writing any code:

- Identify where the Groq translation call happens for the Standard tier
  after Deepgram emits `is_final: true`.
- Identify how per-room glossary terms are currently stored and passed
  into the translation call (struct field, HashMap, Vec<String>, etc.).
- Identify the existing config/env loading pattern.
- Identify the existing dependency injection pattern for app state.
- Report findings as a brief summary before proceeding. Do not modify
  any files during this phase.

### 2. Dependencies

Add to `Cargo.toml` only if not already present:
- `redis` with features `["tokio-comp", "connection-manager"]`
- `md5`

### 3. Cache module

Create `src/cache/mod.rs` (or `src/translation_cache.rs` — match the
project's file structure conventions) with:

```rust
/// Normalizes source text before hashing:
/// lowercase + trim + collapse internal whitespace.
pub fn normalize(text: &str) -> String { ... }

/// Counts words in a string (split on whitespace).
pub fn word_count(text: &str) -> usize { ... }

/// Computes the glossary fingerprint for a set of glossary terms.
/// Returns MD5(sorted(terms).join(",")) if terms is non-empty,
/// or empty string "" if terms is empty or None.
/// Sorting ensures order-independence: ["muone","fotone"] == ["fotone","muone"].
pub fn glossary_fingerprint(terms: Option<&[String]>) -> String { ... }

/// Builds the cache key:
/// MD5(normalize(text) + "|" + src + "|" + tgt + "|" + glossary_fingerprint)
/// glossary_fingerprint is "" for rooms with no active glossary.
pub fn cache_key(text: &str, src: &str, tgt: &str, glossary_fp: &str) -> String { ... }

/// Thin async wrapper around the Redis/DragonflyDB connection.
pub struct TranslationCache { ... }

impl TranslationCache {
    pub async fn connect(url: &str) -> Result<Self, ...> { ... }
    pub async fn get(&self, key: &str) -> Option<String> { ... }
    pub async fn set(&self, key: &str, value: &str, ttl_secs: u64) -> Result<(), ...> { ... }
}
```

### 4. Config

Add to the existing config struct:

```rust
pub cache_enabled: bool,           // TRANSLATION_CACHE_ENABLED, default false
pub cache_max_words: usize,        // TRANSLATION_CACHE_MAX_WORDS, default 8
pub cache_ttl_secs: u64,           // TRANSLATION_CACHE_TTL_SECONDS, default 604800
pub dragonfly_url: Option<String>, // DRAGONFLY_PRIVATE_URL, default None
```

### 5. App state

Add to the existing app state:

```rust
pub translation_cache: Option<Arc<TranslationCache>>,
```

Initialize at startup:

```rust
let translation_cache = if config.cache_enabled {
    match &config.dragonfly_url {
        Some(url) => match TranslationCache::connect(url).await {
            Ok(c) => {
                tracing::info!("Translation cache connected (DragonflyDB)");
                Some(Arc::new(c))
            }
            Err(e) => {
                tracing::warn!("Translation cache unavailable: {e} — continuing without cache");
                None
            }
        },
        None => {
            tracing::warn!("TRANSLATION_CACHE_ENABLED=true but DRAGONFLY_PRIVATE_URL not set — continuing without cache");
            None
        }
    }
} else {
    None
};
```

### 6. Cache integration

In the Standard tier Groq translation call site (identified in step 1),
wrap the Groq call with the cache lookup/store pattern.

Retrieve the room's glossary terms from wherever they are stored (identified
in the audit), compute the fingerprint, and build the key before the lookup:

```rust
// Pseudo-code — adapt to actual types and signatures
if let Some(cache) = &state.translation_cache {
    if word_count(&source_text) <= config.cache_max_words {
        let gfp = glossary_fingerprint(room.glossary_terms.as_deref());
        let key = cache_key(&source_text, &lang_src, &lang_tgt, &gfp);

        if let Some(cached) = cache.get(&key).await {
            // HIT — return immediately, Groq not called
            return Ok(cached);
        }

        // MISS — call Groq, store result
        let translation = groq_translate(...).await?;
        let _ = cache.set(&key, &translation, config.cache_ttl_secs).await;
        return Ok(translation);
    }
}
// Cache disabled or phrase too long — call Groq directly
groq_translate(...).await
```

Do NOT modify the Groq call itself. Only wrap it.

### 7. Tests

Write all tests BEFORE the implementation (TDD). Tests must fail first,
then pass after implementation.

```rust
// normalize()
assert_eq!(normalize("  Ciao Come Stai?  "), "ciao come stai?");
assert_eq!(normalize("hello   world"), "hello world");
assert_eq!(normalize(""), "");

// word_count()
assert_eq!(word_count(""), 0);
assert_eq!(word_count("ciao"), 1);
assert_eq!(word_count("ciao come stai"), 3);
assert_eq!(word_count("  spaces  everywhere  "), 2);

// glossary_fingerprint()
assert_eq!(glossary_fingerprint(None), "");
assert_eq!(glossary_fingerprint(Some(&[])), "");
// order-independence: same terms in different order → same fingerprint
let fp1 = glossary_fingerprint(Some(&["muone".into(), "fotone".into()]));
let fp2 = glossary_fingerprint(Some(&["fotone".into(), "muone".into()]));
assert_eq!(fp1, fp2);
// different glossaries → different fingerprints
let fp3 = glossary_fingerprint(Some(&["marketing".into(), "funnel".into()]));
assert_ne!(fp1, fp3);

// cache_key() — glossary isolation
let k_no_glossary   = cache_key("ciao come stai", "it", "en", "");
let k_physics       = cache_key("ciao come stai", "it", "en", &fp1);
let k_marketing     = cache_key("ciao come stai", "it", "en", &fp3);
assert_ne!(k_no_glossary, k_physics);
assert_ne!(k_physics, k_marketing);

// cache_key() — determinism
assert_eq!(
    cache_key("CIAO COME STAI", "it", "en", ""),
    cache_key("ciao come stai", "it", "en", "")
);

// Integration guard: cache disabled → Option is None, Groq always called
// (use mock/stub for Groq — no live API calls in unit tests)
```

No integration tests against a live DragonflyDB instance.

### 8. Benchmark endpoint

If the backend does not expose a standalone REST endpoint for single-phrase
translation (Standard tier is WebSocket-based), create:

```
POST /internal/bench/translate
Authorization: Bearer <BENCH_SECRET>
Body: { "text": "...", "src": "it", "tgt": "en", "glossary": ["term1", "term2"] }
Response: { "translation": "...", "latency_ms": N, "cached": bool }
```

- `glossary` is optional — omit for no-glossary benchmark runs.
- Guarded by `BENCH_SECRET` env var. Returns 404 if not set, 401 if wrong.
- Respects `TRANSLATION_CACHE_ENABLED` exactly like the real pipeline.
- Not listed in public API docs or OpenAPI spec.

### 9. Final checklist before finishing

- [ ] `TRANSLATION_CACHE_ENABLED=false` → behavior identical to pre-cache code
- [ ] `TRANSLATION_CACHE_ENABLED=true` + no `DRAGONFLY_PRIVATE_URL` → warn + continue
- [ ] `TRANSLATION_CACHE_ENABLED=true` + DragonflyDB down → warn + continue
- [ ] Rooms with no glossary: key uses empty string fingerprint — no regression
- [ ] Rooms with same glossary terms (different order) → same cache key
- [ ] Rooms with different glossary terms → different cache keys
- [ ] Word count filter applied before any cache I/O
- [ ] No `.unwrap()` or `.expect()` in cache code paths
- [ ] All TDD tests written before implementation, all pass after
- [ ] `cargo test` passes
- [ ] `cargo clippy -- -D warnings` passes
- [ ] No existing tests broken

### 10. Production benchmark and automatic Railway toggle

This phase runs AFTER a successful Railway deployment with
`TRANSLATION_CACHE_ENABLED=false`.

Write `scripts/bench_cache.sh` (bash, deps: `curl`, `bc` only):

1. Reads `VOXTRANSLATE_API_URL`, `RAILWAY_API_TOKEN`, `RAILWAY_SERVICE_ID`,
   `RAILWAY_ENVIRONMENT_ID`, `BENCH_SECRET` from environment.
2. **Baseline (no cache):** sends N=20 POST requests to
   `/internal/bench/translate` with a short phrase (`"ciao come stai"`,
   `it→en`, no glossary). Measures per-request latency via
   `curl -s -o /dev/null -w "%{time_total}"`. Computes mean and p95 ms.
3. Enables cache via Railway GraphQL API (`variableUpsert` mutation,
   `TRANSLATION_CACHE_ENABLED=true`), triggers redeploy
   (`serviceInstanceRedeploy`), polls `/health` until 200 (max 120s).
4. **Cached run:** same N=20 requests. Verifies `"cached": true` from
   request 2 onward (logs a warning if hit rate < 90%).
5. Computes `improvement = ((baseline_mean - cached_mean) / baseline_mean) * 100`.
6. Decision:
   - `improvement >= 20` → leave `TRANSLATION_CACHE_ENABLED=true`, print ✅
   - `improvement < 20`  → set `TRANSLATION_CACHE_ENABLED=false` via Railway
     API, redeploy, print ⚠️
7. Also runs a glossary-aware benchmark pass:
   - Same phrase with `"glossary": ["termine1", "termine2"]` in body.
   - Verifies glossary rooms produce different cache keys (different
     `"cached"` pattern on first request vs subsequent).
   - Reports glossary hit rate separately.
8. Prints summary:

```
=== VoxTranslate Cache Benchmark — Standard Tier ===
Phrase        : "ciao come stai" (it → en)
Requests each : 20

                  Mean (ms)   p95 (ms)   Hit rate
  No cache    :   XXX.X       XXX.X       —
  With cache  :   XXX.X       XXX.X       XX%
  + Glossary  :   XXX.X       XXX.X       XX%

  Improvement :  XX.X%
  Decision    :  ✅ Cache ENABLED  /  ⚠️  Cache DISABLED
=====================================================
```

Add `scripts/.env.bench.example` documenting all required vars.
Do NOT hardcode any tokens or IDs.
```