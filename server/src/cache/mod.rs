//! Translation cache (spec 0107): an opt-in, fail-open DragonflyDB cache for the
//! Standard tier (Deepgram + Groq). Short, frequently-repeated phrases ("ciao",
//! "grazie", "ok perfetto") are returned from DragonflyDB in <1 ms instead of
//! paying the full Groq round-trip again.
//!
//! This module is **pure key logic + a thin Redis client**. The cache is wired
//! into [`crate::translator::Translator`] and is entirely optional: when the
//! feature flag is off, or DragonflyDB is unreachable, the translator simply
//! never holds a [`TranslationCache`] and behaves exactly as before.
//!
//! Key format (no tier prefix — Standard is the only cacheable tier):
//! ```text
//! cache_key = MD5(normalize(text) + "|" + src + "|" + tgt + "|" + glossary_fingerprint)
//! ```
//! See `server/CACHE.md` and `server/CACHE_STRATEGY.md` for the full design.

use redis::aio::ConnectionManager;
use redis::AsyncCommands;

/// Normalize source text before hashing: lowercase, trim, and collapse every run
/// of internal whitespace to a single space. `"  Ciao  Come Stai? " →
/// "ciao come stai?"`. This is what makes "Ciao  come stai" and "ciao come stai"
/// share a cache entry.
pub fn normalize(text: &str) -> String {
    // `split_whitespace` trims and collapses every internal run in one pass; the
    // join restores single spaces, then we lowercase the whole thing.
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Count words in `text` (runs of non-whitespace). Used by the
/// `TRANSLATION_CACHE_MAX_WORDS` filter to bypass the cache for long phrases that
/// are unlikely to recur.
pub fn word_count(text: &str) -> usize {
    text.split_whitespace().count()
}

/// Fingerprint a room's glossary terms for the cache key.
///
/// Returns `""` when there is no active glossary (`None` or empty slice) — so
/// no-glossary rooms produce a stable, structurally pre-glossary key. Otherwise
/// returns `MD5(sorted(terms).join(","))`: terms are **sorted before hashing**,
/// so `["muone","fotone"]` and `["fotone","muone"]` yield the same fingerprint
/// (order-independence), while a different term set yields a different one
/// (no cross-glossary contamination).
pub fn glossary_fingerprint(terms: Option<&[String]>) -> String {
    let terms = match terms {
        Some(t) if !t.is_empty() => t,
        // No glossary (or an empty one): empty fingerprint → canonical no-glossary key.
        _ => return String::new(),
    };
    let mut sorted: Vec<&str> = terms.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    format!("{:x}", md5::compute(sorted.join(",").as_bytes()))
}

/// Build the cache key for one translation:
/// `MD5(normalize(text) + "|" + src + "|" + tgt + "|" + glossary_fp)`.
///
/// `src`/`tgt` give cross-language isolation; `glossary_fp` (see
/// [`glossary_fingerprint`]) gives cross-glossary isolation; `normalize` makes
/// the key case- and whitespace-insensitive. Pass `""` for `glossary_fp` when no
/// glossary is active.
pub fn cache_key(text: &str, src: &str, tgt: &str, glossary_fp: &str) -> String {
    let material = format!("{}|{}|{}|{}", normalize(text), src, tgt, glossary_fp);
    format!("{:x}", md5::compute(material.as_bytes()))
}

/// Thin async wrapper around a DragonflyDB (Redis-compatible) connection.
///
/// Cloneable and cheap to share: the inner [`ConnectionManager`] multiplexes over
/// a single connection and reconnects transparently. Every method is fail-open —
/// a Redis error never propagates as a translation failure; it is logged and the
/// caller falls through to Groq.
#[derive(Clone)]
pub struct TranslationCache {
    manager: ConnectionManager,
}

impl TranslationCache {
    /// Connect to DragonflyDB at `url` (`redis://[:pass@]host:port`). This is the
    /// only fallible step and is surfaced to startup so the caller can `warn` and
    /// continue without a cache (fail-open, spec 0107 R2).
    pub async fn connect(url: &str) -> Result<Self, redis::RedisError> {
        let client = redis::Client::open(url)?;
        let manager = ConnectionManager::new(client).await?;
        Ok(Self { manager })
    }

    /// Look up a cached translation. Returns `None` on miss **or** on any Redis
    /// error (logged at `warn`); the key itself is an MD5 hash, never the
    /// plaintext, so logging it leaks nothing sensitive (spec 0107 R7/R8).
    pub async fn get(&self, key: &str) -> Option<String> {
        let mut conn = self.manager.clone();
        match conn.get::<_, Option<String>>(key).await {
            Ok(hit) => {
                tracing::debug!(key = %key, tier = "standard", hit = hit.is_some(), "translation cache lookup");
                hit
            }
            Err(e) => {
                tracing::warn!("translation cache get failed: {e} — falling through to Groq");
                None
            }
        }
    }

    /// Store a translation under `key` with a `ttl_secs` expiry. Best-effort: the
    /// caller ignores the returned error (logged here), so a write failure never
    /// fails the request (spec 0107 R4/R8).
    pub async fn set(
        &self,
        key: &str,
        value: &str,
        ttl_secs: u64,
    ) -> Result<(), redis::RedisError> {
        let mut conn = self.manager.clone();
        let res = conn.set_ex::<_, _, ()>(key, value, ttl_secs).await;
        if let Err(ref e) = res {
            tracing::warn!("translation cache set failed: {e}");
        }
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_lowercases_trims_and_collapses_whitespace() {
        assert_eq!(normalize("  Ciao Come Stai?  "), "ciao come stai?");
        assert_eq!(normalize("hello   world"), "hello world");
        assert_eq!(normalize(""), "");
        assert_eq!(normalize("UPPER"), "upper");
    }

    #[test]
    fn word_count_counts_whitespace_separated_runs() {
        assert_eq!(word_count(""), 0);
        assert_eq!(word_count("ciao"), 1);
        assert_eq!(word_count("ciao come stai"), 3);
        assert_eq!(word_count("  spaces  here  "), 2);
    }

    #[test]
    fn glossary_fingerprint_empty_for_no_glossary() {
        assert_eq!(glossary_fingerprint(None), "");
        assert_eq!(glossary_fingerprint(Some(&[])), "");
    }

    #[test]
    fn glossary_fingerprint_is_order_independent() {
        let a: Vec<String> = vec!["muone".into(), "fotone".into()];
        let b: Vec<String> = vec!["fotone".into(), "muone".into()];
        let fp_a = glossary_fingerprint(Some(a.as_slice()));
        let fp_b = glossary_fingerprint(Some(b.as_slice()));
        assert_eq!(
            fp_a, fp_b,
            "same terms in a different order must hash the same"
        );
        assert!(!fp_a.is_empty());
    }

    #[test]
    fn glossary_fingerprint_differs_per_term_set() {
        let physics: Vec<String> = vec!["muone".into(), "fotone".into()];
        let marketing: Vec<String> = vec!["marketing".into(), "funnel".into()];
        let fp_physics = glossary_fingerprint(Some(physics.as_slice()));
        let fp_marketing = glossary_fingerprint(Some(marketing.as_slice()));
        assert_ne!(fp_physics, fp_marketing);
    }

    #[test]
    fn cache_key_isolates_glossaries() {
        let physics: Vec<String> = vec!["muone".into(), "fotone".into()];
        let marketing: Vec<String> = vec!["marketing".into(), "funnel".into()];
        let fp_physics = glossary_fingerprint(Some(physics.as_slice()));
        let fp_marketing = glossary_fingerprint(Some(marketing.as_slice()));

        let k_plain = cache_key("ciao come stai", "it", "en", "");
        let k_physics = cache_key("ciao come stai", "it", "en", &fp_physics);
        let k_marketing = cache_key("ciao come stai", "it", "en", &fp_marketing);

        assert_ne!(k_plain, k_physics);
        assert_ne!(k_physics, k_marketing);
        assert_ne!(k_plain, k_marketing);
    }

    #[test]
    fn cache_key_isolates_target_language() {
        let k_en = cache_key("ciao come stai", "it", "en", "");
        let k_fr = cache_key("ciao come stai", "it", "fr", "");
        assert_ne!(k_en, k_fr);
    }

    #[test]
    fn cache_key_normalizes_text_before_hashing() {
        assert_eq!(
            cache_key("CIAO COME STAI", "it", "en", ""),
            cache_key("ciao come stai", "it", "en", ""),
            "cache_key must normalize text before hashing"
        );
    }

    #[test]
    fn cache_key_same_glossary_any_order_same_key() {
        let a: Vec<String> = vec!["muone".into(), "fotone".into()];
        let b: Vec<String> = vec!["fotone".into(), "muone".into()];
        let fp_a = glossary_fingerprint(Some(a.as_slice()));
        let fp_b = glossary_fingerprint(Some(b.as_slice()));
        assert_eq!(
            cache_key("ciao come stai", "it", "en", &fp_a),
            cache_key("ciao come stai", "it", "en", &fp_b),
        );
    }
}
