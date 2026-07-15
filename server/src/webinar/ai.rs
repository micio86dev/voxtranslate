//! Webinar AI recap (webinar-analytics epic, Phase B): an AI report + per-
//! participant multilingual recap emails for a FINISHED webinar. Mirrors the
//! MEET AI system (`crate::ai::report` / `crate::ai::email_draft`) — the same
//! Groq generation, the same async `ai_jobs` claim pattern — but sourced from
//! `webinar_transcripts` and charged to the host's ORG credit pool.
//!
//! NO sentiment analysis anywhere (explicit product requirement).
//!
//! Privacy: participant email addresses are resolved server-side (from
//! `users.email` via `webinar_participants.user_id`) and NEVER returned to the
//! caller — only aggregate counts. Guests without an account are skipped, never
//! invented into a recipient.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use uuid::Uuid;

use crate::config::AiConfig;
use crate::transcripts::{ExportSession, TranscriptExport};
use crate::webinar::Webinar;

/// A `webinar_transcripts` row, in `spoken_at` order, as the assembler consumes
/// it: the original utterance + its `{ lang: text }` translation map.
#[derive(Debug, Clone)]
pub struct TranscriptRow {
    pub original_text: String,
    pub original_lang: String,
    /// `{ "<lang>": "<translated text>" }` (JSONB from the DB).
    pub translations: serde_json::Value,
    pub spoken_at: DateTime<Utc>,
}

/// Speaker label used for every host utterance in the assembled transcript. A
/// webinar is 1-to-many (one broadcaster), so a single stable label suffices —
/// there is no per-speaker attribution in `webinar_transcripts`.
const HOST_SPEAKER: &str = "Host";

/// Assemble webinar transcript rows into a [`TranscriptExport`] the shared MEET
/// generation (`ai::report::generate_report`) can consume unchanged.
///
/// For each row we prefer the translation for `lang` when present and non-empty,
/// else fall back to `original_text` — so a report/email in a viewer's language
/// reads their translation, while the source language reads the original. The
/// event `lang` is set to the rendered text's language so the transcript header
/// (`[HH:MM:SS] Host (lang): text`) is accurate.
///
/// `title` becomes the export's `room_name` (used in report/email metadata);
/// `start`/`end` bound the duration (used for per-minute report billing).
pub fn assemble_export(
    rows: &[TranscriptRow],
    lang: &str,
    title: &str,
    start: DateTime<Utc>,
    end: Option<DateTime<Utc>>,
) -> TranscriptExport {
    // Anchor relative timestamps on the earliest utterance when we have one; this
    // keeps `[HH:MM:SS]` offsets sensible even if `actual_start` is missing/late.
    let anchor = rows
        .iter()
        .map(|r| r.spoken_at)
        .min()
        .unwrap_or(start)
        .min(start);
    let effective_end = end.or_else(|| rows.iter().map(|r| r.spoken_at).max());
    let duration_seconds = effective_end
        .map(|e| (e - anchor).num_seconds().max(0))
        .unwrap_or(0);

    let events = rows
        .iter()
        .map(|r| {
            let (text, event_lang) = render_utterance(r, lang);
            crate::transcripts::ExportEvent {
                kind: "speech".to_string(),
                ts: r.spoken_at,
                speaker_id: HOST_SPEAKER.to_string(),
                speaker_name: HOST_SPEAKER.to_string(),
                lang: event_lang,
                original: text,
                translations: HashMap::new(),
            }
        })
        .collect();

    TranscriptExport {
        session: ExportSession {
            id: Uuid::nil(),
            room_name: title.to_string(),
            started_at: anchor,
            ended_at: effective_end,
            duration_seconds,
            participants: vec![],
        },
        events,
        bookmarks: vec![],
        exported_at: Utc::now(),
    }
}

/// Pick the text (and its language) for one utterance in `lang`: the `lang`
/// translation when present and non-empty, else the original. Returns
/// `(text, rendered_lang)`.
fn render_utterance(row: &TranscriptRow, lang: &str) -> (String, String) {
    if let Some(t) = row
        .translations
        .get(lang)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return (t.to_string(), lang.to_string());
    }
    (row.original_text.clone(), row.original_lang.clone())
}

/// A participant the recap-email fan-out will send to, resolved server-side.
/// Carries the DB `participant_id` (for the per-participant dedup key), the
/// resolved address, and the language the email should be written in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailTarget {
    pub participant_id: Uuid,
    pub email: String,
    pub lang: String,
}

