//! Translation fan-out: translate one text into many target languages in
//! parallel, returning a `{ lang: text }` map (including the source language,
//! unchanged). Wraps the Groq client.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Semaphore;

use crate::cache::{cache_key, glossary_fingerprint, word_count, TranslationCache};
use crate::glossary::RoomGlossary;
use crate::groq::Groq;

/// Default cap on concurrent in-flight Groq translation requests, across every
/// room and speaker (override `GROQ_TRANSLATE_MAX_CONCURRENCY`). Sized well above
/// a healthy load — a 4-way room fans out to at most 3 targets per utterance, so
/// 64 covers ~20 simultaneous speakers — but bounds a traffic spike from opening
/// an unbounded number of parallel Groq calls (spec 0069).
pub const DEFAULT_MAX_INFLIGHT: usize = 64;

/// Fan-out translator over a cloneable Groq client.
///
/// The fan-out spawns one task per target language; a shared **admission
/// semaphore** caps how many of those calls are in flight at once across the
/// whole process, so a burst of speech can't open an unbounded number of
/// simultaneous Groq requests (cost / rate-limit backpressure, spec 0069).
#[derive(Clone)]
pub struct Translator {
    groq: Groq,
    /// Process-wide cap on concurrent Groq translation calls. Shared across all
    /// clones (each call site clones the `Translator`), so the bound is global.
    sem: Arc<Semaphore>,
    /// Configured permit count (== `sem` capacity at rest), kept for inspection.
    max_inflight: usize,
    /// Optional DragonflyDB translation cache (spec 0107). `None` (default) ⇒ every
    /// translation takes the byte-for-byte pre-cache Groq path; populated only when
    /// `TRANSLATION_CACHE_ENABLED` is set AND the lazy connect succeeded (fail-open).
    cache: Option<Arc<TranslationCache>>,
    /// Phrases longer than this many words bypass the cache (no read/write), spec
    /// 0107 R5. Inert while `cache` is `None`.
    cache_max_words: usize,
    /// TTL applied to every cache write, seconds. Inert while `cache` is `None`.
    cache_ttl_secs: u64,
}

impl Translator {
    /// Build a translator with the default concurrency cap
    /// ([`DEFAULT_MAX_INFLIGHT`]).
    pub fn new(groq: Groq) -> Self {
        Self::with_max_inflight(groq, DEFAULT_MAX_INFLIGHT)
    }

    /// Build a translator capping concurrent in-flight Groq calls at
    /// `max_inflight` (floored to 1, so a misconfigured `0` still makes progress).
    /// The cache starts off (`None`); call [`Translator::with_cache`] to wire it.
    pub fn with_max_inflight(groq: Groq, max_inflight: usize) -> Self {
        let permits = max_inflight.max(1);
        Self {
            groq,
            sem: Arc::new(Semaphore::new(permits)),
            max_inflight: permits,
            cache: None,
            // Inert while `cache` is `None`; real values arrive via `with_cache`.
            cache_max_words: 8,
            cache_ttl_secs: 604_800,
        }
    }

    /// Fold an optional translation cache into the translator (spec 0107). `cache =
    /// None` leaves the pre-cache behavior untouched, so this is safe to call
    /// unconditionally from the wiring. Builder-style (consumes + returns `self`) so
    /// it chains off [`Translator::with_max_inflight`].
    pub fn with_cache(
        mut self,
        cache: Option<Arc<TranslationCache>>,
        cache_max_words: usize,
        cache_ttl_secs: u64,
    ) -> Self {
        self.cache = cache;
        self.cache_max_words = cache_max_words;
        self.cache_ttl_secs = cache_ttl_secs;
        self
    }

    /// The configured concurrency cap (permits granted to in-flight Groq calls).
    pub fn max_inflight(&self) -> usize {
        self.max_inflight
    }

