//! Sentiment analysis (spec 0015): slice the transcript into time windows,
//! score each window via Groq JSON mode, and aggregate into a timeline +
//! per-speaker breakdown. Results are cached per session (UNIQUE(session_id))
//! so only the first request is billed.

use super::pseudonym::SpeakerAliases;
use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::StreamExt;
use rust_decimal::Decimal;
use uuid::Uuid;

use super::billed_minutes;
use crate::billing::usd;
use crate::config::AiConfig;
use crate::db::Pool;
use crate::groq::{ChatRequest, Groq};
use crate::transcripts::TranscriptExport;

/// Base timeline resolution: one score per window of this many seconds.
pub const WINDOW_SECS: i64 = 120;
/// Model-call cap per analysis; longer sessions get a wider window instead.
pub const MAX_CHUNKS: i64 = 30;
/// Concurrent Groq calls during an analysis (keeps 429s rare).
const CONCURRENCY: usize = 4;
/// Key moments kept in the final result (strongest first, then re-sorted).
const MAX_KEY_MOMENTS: usize = 8;

/// User-facing cost: `base + per_participant × N + per_minute × ⌈min⌉`.
pub fn sentiment_cost(ai: &AiConfig, participants: usize, duration_seconds: i64) -> Decimal {
    (usd(ai.sentiment_base)
        + usd(ai.sentiment_per_participant) * Decimal::from(participants as u64)
        + usd(ai.sentiment_per_minute) * Decimal::from(billed_minutes(duration_seconds)))
    .round_dp(6)
}

/// Window width for a session: 120s, widened in whole 120s steps so the
/// analysis never needs more than [`MAX_CHUNKS`] model calls.
pub fn effective_window(duration_seconds: i64) -> i64 {
    let windows_needed = duration_seconds.max(0) / WINDOW_SECS + 1;
    WINDOW_SECS * ((windows_needed + MAX_CHUNKS - 1) / MAX_CHUNKS).max(1)
}

/// One transcript slice, rendered as `Speaker: text` lines for the prompt.
#[derive(Debug, PartialEq)]
pub struct SentimentChunk {
    /// Window start, seconds from session start.
    pub start_secs: i64,
    pub text: String,
}

/// Group events into `window_secs` slices by speaker line. Empty windows
/// (silence) produce no chunk — the timeline simply has no point there.
pub fn chunk_transcript(
    export: &TranscriptExport,
    window_secs: i64,
    aliases: &SpeakerAliases,
) -> Vec<SentimentChunk> {
    let start = export.session.started_at;
    let mut windows: BTreeMap<i64, String> = BTreeMap::new();
    for ev in &export.events {
        let secs = (ev.ts - start).num_seconds().max(0);
        let chat = if ev.kind == "chat" { " [chat]" } else { "" };
        // The LABEL, never the name. The model only has to tell participants apart;
        // their identities are mapped back in `restore_speakers` once the answer is in.
        windows
            .entry(secs / window_secs)
            .or_default()
            .push_str(&format!(
                "{}{}: {}\n",
                aliases.alias(&ev.speaker_name),
                chat,
                ev.original
            ));
    }
    windows
        .into_iter()
        .map(|(win, text)| SentimentChunk {
            start_secs: win * window_secs,
            text,
        })
        .collect()
}

/// Talk share per speaker as a percentage (1 decimal), proxied by character
/// count of *speech* events. Chat doesn't count as talking. Every session
/// participant appears, silent ones at 0.
pub fn talk_share(export: &TranscriptExport) -> Vec<(String, f64)> {
    let mut chars: BTreeMap<&str, usize> = export
        .session
        .participants
        .iter()
        .map(|p| (p.name.as_str(), 0))
        .collect();
    for ev in &export.events {
        if ev.kind != "chat" {
            *chars.entry(ev.speaker_name.as_str()).or_default() += ev.original.chars().count();
        }
    }
    let total: usize = chars.values().sum();
    let mut shares: Vec<(String, f64)> = chars
        .into_iter()
        .map(|(name, c)| {
            let pct = if total == 0 {
                0.0
            } else {
                (c as f64 / total as f64 * 1000.0).round() / 10.0
            };
            (name.to_string(), pct)
        })
        .collect();
    shares.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    shares
}