/// A `webinar_participants` row joined with its account's language + email, as
/// the fan-out planner consumes it. `user_id`/`email` are `None` for guests.
#[derive(Debug, Clone)]
pub struct ParticipantRow {
    pub participant_id: Uuid,
    /// The account language (`users.language`), when the participant is signed in.
    pub user_language: Option<String>,
    /// The guest/participant language (`webinar_participants.language_code`).
    pub participant_language: Option<String>,
    /// The account email, when signed in AND it has one. None for guests.
    pub email: Option<String>,
}

/// Resolve each participant's recap-email target: their language = `users.language`
/// (logged-in) else `webinar_participants.language_code` (guest) else `fallback`;
/// their address = the account email. Participants without a resolvable email
/// (guests, deleted accounts) are SKIPPED — never invented. Pure, for tests.
pub fn plan_email_targets(rows: &[ParticipantRow], fallback_lang: &str) -> Vec<EmailTarget> {
    rows.iter()
        .filter_map(|r| {
            let email = r
                .email
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())?;
            let lang = r
                .user_language
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    r.participant_language
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                })
                .unwrap_or(fallback_lang);
            Some(EmailTarget {
                participant_id: r.participant_id,
                email: email.to_lowercase(),
                lang: lang.to_string(),
            })
        })
        .collect()
}

/// Group email targets by language so the email content is generated ONCE per
/// language, not once per person (content is language-shared). Returns
/// `lang -> targets`, preserving first-seen target order within each language.
pub fn group_targets_by_lang(targets: &[EmailTarget]) -> HashMap<String, Vec<EmailTarget>> {
    let mut out: HashMap<String, Vec<EmailTarget>> = HashMap::new();
    for t in targets {
        out.entry(t.lang.clone()).or_default().push(t.clone());
    }
    out
}

/// Convert a USD AI cost (the meet report/email price) to whole ORG credits.
/// The org pool is INTEGER credits at 100 credits = $1 (see `credits.rs`), so we
/// round UP to never under-charge a fractional cent. A zero/negative cost → 0.
pub fn usd_to_org_credits(usd: Decimal) -> i32 {
    if usd <= Decimal::ZERO {
        return 0;
    }
    (usd * Decimal::from(100)).ceil().to_i32().unwrap_or(0)
}

/// The report cost in ORG credits for a webinar of `duration_seconds`. Reuses the
/// MEET per-minute report price (`ai::report::report_cost`) then converts USD→credits.
pub fn report_cost_credits(ai: &AiConfig, duration_seconds: i64) -> i32 {
    usd_to_org_credits(crate::ai::report::report_cost(ai, duration_seconds))
}

/// The per-email cost in ORG credits. Reuses the flat MEET email-draft price
/// (`ai::email_draft::email_cost`) then converts USD→credits. Charged once per
/// email actually sent.
pub fn email_cost_credits(ai: &AiConfig) -> i32 {
    usd_to_org_credits(crate::ai::email_draft::email_cost(ai))
}

