//! AI transcript correction on export (spec 0068): clean up the exported
//! transcript text — fix speech-to-text and translation errors using the whole
//! dialogue as context — billed per event and cached per `(session, mode, lang)`
//! so a repeat export of the same shape is free. Mirrors the report/sentiment
//! billing + cache patterns; the corrected text is overlaid onto the
//! [`TranscriptExport`] and rendered by the existing JSON/PDF/subtitle code.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::StreamExt;
use rust_decimal::Decimal;
use uuid::Uuid;

use super::{condense_transcript, transcript_to_text};
use crate::billing::usd;
use crate::config::AiConfig;
use crate::db::Pool;
use crate::groq::{ChatRequest, Groq};
use crate::transcripts::TranscriptExport;

/// Lines corrected per Groq call. Utterances are short, so a modest batch keeps
/// the JSON reply well inside `max_tokens` while bounding the number of calls.
const CORRECTION_BATCH: usize = 12;
/// Concurrent Groq calls during a correction (after the first probes the model).
const CONCURRENCY: usize = 4;

/// What the chosen export shows ⇒ which text fields we correct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorrectionMode {
    /// Source text only (JSON export). `lang` is irrelevant (stored as "").
    Original,
    /// The translation into `lang` only.
    Translated,
    /// Source text and the translation into `lang` (PDF, "both" subtitles).
    Both,
}

impl CorrectionMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "original" => Some(Self::Original),
            "translated" => Some(Self::Translated),
            "both" => Some(Self::Both),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Original => "original",
            Self::Translated => "translated",
            Self::Both => "both",
        }
    }

    fn corrects_original(self) -> bool {
        matches!(self, Self::Original | Self::Both)
    }

    fn corrects_translated(self) -> bool {
        matches!(self, Self::Translated | Self::Both)
    }

    /// Text fields corrected per event — `both` is two passes, so it costs 2×.
    pub fn passes(self) -> i64 {
        if matches!(self, Self::Both) {
            2
        } else {
            1
        }
    }
}

/// User-facing cost: `base + per_event × ⌈events⌉ × passes(mode)`. Floors at one
/// event so an empty estimate is never zero.
pub fn correction_cost(ai: &AiConfig, mode: CorrectionMode, event_count: i64) -> Decimal {
    let events = event_count.max(1) as u64;
    (usd(ai.correction_base)
        + usd(ai.correction_per_event)
            * Decimal::from(events)
            * Decimal::from(mode.passes() as u64))
    .round_dp(6)
}

/// One event's corrected text fields, aligned to the export by event index `i`.
/// Only the fields the mode covers are present; missing fields keep the raw text
/// at apply time.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CorrectedLine {
    pub i: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub translated: Option<String>,
}

/// One line handed to the model for correction.
struct Line {
    i: usize,
    speaker: String,
    lang: String,
    text: String,
}

/// System prompt: a strict editor that fixes errors using the dialogue context
/// and the names that occur in it, never translating or rephrasing. The transcript
/// lines arrive as data (JSON); the context is reference, not an instruction.
///
/// Privacy note. Lines carry a pseudonymous `speaker` label ("Speaker 1"), never the
/// participant's name: the attribution — WHO said WHAT — is the part worth protecting,
/// and the model does not need an identity to use the dialogue's turn structure.
///
/// `names` is a different thing and is deliberately kept: it is the spelling reference
/// for names that occur INSIDE the speech ("ciao Alessandro" misheard as "Alessandra"),
/// which is one of the errors this pass exists to fix. Those names are already present
/// in the transcript text itself, so withholding the list would degrade the correction
/// without actually removing anything from the payload.
fn correction_prompt(context: &str, names: &str) -> String {
    format!(
        "You are a meticulous transcript editor. You receive meeting-transcript lines as a \
         JSON object {{\"lines\":[{{\"i\":<int>,\"speaker\":<name>,\"lang\":<code>,\"text\":<string>}}]}}. \
         Correct each line's `text`: fix speech-to-text and translation errors — misheard or \
         wrong words, missing or wrong punctuation and capitalization, obvious typos, and \
         inconsistent spelling of names — using the DIALOGUE CONTEXT and the NAMES OCCURRING \
         below to disambiguate. `speaker` is an opaque label that only distinguishes who is \
         talking; do not treat it as anyone's name and never write it into the corrected text. Do NOT translate, rephrase, summarize, censor, add, or remove content, and \
         do NOT change the meaning, tone, or language of any line — only fix errors. Keep every \
         line in its own original language. If a line is already correct, return it unchanged. \
         Respond with a single JSON object {{\"lines\":[{{\"i\":<the same i>,\"text\":<corrected text>}}]}} \
         containing exactly the input lines, same `i` values.\n\n\
         NAMES OCCURRING IN THE DIALOGUE (spelling reference): {names}\n\n\
         DIALOGUE CONTEXT (for reference only — do not correct or echo this):\n{context}"
    )
}