/// Speaker labels for this transcript, in order of first appearance.
pub fn aliases_for(export: &TranscriptExport) -> SpeakerAliases {
    // Participants first so the roster order is stable even when someone never speaks,
    // then the events themselves for anyone who spoke without being listed.
    SpeakerAliases::from_names(
        export
            .session
            .participants
            .iter()
            .map(|p| p.name.clone())
            .chain(export.events.iter().map(|e| e.speaker_name.clone())),
    )
}

/// Map a chunk answer's per-speaker keys back to real names.
///
/// The model was given labels, so it answers in labels. `aggregate` folds these into
/// `talk_share`, which is keyed by real names — without this step the two sides never
/// line up and every speaker score is silently dropped.
pub fn restore_speakers(
    mut chunk: serde_json::Value,
    aliases: &SpeakerAliases,
) -> serde_json::Value {
    if let Some(speakers) = chunk.get("speakers") {
        chunk["speakers"] = aliases.restore_keys(speakers);
    }
    chunk
}

fn chunk_prompt() -> String {
    "You rate the emotional tone of a meeting-transcript segment. Lines are \
     `Speaker: text`; `[chat]` marks text-chat messages. Respond with a single \
     JSON object only: {\"score\": <overall tone, -1.0 very negative to 1.0 \
     very positive>, \"speakers\": {\"<name>\": <that speaker's tone score>}, \
     \"moment\": <a short label (max 10 words, in the segment's language) for \
     a notable emotional moment, or null if the segment is unremarkable>}. \
     Score every speaker that appears in the segment."
        .to_string()
}