/// Convenience accessor for the webinar's report language when the host doesn't
/// pass one: the webinar's source language.
pub fn default_report_lang(w: &Webinar) -> &str {
    &w.source_language
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

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn row(orig: &str, orig_lang: &str, tr: serde_json::Value, at: &str) -> TranscriptRow {
        TranscriptRow {
            original_text: orig.into(),
            original_lang: orig_lang.into(),
            translations: tr,
            spoken_at: utc(at),
        }
    }

    #[test]
    fn assemble_prefers_target_translation_then_falls_back_to_original() {
        let rows = vec![
            row(
                "ciao a tutti",
                "it",
                serde_json::json!({ "en": "hello everyone", "es": "hola a todos" }),
                "2030-01-01T10:00:00Z",
            ),
            // No `en` translation → falls back to the original + its language.
            row(
                "grazie",
                "it",
                serde_json::json!({ "es": "gracias" }),
                "2030-01-01T10:01:00Z",
            ),
        ];
        let start = utc("2030-01-01T10:00:00Z");
        let end = Some(utc("2030-01-01T10:05:00Z"));
        let export = assemble_export(&rows, "en", "Q3 Webinar", start, end);

        let text = crate::ai::transcript_to_text(&export);
        // First line uses the English translation, tagged (en).
        assert!(
            text.contains("[00:00:00] Host (en): hello everyone"),
            "{text}"
        );
        // Second line has no English translation → original Italian, tagged (it).
        assert!(text.contains("[00:01:00] Host (it): grazie"), "{text}");
        assert_eq!(export.session.room_name, "Q3 Webinar");
        assert_eq!(export.session.duration_seconds, 300);
    }

    #[test]
    fn assemble_uses_original_for_source_lang_and_derives_duration_without_end() {
        let rows = vec![
            row("uno", "it", serde_json::json!({}), "2030-01-01T10:00:00Z"),
            row("due", "it", serde_json::json!({}), "2030-01-01T10:02:30Z"),
        ];
        let start = utc("2030-01-01T10:00:00Z");
        // No end → duration derived from the last utterance (150s).
        let export = assemble_export(&rows, "it", "T", start, None);
        assert_eq!(export.session.duration_seconds, 150);
        let text = crate::ai::transcript_to_text(&export);
        assert!(text.contains("[00:00:00] Host (it): uno"), "{text}");
        assert!(text.contains("[00:02:30] Host (it): due"), "{text}");
    }

    #[test]
    fn assemble_empty_rows_is_safe() {
        let start = utc("2030-01-01T10:00:00Z");
        let export = assemble_export(&[], "en", "T", start, None);
        assert!(export.events.is_empty());
        assert_eq!(export.session.duration_seconds, 0);
    }

    #[test]
    fn plan_targets_picks_user_lang_then_participant_lang_and_skips_guests() {
        let rows = vec![
            // Signed-in: user_language wins over participant_language.
            ParticipantRow {
                participant_id: Uuid::from_u128(1),
                user_language: Some("de".into()),
                participant_language: Some("fr".into()),
                email: Some("Alice@Example.com".into()),
            },
            // Signed-in, no account language → participant_language.
            ParticipantRow {
                participant_id: Uuid::from_u128(2),
                user_language: None,
                participant_language: Some("es".into()),
                email: Some("bob@example.com".into()),
            },
            // Guest — no email → SKIPPED (never invented).
            ParticipantRow {
                participant_id: Uuid::from_u128(3),
                user_language: None,
                participant_language: Some("it".into()),
                email: None,
            },
            // Signed-in but no language anywhere → fallback.
            ParticipantRow {
                participant_id: Uuid::from_u128(4),
                user_language: None,
                participant_language: None,
                email: Some("carol@example.com".into()),
            },
        ];
        let targets = plan_email_targets(&rows, "en");
        assert_eq!(targets.len(), 3, "guest without email skipped: {targets:?}");
        assert_eq!(targets[0].lang, "de");
        assert_eq!(targets[0].email, "alice@example.com", "lowercased");
        assert_eq!(targets[1].lang, "es");
        assert_eq!(targets[2].lang, "en", "fallback when no language known");
    }

    #[test]
    fn group_by_lang_generates_once_per_language() {
        let targets = vec![
            EmailTarget {
                participant_id: Uuid::from_u128(1),
                email: "a@x.co".into(),
                lang: "en".into(),
            },
            EmailTarget {
                participant_id: Uuid::from_u128(2),
                email: "b@x.co".into(),
                lang: "es".into(),
            },
            EmailTarget {
                participant_id: Uuid::from_u128(3),
                email: "c@x.co".into(),
                lang: "en".into(),
            },
        ];
        let grouped = group_targets_by_lang(&targets);
        assert_eq!(grouped.len(), 2, "two distinct languages");
        assert_eq!(
            grouped["en"].len(),
            2,
            "both en recipients share one generation"
        );
        assert_eq!(grouped["es"].len(), 1);
    }

    #[test]
    fn usd_to_org_credits_rounds_up_and_floors_at_zero() {
        assert_eq!(usd_to_org_credits(Decimal::ZERO), 0);
        assert_eq!(usd_to_org_credits(Decimal::new(-5, 2)), 0);
        // $0.052 (report base+1min) → 5.2 credits → 6.
        assert_eq!(usd_to_org_credits(Decimal::new(52, 3)), 6);
        // $0.02 (email) → 2 credits exactly.
        assert_eq!(usd_to_org_credits(Decimal::new(2, 2)), 2);
        // $0.10 → 10 credits exactly.
        assert_eq!(usd_to_org_credits(Decimal::new(10, 2)), 10);
    }

    #[test]
    fn report_and_email_cost_credits_reuse_meet_pricing() {
        let ai = ai(); // report base 0.05 + 0.002/min; email flat 0.02
                       // 1 minute → $0.052 → 6 credits.
        assert_eq!(report_cost_credits(&ai, 60), 6);
        // 2 minutes → $0.054 → 6 credits.
        assert_eq!(report_cost_credits(&ai, 61), 6);
        // Email flat $0.02 → 2 credits.
        assert_eq!(email_cost_credits(&ai), 2);
    }
}
