//! Which way is this utterance going? (spec 0110)
//!
//! Talk to Anyone puts two people on ONE microphone, so before a single word can be
//! spoken aloud we have to know which of the two configured languages was just spoken.
//! Nothing upstream can tell us:
//!
//! * Qwen takes one optional `input_audio_transcription.language` string and emits no
//!   detection event ([`crate::engine::qwen::session_update_json`]);
//! * OpenAI's translate session has no source-language field at all;
//! * Gemini's `translationConfig` carries only the target;
//! * Deepgram's `detect_language` lives on the REST path, which no live tier uses.
//!
//! **No provider accepts a candidate list.** So detection is ours, and it runs on the
//! ORIGINAL transcript every engine already returns — which is why one implementation
//! covers Standard, Pro and Premium identically.
//!
//! Three stages, cheapest first, and every one of them is allowed to abstain:
//!
//! 1. [`MIN_RESOLVE_CHARS`] — two syllables are not evidence.
//! 2. [`script_hint`] — pure Unicode-range scoring. When the pair uses disjoint scripts
//!    (it↔ja, en↔ar, fr↔zh — most travel pairs) this decides at zero cost and zero
//!    latency, and no network call happens at all.
//! 3. [`Resolver::classify`] — one Groq JSON call constrained to the two candidates.
//!
//! Abstention is a first-class answer. [`Direction::Unknown`] means "hold the audio and
//! keep listening", never "translate anyway": a wrong direction speaks the wrong
//! language out loud at a stranger, which is worse than a beat of silence.

use serde::Deserialize;

use crate::groq::{ChatRequest, Groq};

/// Shortest transcript worth sending to the CLASSIFIER. Below this even a human cannot
/// tell "Ciao" from "Chao", and a wrong commit poisons the whole utterance (the direction
/// is latched once committed — see [`super::utterance`]).
///
/// This gates the model call only. [`script_hint`] has its own, much lower floor because
/// it measures evidence rather than length: five kana are decisive where five Latin
/// letters are not.
pub const MIN_RESOLVE_CHARS: usize = 12;

/// Discriminating characters [`script_hint`] needs before it will decide. Low, because a
/// character that belongs to exactly one of the two candidates is strong evidence — but
/// not one or two, which a single loanword or place name could supply.
pub const MIN_SCRIPT_EVIDENCE: usize = 4;

/// Minimum model confidence that commits a direction. Below it we abstain and wait for
/// more speech rather than guess. Deliberately not user-visible (brief §16).
pub const MIN_CONFIDENCE: f32 = 0.6;

/// Longest transcript sent to the classifier. A clause is plenty and it bounds both cost
/// and latency on a long monologue.
pub const MAX_CLASSIFY_CHARS: usize = 400;

/// Token budget for one classification.
///
/// This is NOT a cost knob, it is a correctness one. `GROQ_TRANSLATION_MODEL` is a
/// gpt-oss **reasoning** model: it spends tokens thinking before it emits anything, so a
/// budget sized for the ~15-token answer is consumed entirely by reasoning and the
/// completion comes back EMPTY — which `response_format: json_object` then rejects with
/// `400 json_validate_failed`. That shipped: at 40 tokens every single classification
/// failed in production, no direction was ever committed, and because an unresolved
/// utterance is held by design the feature looked like it was permanently "Listening…".
///
/// Measured: 40 → always fails. 128 → correct. 256 gives margin for a longer clause.
/// `Groq::chat` documents the same hazard at its empty-completion retry.
pub const CLASSIFY_MAX_TOKENS: u32 = 256;

/// Floor the regression test holds the budget above.
pub const MIN_CLASSIFY_TOKENS: u32 = 128;

/// How sure [`script_hint`] must be before it decides alone: the winning script must
/// carry this share of the strongly-scripted characters.
const SCRIPT_MARGIN: f32 = 0.8;

/// Which way one utterance is going, in room terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// The signed-in user spoke; translate into the other person's language.
    UserToOther,
    /// The other person spoke; translate into the user's language.
    OtherToUser,
    /// Not enough evidence, or a language that is neither of the two. Hold.
    Unknown,
}

impl Direction {
    /// The language the translated speech should come out in, or `None` while unknown.
    pub fn target<'a>(&self, user_lang: &'a str, other_lang: &'a str) -> Option<&'a str> {
        match self {
            Direction::UserToOther => Some(other_lang),
            Direction::OtherToUser => Some(user_lang),
            Direction::Unknown => None,
        }
    }

