//! On-demand AI quiz generation (spec 0067, issue #124). A signed-in user gives
//! a free-text prompt; we ask Groq for a small multiple-choice pack, validate it
//! strictly, and charge credits. The pack then plays through the existing
//! host-authoritative quiz engine (spec 0047) — the server only generates it.

use rust_decimal::Decimal;
use serde::Serialize;

use crate::billing::usd;
use crate::config::AiConfig;
use crate::groq::{ChatRequest, Groq};

/// Question-count bounds for one generated round.
pub const MIN_QUESTIONS: usize = 3;
pub const MAX_QUESTIONS: usize = 10;
const DEFAULT_QUESTIONS: usize = 5;
/// Options per question — fixed at 4 to match the quiz UI (4 answer buttons).
const OPTIONS_PER_Q: usize = 4;
/// Hard cap on the prompt accepted from the client.
pub const MAX_PROMPT_CHARS: usize = 400;
/// Generation token budget — enough for ~10 short multiple-choice questions.
const GEN_MAX_TOKENS: u32 = 1400;

/// Clamp a requested count into `[MIN_QUESTIONS, MAX_QUESTIONS]`, defaulting when
/// `None`.
pub fn clamp_count(requested: Option<usize>) -> usize {
    requested
        .unwrap_or(DEFAULT_QUESTIONS)
        .clamp(MIN_QUESTIONS, MAX_QUESTIONS)
}

/// Credits to generate `count` questions in `num_langs` languages: a base plus a
/// per-question rate charged for EACH language the quiz is produced in (the quiz is
/// localized for everyone in the room, so more distinct languages = more work). A
/// single-language room (`num_langs == 1`) costs exactly base + per_question×count,
/// unchanged from before.
pub fn quiz_cost(ai: &AiConfig, count: usize, num_langs: usize) -> Decimal {
    let langs = num_langs.max(1) as u64;
    (usd(ai.quiz_base)
        + usd(ai.quiz_per_question) * Decimal::from(count as u64) * Decimal::from(langs))
    .round_dp(6)
}

/// One validated multiple-choice question. `answer` indexes `options`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuizQuestion {
    pub q: String,
    pub options: Vec<String>,
    pub answer: usize,
}

/// Validate Groq's JSON into a clean pack, or describe why it's unusable. Pure
/// (no network), so it's unit-tested. Rejects anything the quiz UI can't render:
/// a missing/empty stem, not exactly 4 non-empty options, or an out-of-range
/// answer. Trims to `want` questions; needs at least [`MIN_QUESTIONS`].
pub fn validate_questions(
    value: &serde_json::Value,
    want: usize,
) -> Result<Vec<QuizQuestion>, String> {
    let arr = value
        .get("questions")
        .and_then(|q| q.as_array())
        .ok_or("missing `questions` array")?;
    let mut out = Vec::with_capacity(arr.len().min(want));
    for (i, item) in arr.iter().enumerate() {
        let q = item
            .get("q")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("question {i}: empty stem"))?;
        let opts_raw = item
            .get("options")
            .and_then(|v| v.as_array())
            .ok_or_else(|| format!("question {i}: options not an array"))?;
        if opts_raw.len() != OPTIONS_PER_Q {
            return Err(format!(
                "question {i}: need {OPTIONS_PER_Q} options, got {}",
                opts_raw.len()
            ));
        }
        let mut options = Vec::with_capacity(OPTIONS_PER_Q);
        for (j, o) in opts_raw.iter().enumerate() {
            let s = o
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| format!("question {i}: empty option {j}"))?;
            options.push(s.to_string());
        }
        let answer = item
            .get("answer")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| format!("question {i}: missing answer"))? as usize;
        if answer >= OPTIONS_PER_Q {
            return Err(format!("question {i}: answer {answer} out of range"));
        }
        out.push(QuizQuestion {
            q: q.to_string(),
            options,
            answer,
        });
        if out.len() == want {
            break;
        }
    }
    if out.len() < MIN_QUESTIONS {
        return Err(format!(
            "only {} valid question(s) (need {MIN_QUESTIONS})",
            out.len()
        ));
    }
    Ok(out)
}

/// JSON-mode system prompt asking for a strict quiz in `lang`.
fn system_prompt(count: usize, lang: &str) -> String {
    format!(
        "You are a trivia quiz generator. Produce EXACTLY {count} multiple-choice \
         questions about the user's topic, written in the language with code \
         \"{lang}\". Each question must have EXACTLY 4 options and exactly ONE \
         correct answer. Keep questions and options short. Avoid offensive, \
         explicit, or unsafe content. Respond with ONLY a JSON object of the form \
         {{\"questions\":[{{\"q\":\"...\",\"options\":[\"a\",\"b\",\"c\",\"d\"],\"answer\":0}}]}} \
         where \"answer\" is the 0-based index of the correct option."
    )
}