fn mood(score: f64) -> &'static str {
    if score >= 0.15 {
        "positive"
    } else if score <= -0.15 {
        "negative"
    } else {
        "neutral"
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Fold per-chunk model outputs into the stored result. Malformed chunk
/// results (no numeric `score`) are dropped rather than failing the analysis.
/// Pure, for tests.
pub fn aggregate(
    points: &[(i64, serde_json::Value)],
    export: &TranscriptExport,
    window_secs: i64,
) -> serde_json::Value {
    let mut timeline = Vec::new();
    let mut speaker_scores: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut moments: Vec<(i64, String, f64)> = Vec::new();

    for (t, v) in points {
        let Some(score) = v["score"].as_f64() else {
            continue; // malformed chunk — drop, don't fail
        };
        let score = score.clamp(-1.0, 1.0);
        timeline.push(serde_json::json!({ "t": t, "score": round2(score) }));
        if let Some(map) = v["speakers"].as_object() {
            for (name, s) in map {
                if let Some(s) = s.as_f64() {
                    speaker_scores
                        .entry(name.clone())
                        .or_default()
                        .push(s.clamp(-1.0, 1.0));
                }
            }
        }
        if let Some(label) = v["moment"].as_str() {
            let label = label.trim();
            if !label.is_empty() {
                moments.push((*t, label.to_string(), round2(score)));
            }
        }
    }

    let scores: Vec<f64> = timeline
        .iter()
        .filter_map(|p| p["score"].as_f64())
        .collect();
    let mean = if scores.is_empty() {
        0.0
    } else {
        scores.iter().sum::<f64>() / scores.len() as f64
    };
    let max = scores.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let min = scores.iter().cloned().fold(f64::INFINITY, f64::min);
    // Strong swings in both directions read as "mixed", not their average.
    let overall_mood = if !scores.is_empty() && max >= 0.3 && min <= -0.3 {
        "mixed"
    } else {
        mood(mean)
    };

    // Per-speaker: average chunk score; talk share from speech volume. Only
    // names the model actually scored get a score (others stay null).
    let speakers: Vec<serde_json::Value> = talk_share(export)
        .into_iter()
        .map(|(name, pct)| {
            let avg = speaker_scores
                .get(&name)
                .map(|v| round2(v.iter().sum::<f64>() / v.len() as f64));
            serde_json::json!({
                "name": name,
                "talk_pct": pct,
                "score": avg,
                "mood": avg.map(mood),
            })
        })
        .collect();

    // Keep the strongest moments, then present them chronologically.
    moments.sort_by(|a, b| {
        b.2.abs()
            .partial_cmp(&a.2.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    moments.truncate(MAX_KEY_MOMENTS);
    moments.sort_by_key(|m| m.0);
    let key_moments: Vec<serde_json::Value> = moments
        .into_iter()
        .map(|(t, label, score)| serde_json::json!({ "t": t, "label": label, "score": score }))
        .collect();

    serde_json::json!({
        "overall": { "score": round2(mean), "mood": overall_mood },
        "timeline": timeline,
        "speakers": speakers,
        "key_moments": key_moments,
        "window_secs": window_secs,
    })
}

/// Owned args so the futures built for `buffered()` capture no borrows —
/// borrowed closures here break axum's `Handler` inference upstream.
async fn analyze_chunk(
    groq: Groq,
    model: String,
    text: String,
) -> Result<serde_json::Value, String> {
    let mut req = ChatRequest::new(model, chunk_prompt(), text);
    // gpt-oss is a REASONING model in JSON mode: a small (256) budget is consumed
    // by hidden reasoning before any JSON is emitted, so Groq rejects the call with
    // `json_validate_failed` (empty generation) — which dropped every chunk on long
    // calls. The visible JSON is tiny; the budget is for the reasoning (mirrors the
    // report path's 2048).
    req.max_tokens = 2048;
    req.timeout = Duration::from_secs(30);
    req.max_retries = 3;
    groq.chat_json(req).await
}

/// Run the full analysis. Returns `(result_json, model_used)`.
///
/// The first chunk also probes the model: a 4xx (decommissioned model id)
/// switches the whole run to the fallback model, mirroring the report path. Any
/// OTHER first-chunk failure (timeout, 429/5xx after retries, malformed JSON) is
/// dropped exactly like the rest — a single flaky call must not sink an
/// otherwise-fine analysis (notably long sessions, whose wider first window is a
/// bigger, slower, more timeout-prone request). The analysis only errors when
/// *no* chunk succeeds, so the caller can refuse to charge.
pub async fn analyze(
    groq: &Groq,
    ai: &AiConfig,
    export: &TranscriptExport,
) -> Result<(serde_json::Value, String), String> {
    let window = effective_window(export.session.duration_seconds);
    // Built once and shared by every chunk, so "Speaker 2" means the same person all
    // the way through the analysis.
    let aliases = aliases_for(export);
    let chunks = chunk_transcript(export, window, &aliases);
    let Some((first, rest)) = chunks.split_first() else {
        return Err("transcript has no events".to_string());
    };

    let mut model = ai.report_model.clone();
    let mut points: Vec<(i64, serde_json::Value)> = Vec::with_capacity(chunks.len());
    match analyze_chunk(groq.clone(), model.clone(), first.text.clone()).await {
        Ok(v) => points.push((first.start_secs, restore_speakers(v, &aliases))),
        // Decommissioned model id (4xx): switch the whole run to the fallback and
        // re-probe — but a failure there is dropped too, not fatal.
        Err(e) if e.contains("groq returned 4") && ai.fallback_model != model => {
            tracing::warn!("sentiment model failed ({e}); retrying on fallback model");
            model = ai.fallback_model.clone();
            match analyze_chunk(groq.clone(), model.clone(), first.text.clone()).await {
                Ok(v) => points.push((first.start_secs, restore_speakers(v, &aliases))),
                Err(e) => tracing::warn!("sentiment first chunk failed on fallback (dropped): {e}"),
            }
        }
        // Transient failure on the first chunk: drop it like any other chunk.
        Err(e) => tracing::warn!("sentiment first chunk failed (dropped): {e}"),
    }

    let futs: Vec<_> = rest
        .iter()
        .map(|c| {
            let groq = groq.clone();
            let model = model.clone();
            let text = c.text.clone();
            let t = c.start_secs;
            async move { (t, analyze_chunk(groq, model, text).await) }
        })
        .collect();
    let rest_results: Vec<(i64, Result<serde_json::Value, String>)> = futures::stream::iter(futs)
        .buffered(CONCURRENCY)
        .collect()
        .await;
    for (t, res) in rest_results {
        match res {
            Ok(v) => points.push((t, restore_speakers(v, &aliases))),
            Err(e) => tracing::warn!("sentiment chunk at {t}s failed (dropped): {e}"),
        }
    }

    // Refuse to charge only when EVERY chunk failed (e.g. Groq fully unavailable).
    if points.is_empty() {
        return Err("all sentiment chunks failed".to_string());
    }
    points.sort_by_key(|(t, _)| *t);

    Ok((aggregate(&points, export, window), model))
}

/// One stored analysis, as returned by the REST endpoints.
#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct SentimentRow {
    pub id: Uuid,
    #[serde(rename = "result")]
    pub result_json: serde_json::Value,
    pub model: String,
    pub cost: Decimal,
    pub created_at: DateTime<Utc>,
}

/// Persist an analysis. `None` means another request won the UNIQUE race —
/// the caller should fall back to the stored row.
pub async fn save_sentiment(
    pool: &Pool,
    session_id: Uuid,
    user_id: Uuid,
    result: &serde_json::Value,
    model: &str,
    cost: Decimal,
) -> Result<Option<SentimentRow>, sqlx::Error> {
    sqlx::query_as(
        "INSERT INTO session_sentiments (session_id, user_id, result_json, model, cost)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (session_id) DO NOTHING
         RETURNING id, result_json, model, cost, created_at",
    )
    .bind(session_id)
    .bind(user_id)
    .bind(result)
    .bind(model)
    .bind(cost)
    .fetch_optional(pool)
    .await
}

/// The cached analysis for a session (any participant can read it).
pub async fn get_sentiment(
    pool: &Pool,
    session_id: Uuid,
) -> Result<Option<SentimentRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, result_json, model, cost, created_at
         FROM session_sentiments WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::transcripts::{ExportEvent, ExportParticipant, ExportSession, TranscriptExport};
    use chrono::Duration as ChronoDuration;
    use std::collections::HashMap;

    fn ai() -> AiConfig {
        Config::test_with_billing("postgres://x", "s", 2.0)
            .billing
            .unwrap()
            .ai
    }

    fn export_with(events: Vec<(&str, &str, i64, &str)>) -> TranscriptExport {
        // (speaker, kind, offset_secs, text)
        let start = Utc::now();
        let mut names: Vec<&str> = events.iter().map(|e| e.0).collect();
        names.dedup();
        TranscriptExport {
            session: ExportSession {
                id: Uuid::new_v4(),
                room_name: "demo".into(),
                started_at: start,
                ended_at: Some(start + ChronoDuration::seconds(600)),
                duration_seconds: 600,
                participants: names
                    .into_iter()
                    .map(|n| ExportParticipant {
                        id: format!("{n}-peer"),
                        name: n.into(),
                        language: "en".into(),
                    })
                    .collect(),
            },
            events: events
                .into_iter()
                .map(|(name, kind, secs, text)| ExportEvent {
                    kind: kind.into(),
                    ts: start + ChronoDuration::seconds(secs),
                    speaker_id: format!("{name}-peer"),
                    speaker_name: name.into(),
                    lang: "en".into(),
                    original: text.into(),
                    translations: HashMap::new(),
                })
                .collect(),
            bookmarks: vec![],
            exported_at: start,
        }
    }

    #[test]
    fn cost_formula_charges_base_participants_and_minutes() {
        let ai = ai();
        // base 0.05 + 2 × 0.01 + 11 min × 0.002 = 0.092
        assert_eq!(sentiment_cost(&ai, 2, 610), usd(0.092));
        // Zero-length sessions still bill one minute.
        assert_eq!(sentiment_cost(&ai, 1, 0), usd(0.062));
    }

    #[test]
    fn effective_window_widens_for_long_sessions() {
        assert_eq!(effective_window(0), 120);
        assert_eq!(effective_window(3599), 120); // 30 windows — at the cap
        assert_eq!(effective_window(3600), 240); // 31 needed -> doubled
        assert_eq!(effective_window(2 * 3600), 360);
    }

    #[test]
    fn chunk_transcript_buckets_on_window_edges_and_skips_silence() {
        let export = export_with(vec![
            ("Anna", "speech", 0, "hello"),
            ("Anna", "speech", 119, "still window zero"),
            ("Bob", "speech", 120, "window one starts here"),
            ("Bob", "chat", 121, "a link"),
            // 240–359 silent -> no chunk
            ("Anna", "speech", 360, "window three"),
        ]);
        let chunks = chunk_transcript(&export, 120, &aliases_for(&export));
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].start_secs, 0);
        // Labels, not names — speakers are pseudonymised before the text leaves for Groq
        // (see `aliases_for`). Anna speaks first, so she is Speaker 1.
        assert!(chunks[0].text.contains("Speaker 1: hello"));
        assert!(chunks[0].text.contains("Speaker 1: still window zero"));
        assert_eq!(chunks[1].start_secs, 120);
        assert!(chunks[1].text.contains("Speaker 2: window one starts here"));
        assert!(chunks[1].text.contains("Speaker 2 [chat]: a link"));
        assert_eq!(chunks[2].start_secs, 360);
    }

    #[test]
    fn talk_share_counts_speech_only_and_includes_silent_participants() {
        let mut export = export_with(vec![
            ("Anna", "speech", 0, "123456"), // 6 chars
            ("Bob", "speech", 10, "12"),     // 2 chars
            ("Bob", "chat", 20, "ignored chat wall of text"),
        ]);
        export.session.participants.push(ExportParticipant {
            id: "carl-peer".into(),
            name: "Carl".into(),
            language: "en".into(),
        });
        let shares = talk_share(&export);
        assert_eq!(shares[0], ("Anna".to_string(), 75.0));
        assert_eq!(shares[1], ("Bob".to_string(), 25.0));
        assert_eq!(shares[2], ("Carl".to_string(), 0.0));
    }

    #[test]
    fn aggregate_builds_timeline_speakers_and_moments_dropping_malformed() {
        let export = export_with(vec![
            ("Anna", "speech", 0, "great work everyone"),
            ("Bob", "speech", 130, "this is a disaster"),
        ]);
        let points = vec![
            (
                0,
                serde_json::json!({
                    "score": 0.8,
                    "speakers": { "Anna": 0.8 },
                    "moment": "team celebrates"
                }),
            ),
            (
                120,
                serde_json::json!({
                    "score": -5.0, // clamped to -1
                    "speakers": { "Bob": -0.6, "Anna": "not a number" },
                    "moment": null
                }),
            ),
            (240, serde_json::json!({ "mood": "no score key" })), // dropped
        ];
        let v = aggregate(&points, &export, 120);

        let timeline = v["timeline"].as_array().unwrap();
        assert_eq!(timeline.len(), 2, "malformed chunk dropped");
        assert_eq!(timeline[0]["t"], 0);
        assert_eq!(timeline[0]["score"], 0.8);
        assert_eq!(timeline[1]["score"], -1.0, "scores clamp to [-1,1]");

        // 0.8 and -1.0 swing both ways -> mixed, not the (-0.1) mean's neutral.
        assert_eq!(v["overall"]["mood"], "mixed");
        assert_eq!(v["overall"]["score"], -0.1);

        let speakers = v["speakers"].as_array().unwrap();
        let anna = speakers.iter().find(|s| s["name"] == "Anna").unwrap();
        assert_eq!(anna["score"], 0.8, "non-numeric speaker score ignored");
        assert_eq!(anna["mood"], "positive");
        assert!(anna["talk_pct"].as_f64().unwrap() > 0.0);
        let bob = speakers.iter().find(|s| s["name"] == "Bob").unwrap();
        assert_eq!(bob["score"], -0.6);
        assert_eq!(bob["mood"], "negative");

        let moments = v["key_moments"].as_array().unwrap();
        assert_eq!(moments.len(), 1, "null/absent moments skipped");
        assert_eq!(moments[0]["label"], "team celebrates");
        assert_eq!(moments[0]["t"], 0);
        assert_eq!(v["window_secs"], 120);
    }

    #[test]
    fn aggregate_of_nothing_is_neutral_and_empty() {
        let export = export_with(vec![("Anna", "speech", 0, "hi")]);
        let v = aggregate(&[], &export, 120);
        assert_eq!(v["overall"]["score"], 0.0);
        assert_eq!(v["overall"]["mood"], "neutral");
        assert_eq!(v["timeline"].as_array().unwrap().len(), 0);
        assert_eq!(v["key_moments"].as_array().unwrap().len(), 0);
        // Speakers still listed (talk share is transcript-derived).
        assert_eq!(v["speakers"][0]["name"], "Anna");
        assert!(v["speakers"][0]["score"].is_null());
    }

    #[test]
    fn key_moments_keep_strongest_then_sort_chronologically() {
        let export = export_with(vec![("Anna", "speech", 0, "hi")]);
        let points: Vec<(i64, serde_json::Value)> = (0..12)
            .map(|i| {
                // Later chunks have stronger scores; all carry a moment.
                let score = (i as f64) / 12.0;
                (
                    i64::from(i) * 120,
                    serde_json::json!({
                        "score": score,
                        "speakers": {},
                        "moment": format!("moment {i}")
                    }),
                )
            })
            .collect();
        let v = aggregate(&points, &export, 120);
        let moments = v["key_moments"].as_array().unwrap();
        assert_eq!(moments.len(), MAX_KEY_MOMENTS, "capped");
        // The strongest 8 are moments 4..11, re-sorted by time.
        assert_eq!(moments[0]["label"], "moment 4");
        assert_eq!(moments[7]["label"], "moment 11");
        let ts: Vec<i64> = moments.iter().map(|m| m["t"].as_i64().unwrap()).collect();
        assert!(ts.windows(2).all(|w| w[0] < w[1]), "chronological: {ts:?}");
    }

    #[test]
    fn chunks_carry_labels_not_names() {
        // The whole point of the change: what leaves for Groq must not identify anyone.
        let export = export_with(vec![
            ("Anna", "speech", 0, "buongiorno a tutti"),
            ("Bruno", "speech", 10, "ciao Anna"),
        ]);
        let aliases = aliases_for(&export);
        let text = chunk_transcript(&export, 120, &aliases)
            .into_iter()
            .map(|c| c.text)
            .collect::<String>();

        assert!(text.contains("Speaker 1: buongiorno a tutti"));
        assert!(text.contains("Speaker 2: ciao Anna"));
        // The transcript CONTENT may still mention a name — that is the user's own
        // speech and cannot be removed without breaking the translation. What must not
        // appear is the speaker attribution.
        assert!(
            !text.contains("Anna: "),
            "speaker label leaked a real name: {text}"
        );
        assert!(
            !text.contains("Bruno: "),
            "speaker label leaked a real name: {text}"
        );
    }

    #[test]
    fn restore_speakers_maps_the_answer_back_to_real_names() {
        let export = export_with(vec![
            ("Anna", "speech", 0, "ciao"),
            ("Bruno", "speech", 5, "ehi"),
        ]);
        let aliases = aliases_for(&export);
        let answer = serde_json::json!({
            "score": 0.3,
            "speakers": { "Speaker 1": 0.5, "Speaker 2": 0.1 },
            "moment": null
        });
        let restored = restore_speakers(answer, &aliases);
        assert_eq!(restored["speakers"]["Anna"], 0.5);
        assert_eq!(restored["speakers"]["Bruno"], 0.1);
        assert_eq!(
            restored["score"], 0.3,
            "non-speaker fields must be untouched"
        );
    }

    #[test]
    fn an_answer_without_speakers_survives_restore() {
        let export = export_with(vec![("Anna", "speech", 0, "ciao")]);
        let aliases = aliases_for(&export);
        let answer = serde_json::json!({ "score": 0.0, "moment": null });
        assert_eq!(restore_speakers(answer.clone(), &aliases), answer);
    }
}