    /// The language that was SPOKEN, or `None` while unknown. This is the one the UI
    /// shows as "🇮🇹 Italian detected", and the one whose output must be suppressed
    /// (a source→source session only echoes the speaker back at themselves).
    pub fn source<'a>(&self, user_lang: &'a str, other_lang: &'a str) -> Option<&'a str> {
        match self {
            Direction::UserToOther => Some(user_lang),
            Direction::OtherToUser => Some(other_lang),
            Direction::Unknown => None,
        }
    }

    /// Build from a spoken-language code. Anything that is neither configured language —
    /// a third language, a hallucinated code — is [`Direction::Unknown`], never a
    /// translation (brief §6).
    pub fn from_spoken(spoken: &str, user_lang: &str, other_lang: &str) -> Self {
        let spoken = base_code(spoken);
        if spoken == base_code(user_lang) {
            Direction::UserToOther
        } else if spoken == base_code(other_lang) {
            Direction::OtherToUser
        } else {
            Direction::Unknown
        }
    }
}

/// Compare on the base subtag so `pt-br` and `pt` are the same language. Regional
/// variants are a rendering detail; they are never two sides of a conversation.
fn base_code(code: &str) -> &str {
    let code = code.trim();
    match code.find('-') {
        Some(i) => &code[..i],
        None => code,
    }
}

/// The writing systems we can tell apart from code points alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Script {
    Latin,
    Cyrillic,
    Greek,
    Hebrew,
    Arabic,
    Devanagari,
    Han,
    Kana,
    Hangul,
    Thai,
    Georgian,
    Armenian,
}

/// The script of one character, or `None` for digits, spaces and punctuation — which
/// carry no language signal and must not dilute the vote.
fn char_script(c: char) -> Option<Script> {
    match c as u32 {
        0x0041..=0x005A | 0x0061..=0x007A | 0x00C0..=0x024F => Some(Script::Latin),
        0x0370..=0x03FF | 0x1F00..=0x1FFF => Some(Script::Greek),
        // Cyrillic + Cyrillic Supplement.
        0x0400..=0x052F => Some(Script::Cyrillic),
        0x0530..=0x058F => Some(Script::Armenian),
        0x0590..=0x05FF => Some(Script::Hebrew),
        0x0600..=0x06FF | 0x0750..=0x077F | 0x08A0..=0x08FF => Some(Script::Arabic),
        0x0900..=0x097F => Some(Script::Devanagari),
        0x0E00..=0x0E7F => Some(Script::Thai),
        0x10A0..=0x10FF | 0x1C90..=0x1CBF => Some(Script::Georgian),
        // Kana before Han: Japanese text mixes both, and the kana are what distinguish
        // it from Chinese. A single hiragana settles ja-vs-zh on its own.
        0x3040..=0x30FF => Some(Script::Kana),
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF => Some(Script::Han),
        0xAC00..=0xD7AF | 0x1100..=0x11FF | 0x3130..=0x318F => Some(Script::Hangul),
        _ => None,
    }
}

/// The scripts a language is legitimately written in, or `None` when we do not model it
/// (which simply means the hint abstains and the classifier decides).
///
/// Japanese is the reason this returns a SET rather than one script: it is written in
/// kana *and* the same Han characters as Chinese. Treating Han as evidence for Chinese
/// would misroute a kanji-heavy Japanese sentence with total confidence.
fn lang_scripts(code: &str) -> Option<&'static [Script]> {
    Some(match base_code(code) {
        "ja" => &[Script::Kana, Script::Han],
        "zh" | "yue" => &[Script::Han],
        // Hanja is rare in modern Korean but not absent; sharing Han with the others
        // costs nothing and stops a stray character deciding a conversation.
        "ko" => &[Script::Hangul, Script::Han],
        "th" | "lo" => &[Script::Thai],
        "he" | "yi" => &[Script::Hebrew],
        "ar" | "fa" | "ur" | "ps" | "ckb" => &[Script::Arabic],
        "hi" | "mr" | "ne" => &[Script::Devanagari],
        "el" => &[Script::Greek],
        "ru" | "uk" | "be" | "bg" | "sr" | "mk" | "kk" | "mn" => &[Script::Cyrillic],
        "ka" => &[Script::Georgian],
        "hy" => &[Script::Armenian],
        // Everything else in the union is Latin-scripted. Two Latin languages can never
        // be separated this way — which is exactly what the hint abstains on.
        "en" | "it" | "es" | "fr" | "de" | "pt" | "nl" | "pl" | "tr" | "id" | "ms" | "vi"
        | "sv" | "da" | "nb" | "nn" | "fi" | "cs" | "sk" | "sl" | "hr" | "bs" | "ro" | "hu"
        | "et" | "lv" | "lt" | "sq" | "is" | "ga" | "cy" | "mt" | "eu" | "ca" | "gl" | "af"
        | "sw" | "so" | "ha" | "ig" | "yo" | "zu" | "az" | "uz" | "fil" | "ht" | "lb" => {
            &[Script::Latin]
        }
        _ => return None,
    })
}

