//! AI session report (spec 0014): turn a call transcript into a structured or
//! freeform Markdown report via Groq, billed per call duration.

use std::time::Duration;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use uuid::Uuid;

use super::{billed_minutes, condense_transcript, transcript_to_text};
use crate::billing::usd;
use crate::config::AiConfig;
use crate::db::Pool;
use crate::groq::{ChatRequest, Groq};
use crate::transcripts::TranscriptExport;

/// User-facing cost: `CREDITS_REPORT_BASE + CREDITS_REPORT_PER_MINUTE × ⌈min⌉`.
pub fn report_cost(ai: &AiConfig, duration_seconds: i64) -> Decimal {
    (usd(ai.report_base)
        + usd(ai.report_per_minute) * Decimal::from(billed_minutes(duration_seconds)))
    .round_dp(6)
}

/// Build the report system prompt. Pure, for tests. User guidelines are passed
/// as data with explicit precedence rules, never interpolated into directives.
pub fn report_prompt(
    format: &str,
    lang: &str,
    guidelines: Option<&str>,
    has_bookmarks: bool,
) -> String {
    let mut system = String::from(
        "You write post-meeting reports from call transcripts. The transcript lines are \
         `[HH:MM:SS] Speaker (lang): text`; `[chat]` marks text-chat messages. \
         Participants may speak different languages.\n",
    );
    match format {
        "freeform" => system.push_str(
            "Write a flowing prose report (Markdown, a few short sections at your \
             discretion) that captures what the meeting was about, what was agreed, \
             and what happens next.\n",
        ),
        _ => {
            // The section names below are the canonical English labels the model
            // must map to. It MUST translate each heading into the report
            // language — otherwise it keeps the English labels verbatim while
            // writing the body in the target language, producing a mixed-language
            // report (#223). The localization instruction is restated after the
            // section list and reinforced by the whole-report language line below.
            system.push_str(
                "Write a structured Markdown report with exactly these sections, in this order, \
                 as `##` headings — but TRANSLATE every heading into the report language (never \
                 keep the English label): Executive Summary, Key Points, Decisions, Action Items",
            );
            if has_bookmarks {
                system.push_str(", Bookmarked Highlights (cover each BOOKMARKS entry)");
            }
            system.push_str(
                ", Open Questions. Use bullet lists inside sections; for an empty section write \
                 the report language's equivalent of \"None.\" (not the English word).\n",
            );
        }
    }
    system.push_str(&format!(
        "Write the ENTIRE report — every section heading, label, and body line — in {0}. \
         Do not leave any heading or word in English unless {0} is English. \
         Output ONLY the Markdown report — no preamble.\n",
        crate::groq::lang_name(lang),
    ));
    if let Some(g) = guidelines {
        system.push_str(&format!(
            "\nThe requester added these guidelines (apply them where they don't conflict \
             with the rules above):\n{g}\n"
        ));
    }
    system
}

/// Generate the report Markdown. Returns `(markdown, model_used)`.
///
/// Long transcripts are condensed first (map-reduce, fallback model). The
/// final call runs on `GROQ_REPORT_MODEL` with 3 retries on 429; a 4xx from
/// the primary model (decommissioned/unknown model id) retries once on the
/// fallback model so a Groq model retirement degrades instead of breaking.
pub async fn generate_report(
    groq: &Groq,
    ai: &AiConfig,
    export: &TranscriptExport,
    format: &str,
    lang: &str,
    guidelines: Option<&str>,
) -> Result<(String, String), String> {
    let text = condense_transcript(groq, ai, transcript_to_text(export)).await?;
    let system = report_prompt(format, lang, guidelines, !export.bookmarks.is_empty());

    let mut req = ChatRequest::new(ai.report_model.clone(), system, text);
    req.max_tokens = 2048;
    req.temperature = 0.3;
    req.timeout = Duration::from_secs(30);
    req.max_retries = 3;

    let fallback_req = ChatRequest {
        model: ai.fallback_model.clone(),
        ..req.clone()
    };
    match groq.chat(req).await {
        Ok(md) => Ok((md, ai.report_model.clone())),
        // 4xx = the model id itself was rejected; other failures (timeout,
        // network, 5xx) are not the model's fault and retrying would double
        // the wait for the same outcome.
        Err(e) if e.contains("groq returned 4") && ai.fallback_model != ai.report_model => {
            tracing::warn!("report model failed ({e}); retrying on fallback model");
            groq.chat(fallback_req)
                .await
                .map(|md| (md, ai.fallback_model.clone()))
        }
        Err(e) => Err(e),
    }
}

/// One stored report, as returned by the REST endpoints.
#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct ReportRow {
    pub id: Uuid,
    pub format: String,
    pub lang: String,
    pub guidelines: Option<String>,
    pub markdown: String,
    pub model: String,
    pub cost: Decimal,
    pub created_at: DateTime<Utc>,
}

/// Persist a generated report; multiple per session (regenerate keeps history).
#[allow(clippy::too_many_arguments)]
pub async fn save_report(
    pool: &Pool,
    session_id: Uuid,
    user_id: Uuid,
    format: &str,
    lang: &str,
    guidelines: Option<&str>,
    markdown: &str,
    model: &str,
    cost: Decimal,
) -> Result<ReportRow, sqlx::Error> {
    sqlx::query_as(
        "INSERT INTO session_reports
             (session_id, user_id, guidelines, format, lang, markdown, model, cost)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         RETURNING id, format, lang, guidelines, markdown, model, cost, created_at",
    )
    .bind(session_id)
    .bind(user_id)
    .bind(guidelines)
    .bind(format)
    .bind(lang)
    .bind(markdown)
    .bind(model)
    .bind(cost)
    .fetch_one(pool)
    .await
}

/// Latest report for a session (any participant can read it).
pub async fn latest_report(
    pool: &Pool,
    session_id: Uuid,
) -> Result<Option<ReportRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, format, lang, guidelines, markdown, model, cost, created_at
         FROM session_reports WHERE session_id = $1
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn ai() -> AiConfig {
        Config::test_with_billing("postgres://x", "s", 2.0)
            .billing
            .unwrap()
            .ai
    }

    #[test]
    fn report_cost_is_base_plus_ceiled_minutes() {
        let ai = ai(); // base 0.05, per_minute 0.002
        assert_eq!(report_cost(&ai, 0), usd(0.052)); // floors at 1 minute
        assert_eq!(report_cost(&ai, 60), usd(0.052));
        assert_eq!(report_cost(&ai, 61), usd(0.054)); // 2 minutes
        assert_eq!(report_cost(&ai, 3600), usd(0.05 + 0.002 * 60.0));
    }

    #[test]
    fn report_prompt_structured_sections_and_language() {
        let p = report_prompt("structured", "it", None, true);
        assert!(p.contains("Executive Summary"));
        assert!(p.contains("Bookmarked Highlights"));
        assert!(p.contains("Italian"));
        assert!(!p.contains("guidelines"));
        // #223: headings must be localized, not kept in English. The prompt must
        // explicitly instruct translating every heading into the report language.
        assert!(p.contains("TRANSLATE every heading"));
        assert!(p.contains("every section heading, label, and body line"));

        // No bookmarks → the section isn't requested (the model would invent it).
        let none = report_prompt("structured", "en", None, false);
        assert!(!none.contains("Bookmarked Highlights"));
    }

    #[test]
    fn report_prompt_freeform_and_guidelines() {
        let p = report_prompt("freeform", "en", Some("focus on budget talk"), false);
        assert!(p.contains("prose"));
        assert!(!p.contains("Executive Summary"));
        assert!(p.contains("focus on budget talk"));
    }
}