    /// Translate `text` from `source_lang` into each of `target_langs` in
    /// parallel. The returned map always contains the source language mapped to
    /// the original text; failed individual translations are simply omitted.
    /// When the room has a `glossary`, each direction gets its matching term
    /// pairs injected into the prompt (spec 0011).
    pub async fn translate_fanout(
        &self,
        text: &str,
        source_lang: &str,
        target_langs: &[String],
        glossary: Option<&RoomGlossary>,
    ) -> HashMap<String, String> {
        let mut translations = HashMap::new();
        translations.insert(source_lang.to_string(), text.to_string());

        let mut tasks = Vec::new();
        for tgt in target_langs {
            if tgt == source_lang {
                continue;
            }
            let groq = self.groq.clone();
            let sem = self.sem.clone();
            let cache = self.cache.clone();
            let cache_max_words = self.cache_max_words;
            let cache_ttl_secs = self.cache_ttl_secs;
            let text = text.to_string();
            let src = source_lang.to_string();
            let tgt = tgt.clone();
            let terms = glossary
                .map(|g| g.terms_for(&src, &tgt))
                .unwrap_or_default();
            tasks.push(tokio::spawn(async move {
                // Hold a permit for the whole translation → at most `max_inflight`
                // run at once process-wide. A cache HIT skips Groq but still holds
                // (and promptly releases) the permit. The semaphore is never closed,
                // so `acquire_owned` only errs on shutdown; treat that as a failed
                // translation (omitted from the map) rather than an unbounded call.
                let translated = match sem.acquire_owned().await {
                    Ok(_permit) => {
                        // Fingerprint the direction-filtered "src=tgt" pairs — exactly
                        // what alters the Groq prompt — so glossary rooms key apart.
                        // Only computed when a cache is present; otherwise unused.
                        let fp = if cache.is_some() {
                            let fp_terms: Vec<String> =
                                terms.iter().map(|(s, t)| format!("{s}={t}")).collect();
                            glossary_fingerprint(Some(&fp_terms))
                        } else {
                            String::new()
                        };
                        translate_cached(
                            &groq,
                            cache.as_deref(),
                            cache_max_words,
                            cache_ttl_secs,
                            &text,
                            &src,
                            &tgt,
                            &terms,
                            &fp,
                        )
                        .await
                        .map(|(translated, _hit)| translated)
                    }
                    Err(_) => Err("translator semaphore closed".to_string()),
                };
                (tgt.clone(), translated)
            }));
        }

        for task in tasks {
            if let Ok((lang, Ok(translated))) = task.await {
                translations.insert(lang, translated);
            }
        }
        translations
    }

    /// Translate a single phrase through the same cached path the live fan-out
    /// uses, returning `(translation, was_hit)`. Backs the internal benchmark
    /// endpoint (spec 0107 R9), so its latency reflects the cache exactly like
    /// production. The optional `glossary` is a raw term list fingerprinted into
    /// the cache key only — no pairs are injected into the Groq prompt here; bench
    /// keys measure latency/isolation, not live-room key matching (spec §8).
    pub async fn translate_one(
        &self,
        text: &str,
        src: &str,
        tgt: &str,
        glossary: Option<&[String]>,
    ) -> Result<(String, bool), String> {
        // Hold an admission permit for the whole call, mirroring the live fan-out
        // (spec 0069) — a HIT still acquires and promptly releases it.
        let _permit = self
            .sem
            .acquire()
            .await
            .map_err(|_| "translator semaphore closed".to_string())?;
        let fp = glossary_fingerprint(glossary);
        translate_cached(
            &self.groq,
            self.cache.as_deref(),
            self.cache_max_words,
            self.cache_ttl_secs,
            text,
            src,
            tgt,
            &[],
            &fp,
        )
        .await
    }
}

/// Translate one `(text, src, tgt)` through the cache when one is present,
/// reporting whether the result was a cache HIT. Shared by the live fan-out and
/// the benchmark endpoint so both honor the identical cache rules (spec 0107).
/// With `cache = None`, or a phrase past the word-count bound, this is the
/// byte-identical pre-cache call: `groq.translate(...)` returning `(_, false)`.
#[allow(clippy::too_many_arguments)]
async fn translate_cached(
    groq: &Groq,
    cache: Option<&TranslationCache>,
    cache_max_words: usize,
    cache_ttl_secs: u64,
    text: &str,
    src: &str,
    tgt: &str,
    terms: &[(String, String)],
    glossary_fp: &str,
) -> Result<(String, bool), String> {
    if let Some(cache) = cache {
        if within_cache_word_limit(text, cache_max_words) {
            let key = cache_key(text, src, tgt, glossary_fp);
            // HIT → return the stored translation, Groq untouched (R3).
            if let Some(hit) = cache.get(&key).await {
                return Ok((hit, true));
            }
            // MISS → compute as usual, then store best-effort with TTL (R4). A
            // failed write is logged inside `set` and swallowed here.
            let out = groq.translate(text, src, tgt, terms).await?;
            let _ = cache.set(&key, &out, cache_ttl_secs).await;
            return Ok((out, false));
        }
    }
    // Cache disabled (R1) or phrase too long (R5): untouched pre-cache path.
    let out = groq.translate(text, src, tgt, terms).await?;
    Ok((out, false))
}