/// Correct one batch of lines via Groq JSON mode. Returns `(i, corrected text)`
/// pairs; a line the model omits simply isn't returned (kept raw at apply time).
async fn correct_batch(
    groq: Groq,
    model: String,
    system: String,
    batch: Vec<(usize, String, String, String)>,
) -> Result<Vec<(usize, String)>, String> {
    let lines: Vec<serde_json::Value> = batch
        .iter()
        .map(|(i, speaker, lang, text)| {
            serde_json::json!({ "i": i, "speaker": speaker, "lang": lang, "text": text })
        })
        .collect();
    let user = serde_json::json!({ "lines": lines }).to_string();

    let mut req = ChatRequest::new(model, system, user);
    req.max_tokens = 4096;
    req.temperature = 0.2;
    req.timeout = Duration::from_secs(40);
    req.max_retries = 3;

    let v = groq.chat_json(req).await?;
    let out: Vec<(usize, String)> = v["lines"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|l| {
                    let i = l["i"].as_u64()? as usize;
                    let text = l["text"].as_str()?.trim();
                    (!text.is_empty()).then(|| (i, text.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    if out.is_empty() {
        return Err("correction batch returned no usable lines".to_string());
    }
    Ok(out)
}

/// Correct every line of one field. The first batch probes the model (a 4xx
/// model-id rejection switches the whole field to the fallback model, mirroring
/// the report path); the rest run concurrently and individually-failed batches
/// are dropped (those lines stay raw). Errors only when *no* batch succeeds.
async fn correct_field(
    groq: &Groq,
    ai: &AiConfig,
    system: &str,
    lines: Vec<Line>,
    start_model: String,
) -> Result<(BTreeMap<usize, String>, String), String> {
    let batches: Vec<Vec<(usize, String, String, String)>> = lines
        .chunks(CORRECTION_BATCH)
        .map(|c| {
            c.iter()
                .map(|l| (l.i, l.speaker.clone(), l.lang.clone(), l.text.clone()))
                .collect()
        })
        .collect();
    let Some((first, rest)) = batches.split_first() else {
        return Ok((BTreeMap::new(), start_model));
    };

    let mut model = start_model;
    let mut out: BTreeMap<usize, String> = BTreeMap::new();
    match correct_batch(
        groq.clone(),
        model.clone(),
        system.to_string(),
        first.clone(),
    )
    .await
    {
        Ok(pairs) => out.extend(pairs),
        Err(e) if e.contains("groq returned 4") && ai.fallback_model != model => {
            tracing::warn!("correction model failed ({e}); retrying on fallback model");
            model = ai.fallback_model.clone();
            let pairs = correct_batch(
                groq.clone(),
                model.clone(),
                system.to_string(),
                first.clone(),
            )
            .await?;
            out.extend(pairs);
        }
        Err(e) => return Err(e),
    }

    let futs: Vec<_> = rest
        .iter()
        .map(|batch| {
            let groq = groq.clone();
            let model = model.clone();
            let system = system.to_string();
            let batch = batch.clone();
            async move { correct_batch(groq, model, system, batch).await }
        })
        .collect();
    let results: Vec<Result<Vec<(usize, String)>, String>> = futures::stream::iter(futs)
        .buffered(CONCURRENCY)
        .collect()
        .await;
    for res in results {
        match res {
            Ok(pairs) => out.extend(pairs),
            Err(e) => tracing::warn!("correction batch failed (lines kept raw): {e}"),
        }
    }

    if out.is_empty() {
        return Err("all correction batches failed".to_string());
    }
    Ok((out, model))
}

/// Generate the corrected lines for a session/mode. Returns `(lines, model)`.
/// Whole-dialogue context comes from [`condense_transcript`] (passthrough when
/// short, map-reduce summary when long) so each batch is correction-aware
/// without re-sending the entire transcript verbatim on huge calls.
pub async fn generate_correction(
    groq: &Groq,
    ai: &AiConfig,
    export: &TranscriptExport,
    mode: CorrectionMode,
    lang: &str,
) -> Result<(Vec<CorrectedLine>, String), String> {
    let context = condense_transcript(groq, ai, transcript_to_text(export)).await?;
    let names = participant_names(export);
    // Pseudonymises the per-line attribution; see `correction_prompt` for why the name
    // LIST stays while the per-line `speaker` does not.
    let aliases = crate::ai::pseudonym::SpeakerAliases::from_names(
        export.events.iter().map(|e| e.speaker_name.clone()),
    );
    let system = correction_prompt(&context, &names);

    let mut model = ai.report_model.clone();
    let mut by_index: BTreeMap<usize, CorrectedLine> = BTreeMap::new();

    if mode.corrects_original() {
        let lines: Vec<Line> = export
            .events
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.original.trim().is_empty())
            .map(|(i, e)| Line {
                i,
                speaker: aliases.alias(&e.speaker_name).to_string(),
                lang: e.lang.clone(),
                text: e.original.clone(),
            })
            .collect();
        let (map, used) = correct_field(groq, ai, &system, lines, model.clone()).await?;
        model = used;
        for (i, text) in map {
            by_index
                .entry(i)
                .or_insert_with(|| CorrectedLine {
                    i,
                    ..Default::default()
                })
                .original = Some(text);
        }
    }

    if mode.corrects_translated() {
        let lines: Vec<Line> = export
            .events
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                e.translations
                    .get(lang)
                    .filter(|t| !t.trim().is_empty())
                    .map(|t| Line {
                        i,
                        speaker: aliases.alias(&e.speaker_name).to_string(),
                        lang: lang.to_string(),
                        text: t.clone(),
                    })
            })
            .collect();
        // No translation in this language anywhere → nothing to correct in this
        // pass (the original pass, if any, still stands).
        if !lines.is_empty() {
            let (map, used) = correct_field(groq, ai, &system, lines, model.clone()).await?;
            model = used;
            for (i, text) in map {
                by_index
                    .entry(i)
                    .or_insert_with(|| CorrectedLine {
                        i,
                        ..Default::default()
                    })
                    .translated = Some(text);
            }
        }
    }

    if by_index.is_empty() {
        return Err("correction produced no lines".to_string());
    }
    Ok((by_index.into_values().collect(), model))
}

/// Overlay corrected text onto an export in place: corrected `original` replaces
/// the event's text; corrected `translated` replaces `translations[lang]`. Safe
/// for index mismatches (only overlapping events are touched).
pub fn apply_correction(export: &mut TranscriptExport, lines: &[CorrectedLine], lang: &str) {
    use std::collections::HashMap;
    let map: HashMap<usize, &CorrectedLine> = lines.iter().map(|l| (l.i, l)).collect();
    for (idx, ev) in export.events.iter_mut().enumerate() {
        let Some(cl) = map.get(&idx) else { continue };
        if let Some(o) = &cl.original {
            ev.original = o.clone();
        }
        if let Some(t) = &cl.translated {
            if !lang.is_empty() {
                ev.translations.insert(lang.to_string(), t.clone());
            }
        }
    }
}

fn participant_names(export: &TranscriptExport) -> String {
    export
        .session
        .participants
        .iter()
        .map(|p| p.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// One cached correction row, as returned by the REST endpoints.
#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct CorrectionRow {
    pub id: Uuid,
    pub mode: String,
    pub lang: String,
    #[serde(rename = "result")]
    pub result_json: serde_json::Value,
    pub model: String,
    pub cost: Decimal,
    pub created_at: DateTime<Utc>,
}

/// The corrected lines stored in a row, ready for [`apply_correction`].
pub fn row_lines(row: &CorrectionRow) -> Vec<CorrectedLine> {
    serde_json::from_value(row.result_json.clone()).unwrap_or_default()
}

/// Persist a correction. `None` means another request won the UNIQUE race —
/// the caller should fall back to the stored row.
#[allow(clippy::too_many_arguments)]
pub async fn save_correction(
    pool: &Pool,
    session_id: Uuid,
    user_id: Uuid,
    mode: CorrectionMode,
    lang: &str,
    lines: &[CorrectedLine],
    model: &str,
    cost: Decimal,
) -> Result<Option<CorrectionRow>, sqlx::Error> {
    sqlx::query_as(
        "INSERT INTO session_corrections (session_id, user_id, mode, lang, result_json, model, cost)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (session_id, mode, lang) DO NOTHING
         RETURNING id, mode, lang, result_json, model, cost, created_at",
    )
    .bind(session_id)
    .bind(user_id)
    .bind(mode.as_str())
    .bind(lang)
    .bind(sqlx::types::Json(lines))
    .bind(model)
    .bind(cost)
    .fetch_optional(pool)
    .await
}

/// The cached correction for a `(session, mode, lang)` (any participant reads it).
pub async fn get_correction(
    pool: &Pool,
    session_id: Uuid,
    mode: CorrectionMode,
    lang: &str,
) -> Result<Option<CorrectionRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, mode, lang, result_json, model, cost, created_at
         FROM session_corrections WHERE session_id = $1 AND mode = $2 AND lang = $3",
    )
    .bind(session_id)
    .bind(mode.as_str())
    .bind(lang)
    .fetch_optional(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AiConfig;
    use crate::transcripts::{ExportEvent, ExportParticipant, ExportSession};
    use chrono::Duration as ChronoDuration;
    use std::collections::HashMap;

    fn ai() -> AiConfig {
        AiConfig::test_default() // correction_base 0.05, correction_per_event 0.001
    }

    fn export_with(events: Vec<(&str, &str, &str, Option<&str>)>) -> TranscriptExport {
        // (speaker, lang, original, translation_it)
        let start = Utc::now();
        TranscriptExport {
            session: ExportSession {
                id: Uuid::new_v4(),
                room_name: "demo".into(),
                started_at: start,
                ended_at: Some(start + ChronoDuration::seconds(120)),
                duration_seconds: 120,
                participants: vec![ExportParticipant {
                    id: "p1".into(),
                    name: "Anna".into(),
                    language: "en".into(),
                }],
            },
            events: events
                .into_iter()
                .enumerate()
                .map(|(n, (speaker, lang, original, tr))| {
                    let mut translations = HashMap::new();
                    if let Some(t) = tr {
                        translations.insert("it".to_string(), t.to_string());
                    }
                    ExportEvent {
                        kind: "speech".into(),
                        ts: start + ChronoDuration::seconds(n as i64),
                        speaker_id: format!("{speaker}-peer"),
                        speaker_name: speaker.into(),
                        lang: lang.into(),
                        original: original.into(),
                        translations,
                    }
                })
                .collect(),
            bookmarks: vec![],
            exported_at: start,
        }
    }

    #[test]
    fn mode_parse_and_passes() {
        assert_eq!(CorrectionMode::parse("both"), Some(CorrectionMode::Both));
        assert_eq!(CorrectionMode::parse("nope"), None);
        assert_eq!(CorrectionMode::Original.passes(), 1);
        assert_eq!(CorrectionMode::Translated.passes(), 1);
        assert_eq!(CorrectionMode::Both.passes(), 2);
    }

    #[test]
    fn cost_scales_with_events_and_doubles_for_both() {
        let ai = ai();
        // base 0.05 + 10 events × 0.001 × 1 pass = 0.06
        assert_eq!(
            correction_cost(&ai, CorrectionMode::Original, 10),
            usd(0.06)
        );
        assert_eq!(
            correction_cost(&ai, CorrectionMode::Translated, 10),
            usd(0.06)
        );
        // both: base 0.05 + 10 × 0.001 × 2 = 0.07
        assert_eq!(correction_cost(&ai, CorrectionMode::Both, 10), usd(0.07));
        // floors at one event so the estimate is never just the base
        assert_eq!(
            correction_cost(&ai, CorrectionMode::Original, 0),
            usd(0.051)
        );
    }

    #[test]
    fn apply_overlays_original_and_translation_by_index() {
        let mut export = export_with(vec![
            ("Anna", "en", "helo wrld", Some("ciao mund")),
            ("Anna", "en", "untouched", Some("intatto")),
        ]);
        let lines = vec![
            CorrectedLine {
                i: 0,
                original: Some("hello world".into()),
                translated: Some("ciao mondo".into()),
            },
            // index 1 absent → stays raw
        ];
        apply_correction(&mut export, &lines, "it");
        assert_eq!(export.events[0].original, "hello world");
        assert_eq!(export.events[0].translations["it"], "ciao mondo");
        assert_eq!(export.events[1].original, "untouched");
        assert_eq!(export.events[1].translations["it"], "intatto");
    }

    #[test]
    fn apply_is_safe_for_out_of_range_indices() {
        let mut export = export_with(vec![("Anna", "en", "one", None)]);
        // index 5 doesn't exist — must not panic, must not touch event 0.
        let lines = vec![CorrectedLine {
            i: 5,
            original: Some("nope".into()),
            translated: None,
        }];
        apply_correction(&mut export, &lines, "");
        assert_eq!(export.events[0].original, "one");
    }

    #[test]
    fn original_mode_does_not_touch_translations() {
        let mut export = export_with(vec![("Anna", "en", "x", Some("y"))]);
        let lines = vec![CorrectedLine {
            i: 0,
            original: Some("X.".into()),
            translated: None,
        }];
        apply_correction(&mut export, &lines, "");
        assert_eq!(export.events[0].original, "X.");
        assert_eq!(export.events[0].translations["it"], "y");
    }

    #[test]
    fn prompt_carries_context_and_names_and_forbids_translating() {
        let p = correction_prompt("Anna talks about the Q3 budget.", "Anna, Bob");
        assert!(p.contains("Anna, Bob"));
        assert!(p.contains("Q3 budget"));
        assert!(p.contains("Do NOT translate"));
        assert!(p.contains("\"lines\""));
    }
}
