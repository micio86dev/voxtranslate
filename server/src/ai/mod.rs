//! Shared plumbing for the offline AI features (report, sentiment, email
//! draft) and live suggestions: transcript→text rendering, chunking, and a
//! map-reduce condenser for transcripts too long for one model call.

pub mod correction;
pub mod email_draft;
pub mod quiz;
pub mod report;
pub mod sentiment;

use crate::config::AiConfig;
use crate::groq::{ChatRequest, Groq};
use crate::transcripts::TranscriptExport;
use futures::StreamExt;
use std::time::Duration;

/// Above this many characters the transcript is condensed (map-reduce) before
/// the final prompt. ~12k tokens — well inside every Groq model's context but
/// leaves headroom for the system prompt and the completion.
pub const MAX_DIRECT_CHARS: usize = 48_000;
/// Size of each map-step chunk (split on line boundaries).
pub const CHUNK_CHARS: usize = 24_000;
/// Concurrent map-step Groq calls. Long calls produce many chunks; summarizing
/// them sequentially made a multi-hour transcript's report/email take minutes —
/// long enough for the upstream request proxy to time out and the feature to
/// "silently" fail. Bounded to keep 429s rare (mirrors sentiment's CONCURRENCY).
const CONDENSE_CONCURRENCY: usize = 6;
/// Safety cap on map-reduce passes. Each pass shrinks the text several-fold, so
/// 1–2 passes suffice; the cap only guards a pathological transcript that refuses
/// to shrink, in which case we send the best-effort result (the report model's
/// context is large enough to accept it).
const MAX_CONDENSE_PASSES: usize = 4;

/// Whole minutes for per-minute billing, rounded up; 0-second sessions bill 1.
pub fn billed_minutes(duration_seconds: i64) -> i64 {
    (duration_seconds.max(0) + 59).div_euclid(60).max(1)
}

/// Render a transcript export as plain text for prompts:
/// `[HH:MM:SS] Name (lang): text` per event (chat lines marked), followed by a
/// BOOKMARKS block when participants pinned moments. Timestamps are relative
/// to session start so the model can reason about timing.
pub fn transcript_to_text(export: &TranscriptExport) -> String {
    let start = export.session.started_at;
    let mut out = String::new();
    for ev in &export.events {
        let secs = (ev.ts - start).num_seconds().max(0);
        let kind = if ev.kind == "chat" { " [chat]" } else { "" };
        out.push_str(&format!(
            "[{}] {} ({}){}: {}\n",
            hms(secs),
            ev.speaker_name,
            ev.lang,
            kind,
            ev.original
        ));
    }
    if !export.bookmarks.is_empty() {
        out.push_str("\nBOOKMARKS (moments participants flagged as important):\n");
        for bm in &export.bookmarks {
            let secs = (bm.ts - start).num_seconds().max(0);
            match &bm.label {
                Some(l) => out.push_str(&format!("[{}] {}: {}\n", hms(secs), bm.by, l)),
                None => out.push_str(&format!("[{}] {} (no label)\n", hms(secs), bm.by)),
            }
        }
    }
    out
}