/// Whether `text` is short enough to cache — the word-count filter (spec 0107
/// R5), applied before any cache I/O. Pure, so the bypass rule is unit-tested
/// without a live DragonflyDB. (The "cache must be present" half of eligibility
/// is enforced structurally by the `Option<&TranslationCache>` match above.)
fn within_cache_word_limit(text: &str, max_words: usize) -> bool {
    word_count(text) <= max_words
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::groq::Groq;

    #[tokio::test]
    async fn fanout_includes_source_and_skips_same_lang() {
        let tr = Translator::new(Groq::new("dummy-key".into(), "openai/gpt-oss-20b".into()));
        // No targets -> just the source text, no network call.
        let m = tr.translate_fanout("ciao", "it", &[], None).await;
        assert_eq!(m.get("it").map(String::as_str), Some("ciao"));
        assert_eq!(m.len(), 1);
        // target == source is skipped (still no network).
        let m2 = tr
            .translate_fanout("ciao", "it", &["it".to_string()], None)
            .await;
        assert_eq!(m2.len(), 1);
    }

    #[test]
    fn word_limit_gates_caching() {
        // Within the bound → eligible (cache I/O allowed).
        assert!(within_cache_word_limit("ciao", 8));
        assert!(within_cache_word_limit("ciao come stai", 8));
        assert!(within_cache_word_limit("", 8)); // 0 words
                                                 // Exactly at the bound is still eligible.
        assert!(within_cache_word_limit("a b c d e f g h", 8));
        // Past the bound (9 words) → bypass the cache entirely (R5).
        assert!(!within_cache_word_limit(
            "uno due tre quattro cinque sei sette otto nove",
            8
        ));
    }

    #[tokio::test]
    async fn new_uses_default_concurrency_cap() {
        let tr = Translator::new(Groq::new("dummy-key".into(), "openai/gpt-oss-20b".into()));
        assert_eq!(tr.max_inflight(), DEFAULT_MAX_INFLIGHT);
        assert_eq!(tr.sem.available_permits(), DEFAULT_MAX_INFLIGHT);
    }

    #[tokio::test]
    async fn with_max_inflight_caps_and_floors_to_one() {
        let tr =
            Translator::with_max_inflight(Groq::new("k".into(), "openai/gpt-oss-20b".into()), 4);
        assert_eq!(tr.max_inflight(), 4);
        assert_eq!(tr.sem.available_permits(), 4);

        // A misconfigured 0 is floored to 1 so the pipeline still makes progress.
        let floored =
            Translator::with_max_inflight(Groq::new("k".into(), "openai/gpt-oss-20b".into()), 0);
        assert_eq!(floored.max_inflight(), 1);
        assert_eq!(floored.sem.available_permits(), 1);
    }

    #[tokio::test]
    async fn fanout_parks_on_admission_semaphore_when_full() {
        let tr = Translator::with_max_inflight(
            Groq::new("dummy-key".into(), "openai/gpt-oss-20b".into()),
            1,
        );
        // Drain the only permit so any fan-out must wait for admission.
        let _held = tr.sem.clone().acquire_owned().await.unwrap();
        assert_eq!(tr.sem.available_permits(), 0);

        // The fan-out task parks on `acquire_owned`, so it never reaches Groq —
        // the future cannot resolve while no permit is free (no network at all).
        let targets = ["en".to_string()];
        let mut fut = Box::pin(tr.translate_fanout("ciao", "it", &targets, None));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut fut)
                .await
                .is_err(),
            "fan-out must wait for an admission permit before calling Groq"
        );
        // Close the semaphore so the parked task ends with an error instead of
        // ever making a (dummy-key) network call as the runtime tears down.
        tr.sem.close();
    }
}
