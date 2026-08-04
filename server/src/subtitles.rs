//! SRT / WebVTT subtitle builders (spec 0012). Pure functions over the
//! transcript export — no database, no IO; the HTTP layer in `api.rs` handles
//! auth and fetching.
//!
//! Segmentation rules:
//! - chat events are skipped (subtitles cover speech only)
//! - cue start = event ts − session start; estimated duration = chars / 15 cps
//! - cue end = min(next event start − 100 ms, start + duration), then clamped
//!   to ≥ 1.5 s display and ≤ 10 s
//! - long texts are wrapped at word boundaries into lines of ≤ 42 chars and
//!   grouped two lines per cue; the event window is divided among the chunks
//!   proportionally to their length, with 100 ms gaps in between
//! - `both` mode pairs the full original with the full translation (one line
//!   each); the wrap/split rules apply to the monolingual modes only, since a
//!   bilingual pair has to stay aligned

use chrono::{DateTime, Utc};

use crate::transcripts::ExportEvent;

/// `?lang=` — which text each cue carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LangMode {
    /// What the speaker actually said.
    Original,
    /// `translations[target]`, falling back to the original when the speaker
    /// already spoke the target language (no self-translation is stored).
    Translated,
    /// Original on top, translation below.
    Both,
}

impl LangMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "original" => Some(Self::Original),
            "translated" => Some(Self::Translated),
            "both" => Some(Self::Both),
            _ => None,
        }
    }
}

/// Max characters per rendered subtitle line.
const MAX_LINE_CHARS: usize = 42;
/// Reading speed used for duration estimates (characters per second).
const READ_CPS: i64 = 15;
const MIN_CUE_MS: i64 = 1_500;
const MAX_CUE_MS: i64 = 10_000;
const CUE_GAP_MS: i64 = 100;

/// One subtitle cue, format-agnostic. `lines` is 1–2 rendered lines.
#[derive(Debug, PartialEq, Eq)]
pub struct Cue {
    pub start_ms: i64,
    pub end_ms: i64,
    pub speaker: String,
    pub lines: Vec<String>,
}

/// Turn transcript events into timed cues. `target` is the translation
/// language for [`LangMode::Translated`] / [`LangMode::Both`].
pub fn compute_cues(
    events: &[ExportEvent],
    started_at: DateTime<Utc>,
    mode: LangMode,
    target: &str,
) -> Vec<Cue> {
    let speech: Vec<&ExportEvent> = events.iter().filter(|e| e.kind != "chat").collect();
    let starts: Vec<i64> = speech
        .iter()
        .map(|e| (e.ts - started_at).num_milliseconds().max(0))
        .collect();

    let mut cues = Vec::new();
    for (i, ev) in speech.iter().enumerate() {
        let original = normalize(&ev.original);
        if original.is_empty() {
            continue;
        }
        let translated = ev.translations.get(target).map(|t| normalize(t));

        // Event window per the timing rules above.
        let start = starts[i];
        let window_chars = match mode {
            LangMode::Original => original.chars().count(),
            LangMode::Translated => translated.as_deref().unwrap_or(&original).chars().count(),
            LangMode::Both => {
                original.chars().count() + translated.as_deref().map_or(0, |t| t.chars().count())
            }
        } as i64;
        let mut end = start + window_chars * 1000 / READ_CPS;
        if let Some(next) = starts.get(i + 1) {
            end = end.min(next - CUE_GAP_MS);
        }
        end = end.max(start + MIN_CUE_MS).min(start + MAX_CUE_MS);

        match mode {
            LangMode::Both => {
                let mut lines = vec![original];
                if let Some(t) = translated.filter(|t| *t != lines[0]) {
                    lines.push(t);
                }
                cues.push(Cue {
                    start_ms: start,
                    end_ms: end,
                    speaker: ev.speaker_name.clone(),
                    lines,
                });
            }
            LangMode::Original | LangMode::Translated => {
                let text = match mode {
                    LangMode::Translated => translated.unwrap_or(original),
                    _ => original,
                };
                let lines = wrap_lines(&text, MAX_LINE_CHARS);
                let chunks: Vec<&[String]> = lines.chunks(2).collect();
                let total: i64 = chunks.iter().map(|c| chunk_chars(c)).sum();
                let span = end - start;
                let mut t0 = start;
                let mut acc = 0i64;
                for (ci, chunk) in chunks.iter().enumerate() {
                    acc += chunk_chars(chunk);
                    let last = ci == chunks.len() - 1;
                    // Proportional boundary; the last chunk closes the window.
                    let boundary = if last {
                        end
                    } else {
                        start + span * acc / total.max(1)
                    };
                    let c_end = if last {
                        boundary
                    } else {
                        (boundary - CUE_GAP_MS).max(t0 + 1)
                    };
                    cues.push(Cue {
                        start_ms: t0,
                        end_ms: c_end,
                        speaker: ev.speaker_name.clone(),
                        lines: chunk.to_vec(),
                    });
                    t0 = boundary;
                }
            }
        }
    }
    cues
}