/// Decide from writing system alone, or abstain.
///
/// Only characters that belong to exactly ONE of the two candidates count as evidence.
/// A script both languages share (Han in a ja↔zh conversation) is skipped entirely
/// rather than credited to either side — that is the difference between "I cannot tell"
/// and a confident wrong answer.
///
/// Abstains unless there are at least [`MIN_SCRIPT_EVIDENCE`] discriminating characters
/// and one side holds [`SCRIPT_MARGIN`] of them. Note there is no *text length* gate
/// here: a five-character Japanese sentence is far more evidence than five Latin
/// letters, so the floor is counted in evidence, not in characters.
pub fn script_hint(text: &str, user_lang: &str, other_lang: &str) -> Direction {
    let (Some(user_scripts), Some(other_scripts)) =
        (lang_scripts(user_lang), lang_scripts(other_lang))
    else {
        return Direction::Unknown;
    };

    let mut user_hits = 0usize;
    let mut other_hits = 0usize;
    for c in text.chars() {
        let Some(s) = char_script(c) else { continue };
        let in_user = user_scripts.contains(&s);
        let in_other = other_scripts.contains(&s);
        // Shared or foreign scripts are not evidence for either side.
        match (in_user, in_other) {
            (true, false) => user_hits += 1,
            (false, true) => other_hits += 1,
            _ => {}
        }
    }

    let evidence = user_hits + other_hits;
    if evidence < MIN_SCRIPT_EVIDENCE {
        return Direction::Unknown;
    }
    let evidence = evidence as f32;
    if user_hits as f32 / evidence >= SCRIPT_MARGIN {
        Direction::UserToOther
    } else if other_hits as f32 / evidence >= SCRIPT_MARGIN {
        Direction::OtherToUser
    } else {
        Direction::Unknown
    }
}

/// What the classifier is asked to return. `lang` is one of the two codes or the literal
/// `"other"`, which is how a third language is rejected rather than forced into the pair.
#[derive(Debug, Deserialize)]
struct Verdict {
    #[serde(default)]
    lang: String,
    #[serde(default)]
    confidence: f32,
}

/// The system prompt. Two candidates, one JSON object, no prose. It names the languages
/// in English via [`crate::groq::lang_name`] so the model is not guessing at bare codes,
/// and it must mention JSON — a Groq requirement for `response_format: json_object`.
fn classify_prompt(user_lang: &str, other_lang: &str) -> String {
    let a = crate::groq::lang_name(user_lang);
    let b = crate::groq::lang_name(other_lang);
    format!(
        "You identify the language of a short speech transcript. It is almost always \
         either {a} (code \"{ua}\") or {b} (code \"{ob}\"). \
         Reply with a single JSON object and nothing else: \
         {{\"lang\": \"{ua}\" | \"{ob}\" | \"other\", \"confidence\": 0.0-1.0}}. \
         Rules: (1) answer \"other\" if the text is clearly in some THIRD language. \
         (2) answer \"other\" if the text is too short, garbled, or is only names, \
         numbers or noise. (3) confidence is your honest probability, not a formality — \
         use a low value when the two languages are hard to tell apart here. \
         (4) never explain, never add fields.",
        a = a,
        b = b,
        ua = base_code(user_lang),
        ob = base_code(other_lang),
    )
}

/// Two-candidate language identification for a conversation.
///
/// Cheap to clone (`Groq` is `reqwest`-backed) so one lives per session.
#[derive(Clone)]
pub struct Resolver {
    groq: Groq,
    model: String,
    user_lang: String,
    other_lang: String,
}

impl Resolver {
    pub fn new(groq: Groq, model: String, user_lang: String, other_lang: String) -> Self {
        Self {
            groq,
            model,
            user_lang,
            other_lang,
        }
    }