fn gen_request(model: &str, count: usize, lang: &str, prompt: &str) -> ChatRequest {
    let mut req = ChatRequest::new(
        model,
        system_prompt(count, lang),
        format!("Topic: {prompt}"),
    );
    req.json_mode = true;
    req.temperature = 0.7; // a little variety between rounds
    req.max_tokens = GEN_MAX_TOKENS;
    req
}

/// Generate + validate a quiz pack. Returns the questions and the model that
/// produced them. `Err` means nothing usable came back — the caller must NOT
/// charge. Tries the primary model, then the fallback once.
pub async fn generate(
    groq: &Groq,
    ai: &AiConfig,
    prompt: &str,
    count: usize,
    lang: &str,
) -> Result<(Vec<QuizQuestion>, String), String> {
    let primary = ai.report_model.clone();
    let (value, used) = match groq
        .chat_json(gen_request(&primary, count, lang, prompt))
        .await
    {
        Ok(v) => (v, primary),
        Err(e1) => {
            tracing::warn!("quiz primary model failed: {e1}");
            let fallback = ai.fallback_model.clone();
            let v = groq
                .chat_json(gen_request(&fallback, count, lang, prompt))
                .await
                .map_err(|e| format!("groq quiz generation failed: {e}"))?;
            (v, fallback)
        }
    };
    let questions = validate_questions(&value, count)?;
    Ok((questions, used))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack_json(n: usize) -> serde_json::Value {
        let qs: Vec<_> = (0..n)
            .map(|i| {
                serde_json::json!({
                    "q": format!("Question {i}?"),
                    "options": ["a", "b", "c", "d"],
                    "answer": i % 4,
                })
            })
            .collect();
        serde_json::json!({ "questions": qs })
    }

    #[test]
    fn clamp_count_bounds_and_default() {
        assert_eq!(clamp_count(None), DEFAULT_QUESTIONS);
        assert_eq!(clamp_count(Some(0)), MIN_QUESTIONS);
        assert_eq!(clamp_count(Some(1)), MIN_QUESTIONS);
        assert_eq!(clamp_count(Some(7)), 7);
        assert_eq!(clamp_count(Some(999)), MAX_QUESTIONS);
    }

    #[test]
    fn cost_is_base_plus_per_question_per_language() {
        let ai = AiConfig::test_default();
        // Single language: base + per_question * count (unchanged from before).
        assert_eq!(
            quiz_cost(&ai, 5, 1),
            (usd(ai.quiz_base) + usd(ai.quiz_per_question) * Decimal::from(5u64)).round_dp(6)
        );
        // monotonic in count.
        assert!(quiz_cost(&ai, 10, 1) > quiz_cost(&ai, 3, 1));
        // Each extra language adds another per_question*count block.
        assert_eq!(
            quiz_cost(&ai, 5, 3),
            (usd(ai.quiz_base) + usd(ai.quiz_per_question) * Decimal::from(15u64)).round_dp(6)
        );
        assert!(quiz_cost(&ai, 5, 3) > quiz_cost(&ai, 5, 1));
        // 0 languages is floored to 1 (never cheaper than the base single-lang cost).
        assert_eq!(quiz_cost(&ai, 5, 0), quiz_cost(&ai, 5, 1));
    }

    #[test]
    fn validates_a_well_formed_pack_and_trims_to_want() {
        let got = validate_questions(&pack_json(8), 5).unwrap();
        assert_eq!(got.len(), 5, "trimmed to the requested count");
        assert_eq!(got[0].options.len(), 4);
        assert!(got.iter().all(|q| q.answer < 4));
    }

    #[test]
    fn rejects_malformed_packs() {
        // Not an object with `questions`.
        assert!(validate_questions(&serde_json::json!([]), 5).is_err());
        // Too few valid questions.
        assert!(validate_questions(&pack_json(2), 5).is_err());
        // Wrong option count.
        let bad = serde_json::json!({ "questions": [
            {"q":"x","options":["a","b","c"],"answer":0},
            {"q":"y","options":["a","b","c","d"],"answer":0},
            {"q":"z","options":["a","b","c","d"],"answer":0},
        ]});
        assert!(validate_questions(&bad, 3).is_err());
        // Empty stem.
        let bad = serde_json::json!({ "questions": [
            {"q":"  ","options":["a","b","c","d"],"answer":0},
            {"q":"y","options":["a","b","c","d"],"answer":0},
            {"q":"z","options":["a","b","c","d"],"answer":0},
        ]});
        assert!(validate_questions(&bad, 3).is_err());
        // Answer out of range.
        let bad = serde_json::json!({ "questions": [
            {"q":"x","options":["a","b","c","d"],"answer":4},
            {"q":"y","options":["a","b","c","d"],"answer":0},
            {"q":"z","options":["a","b","c","d"],"answer":0},
        ]});
        assert!(validate_questions(&bad, 3).is_err());
    }
}