/// Collapse all whitespace runs (incl. newlines) to single spaces.
fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn chunk_chars(lines: &[String]) -> i64 {
    lines
        .iter()
        .map(|l| l.chars().count() as i64)
        .sum::<i64>()
        .max(1)
}

/// Greedy word-wrap into lines of at most `width` chars; words longer than
/// `width` are hard-split so no line ever exceeds the limit.
fn wrap_lines(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;
    for word in text.split_whitespace() {
        let mut rest: Vec<char> = word.chars().collect();
        while rest.len() > width {
            // Hard split an over-long word; flush the current line first.
            if cur_len > 0 {
                lines.push(std::mem::take(&mut cur));
                cur_len = 0;
            }
            lines.push(rest[..width].iter().collect());
            rest = rest[width..].to_vec();
        }
        let wlen = rest.len();
        if wlen == 0 {
            continue;
        }
        if cur_len == 0 {
            cur = rest.into_iter().collect();
            cur_len = wlen;
        } else if cur_len + 1 + wlen <= width {
            cur.push(' ');
            cur.extend(rest);
            cur_len += 1 + wlen;
        } else {
            lines.push(std::mem::take(&mut cur));
            cur = rest.into_iter().collect();
            cur_len = wlen;
        }
    }
    if cur_len > 0 {
        lines.push(cur);
    }
    lines
}