    pub fn user_lang(&self) -> &str {
        &self.user_lang
    }

    pub fn other_lang(&self) -> &str {
        &self.other_lang
    }

    /// Resolve `text` without a network call, or return [`Direction::Unknown`] to say
    /// "ask the model". Split out from [`Self::resolve`] so the free path is unit-tested
    /// on its own and so the caller can skip spawning entirely when this decides.
    pub fn resolve_local(&self, text: &str) -> Direction {
        script_hint(text, &self.user_lang, &self.other_lang)
    }

    /// Full resolution: local first, model second.
    ///
    /// `Err` is a provider failure, which the caller must surface rather than absorb —
    /// holding every utterance behind a dead classifier is indistinguishable from the
    /// app being broken, because it is.
    pub async fn resolve(&self, text: &str) -> Result<Direction, String> {
        let local = self.resolve_local(text);
        if local != Direction::Unknown {
            return Ok(local);
        }
        if text.trim().chars().count() < MIN_RESOLVE_CHARS {
            return Ok(Direction::Unknown);
        }
        self.classify(text).await
    }

    /// Build the classification request. Separated from the HTTP call so the token
    /// budget is unit-testable — see [`MIN_CLASSIFY_TOKENS`].
    fn classify_request(&self, text: &str) -> ChatRequest {
        let clipped: String = text.trim().chars().take(MAX_CLASSIFY_CHARS).collect();
        let mut req = ChatRequest::new(
            self.model.clone(),
            classify_prompt(&self.user_lang, &self.other_lang),
            clipped,
        );
        req.temperature = 0.0;
        req.max_tokens = CLASSIFY_MAX_TOKENS;
        // Measured at ~240 ms against gpt-oss-20b; 6 s is generous headroom for a bad
        // network without stalling a conversation.
        req.timeout = std::time::Duration::from_secs(6);
        // One retry, deliberately. `Groq::chat` treats an empty completion as transient
        // and retries it — that is the whole reason the retry budget exists, and setting
        // it to zero threw the protection away.
        req.max_retries = 1;
        req
    }

    /// Ask the model. `Err` means the PROVIDER failed (network, 4xx, unparseable) as
    /// opposed to the model legitimately abstaining, which is `Ok(Unknown)`.
    ///
    /// The distinction is not academic: an abstention is normal and silent, while a
    /// provider failure means every utterance will keep being held with the UI saying
    /// "Listening…" forever. The caller needs to be able to tell the user.
    async fn classify(&self, text: &str) -> Result<Direction, String> {
        let value = self
            .groq
            .chat_json(self.classify_request(text))
            .await
            .map_err(|e| {
                tracing::warn!("talk: direction classify failed: {e}");
                e
            })?;
        let verdict: Verdict = serde_json::from_value(value).map_err(|e| {
            tracing::warn!("talk: direction verdict unparseable: {e}");
            e.to_string()
        })?;
        Ok(interpret(
            &verdict.lang,
            verdict.confidence,
            &self.user_lang,
            &self.other_lang,
        ))
    }
}