fn hms(secs: i64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Split `text` into chunks of at most `max_chars`, breaking on line
/// boundaries (a single over-long line becomes its own chunk).
pub fn chunk_lines(text: &str, max_chars: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        if !current.is_empty() && current.len() + line.len() + 1 > max_chars {
            chunks.push(std::mem::take(&mut current));
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Make a transcript fit one model call. Short transcripts pass through
/// untouched; long ones go through a map step (each chunk summarized with the
/// cheap fallback model, preserving speakers/decisions/numbers) whose outputs
/// are concatenated for the final reduce prompt. A 2h call ≈ 100k+ chars —
/// over the practical context budget — so this is required day one.
///
/// The map step runs chunks concurrently (bounded by [`CONDENSE_CONCURRENCY`]),
/// and repeats while the result still exceeds [`MAX_DIRECT_CHARS`], so even very
/// long calls converge to a single bounded final prompt instead of timing out.
pub async fn condense_transcript(
    groq: &Groq,
    ai: &AiConfig,
    mut text: String,
) -> Result<String, String> {
    for _ in 0..MAX_CONDENSE_PASSES {
        if text.len() <= MAX_DIRECT_CHARS {
            return Ok(text);
        }
        let chunks = chunk_lines(&text, CHUNK_CHARS);
        let total = chunks.len();
        let futs: Vec<_> = chunks
            .into_iter()
            .enumerate()
            .map(|(i, chunk)| {
                let groq = groq.clone();
                let model = ai.fallback_model.clone();
                async move {
                    let system =
                        "You condense meeting-transcript segments. Summarize the segment in \
                         detailed bullet points, preserving WHO said WHAT, all decisions, action \
                         items, numbers, dates and names. Keep the original language(s). Output \
                         only the bullets.";
                    let mut req = ChatRequest::new(model, system, chunk);
                    req.max_tokens = 1024;
                    req.timeout = Duration::from_secs(30);
                    req.max_retries = 3;
                    (i, groq.chat(req).await)
                }
            })
            .collect();
        // `buffered` yields results in input order, so segments stay in sequence.
        let results: Vec<(usize, Result<String, String>)> = futures::stream::iter(futs)
            .buffered(CONDENSE_CONCURRENCY)
            .collect()
            .await;
        let mut parts = Vec::with_capacity(total);
        for (i, res) in results {
            let summary =
                res.map_err(|e| format!("condense step {}/{} failed: {e}", i + 1, total))?;
            parts.push(format!("--- segment {}/{} ---\n{}", i + 1, total, summary));
        }
        text = parts.join("\n\n");
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcripts::{
        ExportBookmark, ExportEvent, ExportParticipant, ExportSession, TranscriptExport,
    };
    use chrono::{Duration as ChronoDuration, Utc};
    use std::collections::HashMap;
    use uuid::Uuid;

    fn export_fixture() -> TranscriptExport {
        let start = Utc::now();
        TranscriptExport {
            session: ExportSession {
                id: Uuid::new_v4(),
                room_name: "demo".into(),
                started_at: start,
                ended_at: Some(start + ChronoDuration::seconds(610)),
                duration_seconds: 610,
                participants: vec![ExportParticipant {
                    id: "p1".into(),
                    name: "Anna".into(),
                    language: "it".into(),
                }],
            },
            events: vec![
                ExportEvent {
                    kind: "speech".into(),
                    ts: start + ChronoDuration::seconds(65),
                    speaker_id: "p1".into(),
                    speaker_name: "Anna".into(),
                    lang: "it".into(),
                    original: "ciao a tutti".into(),
                    translations: HashMap::new(),
                },
                ExportEvent {
                    kind: "chat".into(),
                    ts: start + ChronoDuration::seconds(70),
                    speaker_id: "p1".into(),
                    speaker_name: "Anna".into(),
                    lang: "it".into(),
                    original: "link: example.com".into(),
                    translations: HashMap::new(),
                },
            ],
            bookmarks: vec![ExportBookmark {
                ts: start + ChronoDuration::seconds(66),
                label: Some("decisione importante".into()),
                by: "Anna".into(),
            }],
            exported_at: start,
        }
    }

    #[test]
    fn transcript_to_text_renders_events_chat_and_bookmarks() {
        let text = transcript_to_text(&export_fixture());
        assert!(text.contains("[00:01:05] Anna (it): ciao a tutti"));
        assert!(text.contains("[00:01:10] Anna (it) [chat]: link: example.com"));
        assert!(text.contains("BOOKMARKS"));
        assert!(text.contains("[00:01:06] Anna: decisione importante"));
    }

    #[test]
    fn billed_minutes_rounds_up_and_floors_at_one() {
        assert_eq!(billed_minutes(0), 1);
        assert_eq!(billed_minutes(59), 1);
        assert_eq!(billed_minutes(60), 1);
        assert_eq!(billed_minutes(61), 2);
        assert_eq!(billed_minutes(610), 11);
        assert_eq!(billed_minutes(-5), 1);
    }

    #[test]
    fn chunk_lines_splits_on_line_boundaries() {
        let text = "aaaa\nbbbb\ncccc\ndddd\n";
        let chunks = chunk_lines(text, 10);
        // Each line is 5 chars with its newline: two lines fit per 10-char chunk.
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "aaaa\nbbbb\n");
        assert_eq!(chunks[1], "cccc\ndddd\n");
        // Lossless: concatenation reproduces the input.
        assert_eq!(chunks.concat(), text);
        // A single over-long line still lands in exactly one chunk.
        let long = chunk_lines("xxxxxxxxxxxxxxxxxxxx", 10);
        assert_eq!(long.len(), 1);
    }
}