/// Render cues as SubRip. The speaker is prefixed to the first line
/// (`Name: text`) — SRT has no markup for voices.
pub fn build_srt(cues: &[Cue]) -> String {
    let mut out = String::new();
    for (i, c) in cues.iter().enumerate() {
        out.push_str(&format!(
            "{}\n{} --> {}\n",
            i + 1,
            timestamp(c.start_ms, ','),
            timestamp(c.end_ms, ',')
        ));
        for (j, line) in c.lines.iter().enumerate() {
            if j == 0 {
                out.push_str(&c.speaker);
                out.push_str(": ");
            }
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

/// Render cues as WebVTT with `<v Speaker>` voice tags.
///
/// The `NOTE` block is the AI Act Art. 50(2) marking for the downloaded file: a standard
/// WebVTT comment, ignored by every player, readable by anything that parses the file.
/// SRT deliberately gets no equivalent — the format has no comment syntax, and inventing
/// one breaks strict parsers for a marking the standard-editing carve-out likely does not
/// even require. See `docs/eu-ai-act-compliance.md`.
pub fn build_vtt(cues: &[Cue]) -> String {
    let mut out = String::from("WEBVTT\n\nNOTE AI-generated: transcribed and translated by AI (VoxTranslate); may contain errors.\n\n");
    for c in cues {
        out.push_str(&format!(
            "{} --> {}\n",
            timestamp(c.start_ms, '.'),
            timestamp(c.end_ms, '.')
        ));
        out.push_str(&format!("<v {}>", escape_vtt(&c.speaker)));
        for (j, line) in c.lines.iter().enumerate() {
            if j > 0 {
                out.push('\n');
            }
            out.push_str(&escape_vtt(line));
        }
        out.push_str("\n\n");
    }
    out
}

/// `HH:MM:SS{sep}mmm` — SRT uses a comma, VTT a period.
fn timestamp(ms: i64, sep: char) -> String {
    let ms = ms.max(0);
    format!(
        "{:02}:{:02}:{:02}{}{:03}",
        ms / 3_600_000,
        (ms / 60_000) % 60,
        (ms / 1000) % 60,
        sep,
        ms % 1000
    )
}

/// Escape VTT cue-text metacharacters (speaker names and spoken text are
/// user-controlled — a literal `>` would break out of the `<v>` tag).
fn escape_vtt(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn ev(ts_ms: i64, lang: &str, original: &str, translations: &[(&str, &str)]) -> ExportEvent {
        ExportEvent {
            kind: "speech".into(),
            ts: t0() + chrono::Duration::milliseconds(ts_ms),
            speaker_id: "p1".into(),
            speaker_name: "Alice".into(),
            lang: lang.into(),
            original: original.into(),
            translations: translations
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<HashMap<_, _>>(),
        }
    }

    fn t0() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-06-10T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn lang_mode_parses() {
        assert_eq!(LangMode::parse("original"), Some(LangMode::Original));
        assert_eq!(LangMode::parse("translated"), Some(LangMode::Translated));
        assert_eq!(LangMode::parse("both"), Some(LangMode::Both));
        assert_eq!(LangMode::parse("klingon"), None);
    }

    #[test]
    fn short_cue_gets_min_display_time() {
        let events = vec![ev(2_000, "en", "Hi.", &[])];
        let cues = compute_cues(&events, t0(), LangMode::Original, "it");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ms, 2_000);
        assert_eq!(cues[0].end_ms, 3_500, "clamped to 1.5 s display");
        assert_eq!(cues[0].lines, vec!["Hi."]);
        assert_eq!(cues[0].speaker, "Alice");
    }

    #[test]
    fn reading_speed_sets_duration() {
        // 30 chars at 15 cps = 2 s — longer than the 1.5 s floor.
        let text = "abcde ".repeat(5); // normalized to 29 chars
        let events = vec![ev(0, "en", &text, &[])];
        let cues = compute_cues(&events, t0(), LangMode::Original, "it");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].end_ms, 29 * 1000 / 15);
    }

    #[test]
    fn crowded_events_keep_100ms_gap() {
        // Long first text (reading time 4 s) but the next event lands at 2 s.
        let events = vec![
            ev(0, "en", &"a ".repeat(30), &[]),
            ev(2_000, "en", "Next.", &[]),
        ];
        let cues = compute_cues(&events, t0(), LangMode::Original, "it");
        let first_end = cues[..cues.len() - 1]
            .iter()
            .map(|c| c.end_ms)
            .max()
            .unwrap();
        assert!(
            first_end <= 1_900,
            "first event ends 100ms before the next: {first_end}"
        );
        assert_eq!(cues.last().unwrap().start_ms, 2_000);
    }

    #[test]
    fn long_text_splits_into_wrapped_chunks() {
        let text = "The quick brown fox jumps over the lazy dog and keeps on running \
                    through the endless green fields towards the bright golden horizon";
        let events = vec![ev(0, "en", text, &[])];
        let cues = compute_cues(&events, t0(), LangMode::Original, "it");
        assert!(cues.len() >= 2, "131 chars split into multiple cues");
        for c in &cues {
            assert!(c.lines.len() <= 2, "max two lines per cue");
            for line in &c.lines {
                assert!(line.chars().count() <= 42, "line too long: {line:?}");
            }
        }
        // Chunks are contiguous, ordered, gapped, and inside the event window.
        for pair in cues.windows(2) {
            assert!(pair[0].end_ms < pair[1].start_ms, "cues overlap");
            assert!(pair[1].start_ms - pair[0].end_ms >= CUE_GAP_MS);
        }
        assert_eq!(cues[0].start_ms, 0);
        let total_ms = cues.last().unwrap().end_ms;
        assert!(total_ms <= text.len() as i64 * 1000 / 15 + 1);
        // Re-joined lines reproduce the normalized text.
        let joined = cues
            .iter()
            .flat_map(|c| c.lines.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(joined, normalize(text));
    }

    #[test]
    fn no_cue_exceeds_ten_seconds() {
        let text = "word ".repeat(60); // 300 chars ≈ 20 s of reading time
        let events = vec![ev(0, "en", &text, &[])];
        let cues = compute_cues(&events, t0(), LangMode::Original, "it");
        for c in &cues {
            assert!(c.end_ms - c.start_ms <= MAX_CUE_MS, "cue too long");
        }
    }

    #[test]
    fn chat_events_are_skipped() {
        let mut chat = ev(0, "en", "a chat line", &[]);
        chat.kind = "chat".into();
        let events = vec![chat, ev(1_000, "en", "Spoken.", &[])];
        let cues = compute_cues(&events, t0(), LangMode::Original, "it");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].lines, vec!["Spoken."]);
    }

    #[test]
    fn translated_mode_uses_target_and_falls_back() {
        let events = vec![
            ev(0, "en", "Hello world.", &[("it", "Ciao mondo.")]),
            // Speaker already speaks the target -> no stored translation.
            ev(5_000, "it", "Va bene.", &[("en", "All right.")]),
        ];
        let cues = compute_cues(&events, t0(), LangMode::Translated, "it");
        assert_eq!(cues[0].lines, vec!["Ciao mondo."]);
        assert_eq!(
            cues[1].lines,
            vec!["Va bene."],
            "falls back to the original"
        );
    }

    #[test]
    fn both_mode_pairs_original_and_translation() {
        let events = vec![ev(0, "en", "Hello world.", &[("it", "Ciao mondo.")])];
        let cues = compute_cues(&events, t0(), LangMode::Both, "it");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].lines, vec!["Hello world.", "Ciao mondo."]);
        // Missing translation -> single line, no duplicate.
        let cues = compute_cues(&events, t0(), LangMode::Both, "fr");
        assert_eq!(cues[0].lines, vec!["Hello world."]);
    }

    #[test]
    fn srt_output_shape() {
        let events = vec![
            ev(1_000, "en", "Hello there.", &[]),
            ev(6_000, "en", "Bye.", &[]),
        ];
        let srt = build_srt(&compute_cues(&events, t0(), LangMode::Original, "it"));
        assert_eq!(
            srt,
            "1\n00:00:01,000 --> 00:00:02,500\nAlice: Hello there.\n\n\
             2\n00:00:06,000 --> 00:00:07,500\nAlice: Bye.\n\n"
        );
    }

    #[test]
    fn vtt_output_shape_and_escaping() {
        let mut e = ev(0, "en", "Tags <b> & such.", &[]);
        e.speaker_name = "A<l>ice & Bob".into();
        let vtt = build_vtt(&compute_cues(&[e], t0(), LangMode::Original, "it"));
        assert!(vtt.starts_with("WEBVTT\n\n"), "{vtt}");
        // AI Act Art. 50(2) marking: a NOTE block before the first cue.
        assert!(vtt.contains("\nNOTE AI-generated:"), "{vtt}");
        assert!(
            vtt.find("NOTE AI-generated:") < vtt.find("00:00:00.000 -->"),
            "the marking must precede the first cue: {vtt}"
        );
        assert!(vtt.contains("00:00:00.000 --> 00:00:01.500"), "{vtt}");
        assert!(
            vtt.contains("<v A&lt;l&gt;ice &amp; Bob>Tags &lt;b&gt; &amp; such."),
            "{vtt}"
        );
    }

    #[test]
    fn timestamp_rolls_over_an_hour() {
        assert_eq!(timestamp(3_661_234, ','), "01:01:01,234");
        assert_eq!(timestamp(59_999, '.'), "00:00:59.999");
        assert_eq!(timestamp(-5, ','), "00:00:00,000");
    }

    #[test]
    fn wrap_hard_splits_overlong_words() {
        let lines = wrap_lines(&"x".repeat(100), 42);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].len(), 42);
        assert_eq!(lines[2].len(), 16);
    }
}
