//! Translation fan-out: translate one text into many target languages in
//! parallel, returning a `{ lang: text }` map (including the source language,
//! unchanged). Wraps the Groq client.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Semaphore;

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
}

impl Translator {
    /// Build a translator with the default concurrency cap
    /// ([`DEFAULT_MAX_INFLIGHT`]).
    pub fn new(groq: Groq) -> Self {
        Self::with_max_inflight(groq, DEFAULT_MAX_INFLIGHT)
    }

    /// Build a translator capping concurrent in-flight Groq calls at
    /// `max_inflight` (floored to 1, so a misconfigured `0` still makes progress).
    pub fn with_max_inflight(groq: Groq, max_inflight: usize) -> Self {
        let permits = max_inflight.max(1);
        Self {
            groq,
            sem: Arc::new(Semaphore::new(permits)),
            max_inflight: permits,
        }
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
            let text = text.to_string();
            let src = source_lang.to_string();
            let tgt = tgt.clone();
            let terms = glossary
                .map(|g| g.terms_for(&src, &tgt))
                .unwrap_or_default();
            tasks.push(tokio::spawn(async move {
                // Hold a permit for the whole Groq call → at most `max_inflight`
                // translations run at once process-wide. The semaphore is never
                // closed, so `acquire_owned` only errs on shutdown; treat that as
                // a failed translation (omitted from the map) rather than an
                // unbounded call.
                let translated = match sem.acquire_owned().await {
                    Ok(_permit) => groq.translate(&text, &src, &tgt, &terms).await,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::groq::Groq;

    #[tokio::test]
    async fn fanout_includes_source_and_skips_same_lang() {
        let tr = Translator::new(Groq::new("dummy-key".into()));
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

    #[tokio::test]
    async fn new_uses_default_concurrency_cap() {
        let tr = Translator::new(Groq::new("dummy-key".into()));
        assert_eq!(tr.max_inflight(), DEFAULT_MAX_INFLIGHT);
        assert_eq!(tr.sem.available_permits(), DEFAULT_MAX_INFLIGHT);
    }

    #[tokio::test]
    async fn with_max_inflight_caps_and_floors_to_one() {
        let tr = Translator::with_max_inflight(Groq::new("k".into()), 4);
        assert_eq!(tr.max_inflight(), 4);
        assert_eq!(tr.sem.available_permits(), 4);

        // A misconfigured 0 is floored to 1 so the pipeline still makes progress.
        let floored = Translator::with_max_inflight(Groq::new("k".into()), 0);
        assert_eq!(floored.max_inflight(), 1);
        assert_eq!(floored.sem.available_permits(), 1);
    }

    #[tokio::test]
    async fn fanout_parks_on_admission_semaphore_when_full() {
        let tr = Translator::with_max_inflight(Groq::new("dummy-key".into()), 1);
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
