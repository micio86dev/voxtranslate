//! Shared language↔tier map (spec 0102).
//!
//! The single source of truth lives in `shared/languages.json` at the repo root and is
//! consumed by BOTH this backend (here, embedded at compile time via [`include_str!`]) and
//! the TS frontend (`client/src/scripts/langmap.ts`, a plain JSON import). Keeping one file
//! means the picker's per-tier language filtering and each engine's advertised
//! `output_languages` can never drift.
//!
//! A tier lists exactly the languages its engine can SPEAK. Qwen 3.5 LiveTranslate
//! reaches 60 target languages but only 29 of them with AUDIO; the other 31 are text
//! only. Offering a text-only language on a speech-to-speech tier hands the user
//! subtitles and silence, so `standard` carries the audio-capable set rather than the
//! model's full reach.
//!
//! Invariant (asserted in tests): every tier list is a **superset of the legacy 8**
//! (`it,en,es,fr,de,pt,ja,zh`) so the flag-off `commonLangs` intersection (spec 0094) never
//! shrinks, and `premium` is the full union (the universal-fallback tier). Provider language
//! lists are a verified-against-docs baseline — correct them in the JSON, never in code.

use std::collections::HashMap;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

/// One language in the union, mirroring the JSON entries. The backend only needs the tier
/// lists today; the metadata fields are parsed (and exposed) so tests can assert integrity
/// and a future endpoint could serve them.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Language {
    pub code: String,
    pub native: String,
    pub english: String,
    pub region: String,
    pub rtl: bool,
    pub flag: String,
}

#[derive(Debug, Deserialize)]
struct LanguageMap {
    regions: Vec<String>,
    languages: Vec<Language>,
    tiers: HashMap<String, Vec<String>>,
}

/// The embedded map, parsed once. A malformed `languages.json` is a build-breaking
/// programming error (the file ships with the binary), so `expect` is appropriate here.
/// The JSON lives next to this file (NOT at the repo root) so it's inside the server's
/// Docker build context (`COPY src ./src`); the Railway build only uploads `server/`.
static MAP: LazyLock<LanguageMap> = LazyLock::new(|| {
    const RAW: &str = include_str!("languages.json");
    serde_json::from_str(RAW).expect("languages.json must be valid")
});

/// The OUTPUT-language codes for a tier (`"standard" | "enhanced" | "pro" | "premium"`),
/// in the JSON's declared order. Empty for an unknown tier (caller decides the fallback).
pub fn tier_output_langs(tier: &str) -> Vec<String> {
    MAP.tiers.get(tier).cloned().unwrap_or_default()
}

/// The whole tier → output-languages map, for serving the catalogue to clients that
/// cannot import this file at build time (the Chrome extension lives in its own repo).
/// Handing out the map beats letting each client keep a synced copy that drifts.
pub fn tiers() -> &'static HashMap<String, Vec<String>> {
    &MAP.tiers
}

/// Every language code in the union (the master set the picker offers).
pub fn all_language_codes() -> Vec<String> {
    MAP.languages.iter().map(|l| l.code.clone()).collect()
}

/// The full language metadata list (code/native/english/region/rtl/flag).
pub fn languages() -> &'static [Language] {
    &MAP.languages
}

/// The region grouping order used by the picker.
pub fn regions() -> &'static [String] {
    &MAP.regions
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Languages VoxTranslate has always shipped — every tier must remain a superset so the
    /// flag-off cross-engine intersection (`commonLangs`, spec 0094) never shrinks.
    const LEGACY_8: &[&str] = &["it", "en", "es", "fr", "de", "pt", "ja", "zh"];

    #[test]
    fn map_parses_and_is_non_empty() {
        assert!(!MAP.languages.is_empty());
        assert!(!MAP.regions.is_empty());
        for tier in ["standard", "enhanced", "pro", "premium"] {
            assert!(
                !tier_output_langs(tier).is_empty(),
                "tier {tier} must list languages"
            );
        }
        assert!(tier_output_langs("nope").is_empty());
    }

    #[test]
    fn every_tier_code_exists_and_legacy8_is_subset() {
        let universe: HashSet<&str> = MAP.languages.iter().map(|l| l.code.as_str()).collect();
        for (tier, list) in &MAP.tiers {
            // No dangling codes.
            for code in list {
                assert!(
                    universe.contains(code.as_str()),
                    "tier {tier} references unknown code {code}"
                );
            }
            // No duplicates within a tier.
            let unique: HashSet<&String> = list.iter().collect();
            assert_eq!(unique.len(), list.len(), "tier {tier} has duplicate codes");
            // Legacy 8 always present.
            let set: HashSet<&str> = list.iter().map(String::as_str).collect();
            for code in LEGACY_8 {
                assert!(
                    set.contains(code),
                    "tier {tier} is missing legacy code {code}"
                );
            }
        }
    }

    #[test]
    fn premium_is_the_full_union() {
        let premium: HashSet<&str> = MAP.tiers["premium"].iter().map(String::as_str).collect();
        for lang in &MAP.languages {
            assert!(
                premium.contains(lang.code.as_str()),
                "premium (universal fallback) must cover every language; missing {}",
                lang.code
            );
        }
        assert_eq!(premium.len(), MAP.languages.len());
    }

    #[test]
    fn every_language_has_a_known_region_and_metadata() {
        let regions: HashSet<&str> = MAP.regions.iter().map(String::as_str).collect();
        let mut codes = HashSet::new();
        for l in &MAP.languages {
            assert!(regions.contains(l.region.as_str()), "{} bad region", l.code);
            assert!(!l.native.is_empty() && !l.english.is_empty() && !l.flag.is_empty());
            assert!(
                codes.insert(l.code.as_str()),
                "duplicate language {}",
                l.code
            );
        }
    }
}
