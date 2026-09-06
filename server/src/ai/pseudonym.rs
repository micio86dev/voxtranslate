//! Speaker pseudonymisation for third-party AI prompts.
//!
//! The AI features send transcript lines to Groq, and until now they sent them as
//! `RealName: text` — `sentiment.rs` formatted them that way and `correction.rs` passed
//! the names both inside each line and as a dedicated prompt parameter. The models never
//! needed the identity: what they need is to tell one participant apart from another.
//! So the names go out as stable per-analysis labels and come back mapped to the real
//! ones, which keeps every feature identical while removing personal data from the
//! payload entirely.
//!
//! Stable is the operative word. The alias comes from ORDER OF FIRST APPEARANCE, so the
//! same transcript always produces the same labels — reruns are comparable, and a cached
//! or replayed analysis lines up with the one before it.

use std::collections::HashMap;

/// A two-way map between real speaker names and the labels sent to the model.
#[derive(Debug, Clone, Default)]
pub struct SpeakerAliases {
    to_alias: HashMap<String, String>,
    to_real: HashMap<String, String>,
}

impl SpeakerAliases {
    /// Build from names in the order they occur. Duplicates keep their first alias, so
    /// callers can pass a raw event stream without deduplicating first.
    pub fn from_names<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut me = Self::default();
        for name in names {
            me.intern(name.as_ref());
        }
        me
    }

    /// Assign the next label to `real` if it hasn't got one, and return it.
    fn intern(&mut self, real: &str) -> &str {
        if !self.to_alias.contains_key(real) {
            // 1-based: "Speaker 1" reads better in a prompt than "Speaker 0", and the
            // models are used to seeing it that way in diarised transcripts.
            let alias = format!("Speaker {}", self.to_alias.len() + 1);
            self.to_alias.insert(real.to_string(), alias.clone());
            self.to_real.insert(alias, real.to_string());
        }
        &self.to_alias[real]
    }

    /// The label to send for `real`. Falls back to `real` for a name that was never
    /// interned — losing the pseudonym is bad, but dropping a speaker from the analysis
    /// would be worse, and every caller builds the map from the same events it renders.
    pub fn alias<'a>(&'a self, real: &'a str) -> &'a str {
        self.to_alias.get(real).map(String::as_str).unwrap_or(real)
    }

    /// The real name behind a label, or `None` if the model invented one.
    pub fn real(&self, alias: &str) -> Option<&str> {
        self.to_real.get(alias).map(String::as_str)
    }

    /// Every label, in assignment order — for prompts that list the cast up front.
    pub fn aliases(&self) -> Vec<&str> {
        let mut all: Vec<&str> = self.to_real.keys().map(String::as_str).collect();
        all.sort_by_key(|a| a.rsplit(' ').next().and_then(|n| n.parse::<usize>().ok()));
        all
    }

    /// How many distinct speakers were interned.
    pub fn len(&self) -> usize {
        self.to_alias.len()
    }

    pub fn is_empty(&self) -> bool {
        self.to_alias.is_empty()
    }

    /// Rewrite the KEYS of a `{"<speaker>": value}` object back to real names.
    ///
    /// `sentiment` asks the model for a per-speaker score keyed by the label it was
    /// given, so the answer has to be mapped back before it is stored or the UI would
    /// show "Speaker 1". Keys the model invented are dropped rather than passed through:
    /// a hallucinated name is not a participant, and letting it into stored results would
    /// put an unmappable string where the UI expects someone real.
    pub fn restore_keys(&self, obj: &serde_json::Value) -> serde_json::Value {
        let Some(map) = obj.as_object() else {
            return obj.clone();
        };
        let restored: serde_json::Map<String, serde_json::Value> = map
            .iter()
            .filter_map(|(k, v)| self.real(k).map(|r| (r.to_string(), v.clone())))
            .collect();
        serde_json::Value::Object(restored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_follow_order_of_first_appearance() {
        let a = SpeakerAliases::from_names(["Anna", "Bruno", "Anna", "Carla"]);
        assert_eq!(a.alias("Anna"), "Speaker 1");
        assert_eq!(a.alias("Bruno"), "Speaker 2");
        assert_eq!(a.alias("Carla"), "Speaker 3");
        assert_eq!(a.len(), 3, "a repeated name must not take a second label");
    }

    #[test]
    fn the_same_transcript_always_produces_the_same_labels() {
        // Reruns have to be comparable: an analysis repeated on the same events must not
        // shuffle who "Speaker 2" is.
        let first = SpeakerAliases::from_names(["Anna", "Bruno", "Carla"]);
        let again = SpeakerAliases::from_names(["Anna", "Bruno", "Carla"]);
        for n in ["Anna", "Bruno", "Carla"] {
            assert_eq!(first.alias(n), again.alias(n));
        }
    }

    #[test]
    fn round_trips_real_to_alias_and_back() {
        let a = SpeakerAliases::from_names(["Anna", "Bruno"]);
        assert_eq!(a.real(a.alias("Bruno")), Some("Bruno"));
        assert_eq!(a.real("Speaker 9"), None);
    }

    #[test]
    fn an_unknown_name_falls_back_to_itself_rather_than_vanishing() {
        // Losing the pseudonym on a stray name is bad; dropping the speaker from the
        // analysis entirely would be worse.
        let a = SpeakerAliases::from_names(["Anna"]);
        assert_eq!(a.alias("Zoe"), "Zoe");
    }

    #[test]
    fn lists_labels_in_numeric_order_not_string_order() {
        // "Speaker 10" must not sort between 1 and 2 — a prompt that lists the cast
        // out of order invites the model to renumber them.
        let names: Vec<String> = (1..=11).map(|i| format!("P{i}")).collect();
        let a = SpeakerAliases::from_names(&names);
        assert_eq!(a.aliases()[0], "Speaker 1");
        assert_eq!(a.aliases()[1], "Speaker 2");
        assert_eq!(a.aliases()[9], "Speaker 10");
        assert_eq!(a.aliases()[10], "Speaker 11");
    }

    #[test]
    fn restores_per_speaker_keys_from_a_model_answer() {
        let a = SpeakerAliases::from_names(["Anna", "Bruno"]);
        let answer = serde_json::json!({ "Speaker 1": 0.4, "Speaker 2": -0.2 });
        assert_eq!(
            a.restore_keys(&answer),
            serde_json::json!({ "Anna": 0.4, "Bruno": -0.2 })
        );
    }

    #[test]
    fn drops_speaker_keys_the_model_invented() {
        // A hallucinated name is not a participant; storing it would put an unmappable
        // string where the UI expects someone real.
        let a = SpeakerAliases::from_names(["Anna"]);
        let answer = serde_json::json!({ "Speaker 1": 0.5, "Speaker 7": 0.9, "Bob": 0.1 });
        assert_eq!(a.restore_keys(&answer), serde_json::json!({ "Anna": 0.5 }));
    }

    #[test]
    fn a_non_object_answer_passes_through_untouched() {
        let a = SpeakerAliases::from_names(["Anna"]);
        assert_eq!(
            a.restore_keys(&serde_json::json!(null)),
            serde_json::json!(null)
        );
    }

    #[test]
    fn no_real_name_survives_in_the_alias_set() {
        // The whole point: what goes to the provider must not contain the identities.
        let a = SpeakerAliases::from_names(["Alessandro Micelli", "Anna Rossi"]);
        let sent = a.aliases().join(" ");
        assert!(!sent.contains("Alessandro"));
        assert!(!sent.contains("Micelli"));
        assert!(!sent.contains("Anna"));
    }
}