/// Turn a raw verdict into a direction, applying the confidence floor and the
/// third-language rule. Pure, so both are tested without a network.
fn interpret(lang: &str, confidence: f32, user_lang: &str, other_lang: &str) -> Direction {
    let lang = lang.trim().to_lowercase();
    if lang.is_empty() || lang == "other" {
        return Direction::Unknown;
    }
    if !(MIN_CONFIDENCE..=1.0).contains(&confidence) {
        // Below the floor, or a nonsense value (negative, >1, NaN): abstain. A model that
        // cannot express calibrated doubt is not evidence.
        tracing::debug!(%lang, confidence, "talk: direction below confidence floor");
        return Direction::Unknown;
    }
    Direction::from_spoken(&lang, user_lang, other_lang)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_third_language_is_never_translated() {
        // The brief's hardest rule (§6): only the two configured languages may be
        // spoken aloud. German in an it/es conversation must be silence, not a guess.
        assert_eq!(Direction::from_spoken("de", "it", "es"), Direction::Unknown);
        assert_eq!(interpret("other", 0.99, "it", "es"), Direction::Unknown);
        assert_eq!(interpret("de", 0.99, "it", "es"), Direction::Unknown);
    }

    #[test]
    fn regional_variants_are_the_same_language() {
        // pt-br and pt are one side of the conversation, not two.
        assert_eq!(
            Direction::from_spoken("pt-br", "pt", "en"),
            Direction::UserToOther
        );
        assert_eq!(
            Direction::from_spoken("en", "pt-br", "en-us"),
            Direction::OtherToUser
        );
    }

    #[test]
    fn confidence_floor_abstains_instead_of_guessing() {
        assert_eq!(interpret("it", 0.59, "it", "es"), Direction::Unknown);
        assert_eq!(interpret("it", 0.60, "it", "es"), Direction::UserToOther);
        // Nonsense confidences are treated as no evidence, not as certainty.
        assert_eq!(interpret("it", -1.0, "it", "es"), Direction::Unknown);
        assert_eq!(interpret("it", 2.0, "it", "es"), Direction::Unknown);
        assert_eq!(interpret("it", f32::NAN, "it", "es"), Direction::Unknown);
        assert_eq!(interpret("", 0.99, "it", "es"), Direction::Unknown);
    }

    #[test]
    fn script_hint_decides_disjoint_scripts_for_free() {
        // Japanese vs Italian: one kana settles it, no network call.
        assert_eq!(
            script_hint("駅に行きたいのですが", "it", "ja"),
            Direction::OtherToUser
        );
        assert_eq!(
            script_hint("Vorrei andare alla stazione", "it", "ja"),
            Direction::UserToOther
        );
        // Arabic vs English, Cyrillic vs French, Hangul vs Spanish.
        assert_eq!(
            script_hint("أين محطة القطار", "en", "ar"),
            Direction::OtherToUser
        );
        assert_eq!(
            script_hint("Где вокзал", "fr", "ru"),
            Direction::OtherToUser
        );
        assert_eq!(
            script_hint("역이 어디에 있나요", "es", "ko"),
            Direction::OtherToUser
        );
    }

    #[test]
    fn a_script_both_languages_share_is_never_evidence() {
        // ja and zh both use Han. Crediting Han to Chinese would route a kanji-heavy
        // Japanese sentence into Chinese with total confidence — a confident wrong
        // answer, which is the one outcome worse than silence.
        assert_eq!(script_hint("東京駅前", "ja", "zh"), Direction::Unknown);
        assert_eq!(script_hint("車站在哪裡", "ja", "zh"), Direction::Unknown);

        // Kana belong to ja alone, so they decide even surrounded by kanji.
        assert_eq!(
            script_hint("駅はどこですか", "ja", "zh"),
            Direction::UserToOther
        );

        // Same rule for the other Han-sharing pairs.
        assert_eq!(
            script_hint("어디에 있나요", "ko", "zh"),
            Direction::UserToOther
        );
        assert_eq!(
            script_hint("どこですか", "ja", "ko"),
            Direction::UserToOther
        );
    }

    #[test]
    fn a_shared_script_pair_falls_through_to_the_classifier() {
        // Against a Latin language, Han IS discriminating — nothing else in the pair
        // uses it — so the same characters that abstain above now decide.
        assert_eq!(
            script_hint("車站在哪裡", "it", "zh"),
            Direction::OtherToUser
        );
        assert_eq!(script_hint("東京駅前", "it", "ja"), Direction::OtherToUser);
    }

    #[test]
    fn script_hint_abstains_on_two_latin_languages() {
        // The common travel case. There is no free answer here — that is what the
        // classifier is for, and the hint must not invent one.
        assert_eq!(
            script_hint("Vorrei andare alla stazione", "it", "es"),
            Direction::Unknown
        );
        assert_eq!(
            script_hint("Quiero ir a la estación", "it", "es"),
            Direction::Unknown
        );
    }

    #[test]
    fn script_hint_abstains_without_enough_evidence() {
        // Digits and punctuation carry no script; a text of only those must abstain
        // rather than divide by zero or fall through to the first candidate.
        assert_eq!(
            script_hint("12:45 — 3,50 €", "it", "ja"),
            Direction::Unknown
        );
        assert_eq!(script_hint("", "it", "ja"), Direction::Unknown);
        // One or two characters are not evidence — a stray loanword ("OK" mid-sentence)
        // must not swing a whole utterance.
        assert_eq!(script_hint("はい", "it", "ja"), Direction::Unknown);
        assert_eq!(script_hint("OK", "it", "ja"), Direction::Unknown);
        // Four IS enough when the scripts are disjoint, and that is the point: against a
        // Japanese candidate, four Latin letters are conclusive (Japanese speech is not
        // transcribed in Latin script), so a one-word greeting is translated instantly
        // instead of waiting for a classifier that has nothing left to add.
        assert_eq!(script_hint("Ciao", "it", "ja"), Direction::UserToOther);
        // An unmodelled code abstains instead of being assumed Latin.
        assert_eq!(script_hint("hello there", "en", "xx"), Direction::Unknown);
    }

    #[test]
    fn a_mixed_sentence_abstains_rather_than_picking_the_majority() {
        // Someone reading a Latin brand name mid-Japanese, or the reverse. Neither side
        // clears the margin, so the classifier gets to look at the words.
        assert_eq!(
            script_hint("駅 the station です", "en", "ja"),
            Direction::Unknown
        );
    }

    #[test]
    fn the_free_path_measures_evidence_not_characters() {
        let r = Resolver::new(
            Groq::new("k".into(), "m".into()),
            "m".into(),
            "it".into(),
            "ja".into(),
        );
        // Ten Latin characters are not enough to separate two Latin languages — but this
        // pair is not Latin-vs-Latin, and "Vorrei un caffè" is decisively Latin here.
        assert_eq!(r.resolve_local("Vorrei un caffè"), Direction::UserToOther);
        // A short Japanese sentence is DENSE: nine characters of it settle the question,
        // where nine Latin letters would not. A character-count floor would have thrown
        // this away and paid for a model call instead.
        assert_eq!(
            r.resolve_local("駅に行きたいのですが"),
            Direction::OtherToUser
        );
        // Two kana are still not evidence.
        assert_eq!(r.resolve_local("はい"), Direction::Unknown);
    }

    #[test]
    fn target_and_source_are_opposite_ends() {
        let (u, o) = ("it", "es");
        assert_eq!(Direction::UserToOther.target(u, o), Some("es"));
        assert_eq!(Direction::UserToOther.source(u, o), Some("it"));
        assert_eq!(Direction::OtherToUser.target(u, o), Some("it"));
        assert_eq!(Direction::OtherToUser.source(u, o), Some("es"));
        // Unknown has neither — the caller must hold, and cannot accidentally default
        // to one end by unwrapping.
        assert_eq!(Direction::Unknown.target(u, o), None);
        assert_eq!(Direction::Unknown.source(u, o), None);
    }

    /// The regression that took the feature down in production the day it shipped.
    #[test]
    fn the_token_budget_leaves_room_for_a_reasoning_model_to_answer() {
        // gpt-oss reasons before it writes. A budget sized for the ~15-token answer is
        // eaten entirely by that reasoning, the completion comes back empty, and JSON
        // mode turns it into a 400 — so EVERY classification fails, no direction is ever
        // committed, and the UI sits on "Listening…" forever. Measured against the live
        // model: 40 tokens always fails, 128 always succeeds.
        let r = Resolver::new(
            Groq::new("k".into(), "m".into()),
            "m".into(),
            "it".into(),
            "es".into(),
        );
        let req = r.classify_request("Vorrei andare alla stazione");
        assert!(
            req.max_tokens >= MIN_CLASSIFY_TOKENS,
            "a reasoning model needs headroom BEYOND the answer it must write; {} is \
             below the measured floor of {MIN_CLASSIFY_TOKENS}",
            req.max_tokens
        );
        // Deterministic, and retried once: `Groq::chat` absorbs a transient empty
        // completion only if it has a retry left to spend.
        assert_eq!(req.temperature, 0.0);
        assert!(
            req.max_retries >= 1,
            "the empty-completion retry must stay available"
        );
    }

    #[test]
    fn a_long_utterance_is_clipped_before_it_is_sent() {
        let r = Resolver::new(
            Groq::new("k".into(), "m".into()),
            "m".into(),
            "it".into(),
            "es".into(),
        );
        let req = r.classify_request(&"a".repeat(MAX_CLASSIFY_CHARS * 3));
        assert_eq!(req.user.chars().count(), MAX_CLASSIFY_CHARS);
    }

    #[test]
    fn classify_prompt_names_both_languages_and_asks_for_json() {
        let p = classify_prompt("it", "es");
        assert!(p.contains("Italian") && p.contains("Spanish"));
        assert!(p.contains("\"it\"") && p.contains("\"es\""));
        // Groq's json_object mode rejects a prompt that never mentions JSON.
        assert!(p.contains("JSON"));
        // The escape hatch for a third language must be offered explicitly.
        assert!(p.contains("other"));
    }
}
