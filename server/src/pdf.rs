//! Transcript PDF rendering (spec 0009) via an embedded typst engine.
//!
//! Injection safety (load-bearing): ALL user data crosses into typst as ONE
//! JSON string through `sys.inputs.data` — the template decodes it with
//! `json(bytes(..))`, and decoded strings render literally. Never `format!`
//! user text into typst markup.

use std::collections::HashMap;
use std::sync::OnceLock;

use typst::foundations::{Dict, IntoValue};
use typst_as_lib::{TypstEngine, TypstTemplateMainFile};
use typst_layout::PagedDocument;

use crate::transcripts::TranscriptExport;

/// Template + fonts are compiled into the binary, so the engine has no
/// filesystem or network access at render time (default features only).
static ENGINE: OnceLock<TypstEngine<TypstTemplateMainFile>> = OnceLock::new();

fn engine() -> &'static TypstEngine<TypstTemplateMainFile> {
    ENGINE.get_or_init(|| {
        TypstEngine::builder()
            .main_file(include_str!("templates/transcript.typ"))
            .fonts([
                include_bytes!("../assets/fonts/NotoSans-Regular.ttf").as_slice(),
                include_bytes!("../assets/fonts/NotoSans-Bold.ttf").as_slice(),
                include_bytes!("../assets/fonts/NotoSansSC-Regular.otf").as_slice(),
                include_bytes!("../assets/fonts/NotoSansJP-Regular.otf").as_slice(),
            ])
            .build()
    })
}

pub struct PdfRender {
    pub bytes: Vec<u8>,
    /// Page count of the compiled document (asserted in tests).
    pub pages: usize,
}

/// Compile the JSON document (shape produced by [`build_pdf_doc`]) to PDF.
/// CPU-bound — call through `tokio::task::spawn_blocking` from handlers.
pub fn render_transcript_pdf(doc_json: &str) -> Result<PdfRender, String> {
    let mut inputs = Dict::new();
    inputs.insert("data".into(), doc_json.into_value());
    let doc: PagedDocument = engine()
        .compile_with_input(inputs)
        .output
        .map_err(|e| format!("typst compile failed: {e}"))?;
    let bytes = typst_pdf::pdf(&doc, &typst_pdf::PdfOptions::default())
        .map_err(|e| format!("pdf export failed: {e:?}"))?;
    Ok(PdfRender {
        bytes,
        pages: doc.pages().len(),
    })
}

/// Pre-render the export into the flat JSON shape `transcript.typ` consumes:
/// times localized to `tz`, exactly one translation per event (the requester's
/// `lang`, omitted when missing or identical to the original), and a stable
/// color index per speaker (participant join order).
pub fn build_pdf_doc(
    export: &TranscriptExport,
    tz: chrono_tz::Tz,
    lang: &str,
) -> serde_json::Value {
    let color_of: HashMap<&str, usize> = export
        .session
        .participants
        .iter()
        .enumerate()
        .map(|(i, p)| (p.id.as_str(), i))
        .collect();

    let participants: Vec<serde_json::Value> = export
        .session
        .participants
        .iter()
        .enumerate()
        .map(|(i, p)| serde_json::json!({ "name": p.name, "lang": p.language, "color": i }))
        .collect();

    // Events and bookmark markers interleave chronologically. Both go through
    // the same JSON channel, so bookmark labels are injection-safe like all
    // other user text.
    let mut timeline: Vec<(chrono::DateTime<chrono::Utc>, serde_json::Value)> = export
        .events
        .iter()
        .map(|ev| {
            let translation = ev.translations.get(lang).filter(|t| **t != ev.original);
            (
                ev.ts,
                serde_json::json!({
                    "marker": false,
                    "time": ev.ts.with_timezone(&tz).format("%H:%M:%S").to_string(),
                    "speaker": ev.speaker_name,
                    "color": color_of.get(ev.speaker_id.as_str()).copied().unwrap_or(0),
                    "chat": ev.kind == "chat",
                    "original": ev.original,
                    "translation": translation,
                }),
            )
        })
        .chain(export.bookmarks.iter().map(|b| {
            (
                b.ts,
                serde_json::json!({
                    "marker": true,
                    "time": b.ts.with_timezone(&tz).format("%H:%M:%S").to_string(),
                    "by": b.by,
                    "label": b.label,
                }),
            )
        }))
        .collect();
    timeline.sort_by_key(|(ts, _)| *ts);
    let events: Vec<serde_json::Value> = timeline.into_iter().map(|(_, v)| v).collect();

    serde_json::json!({
        "title": "Call Transcript",
        "room": export.session.room_name,
        "meta": [
            {
                "label": "Date",
                "value": export.session.started_at.with_timezone(&tz).format("%Y-%m-%d %H:%M %Z").to_string(),
            },
            {
                "label": "Duration",
                "value": format_duration(export.session.duration_seconds),
            },
        ],
        "participants_label": "Participants",
        "participants": participants,
        "bookmark_label": "BOOKMARK",
        "events": events,
        "empty_label": "No transcript events were recorded during this call.",
        "footer": format!(
            "Generated by VoxTranslate · {}",
            export.exported_at.format("%Y-%m-%d %H:%M UTC")
        ),
    })
}

fn format_duration(total_seconds: i64) -> String {
    let total = total_seconds.max(0);
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}h {m:02}m {s:02}s")
    } else {
        format!("{m}m {s:02}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcripts::{
        ExportBookmark, ExportEvent, ExportParticipant, ExportSession, TranscriptExport,
    };
    use chrono::{Duration, TimeZone, Utc};
    use std::collections::HashMap;
    use uuid::Uuid;

    fn event(kind: &str, speaker: usize, text: &str, lang: &str, secs: i64) -> ExportEvent {
        ExportEvent {
            kind: kind.into(),
            ts: Utc.with_ymd_and_hms(2026, 6, 10, 12, 0, 0).unwrap() + Duration::seconds(secs),
            speaker_id: format!("peer-{speaker}"),
            speaker_name: format!("Speaker {speaker}"),
            lang: lang.into(),
            original: text.into(),
            translations: HashMap::new(),
        }
    }

    fn export(room: &str, events: Vec<ExportEvent>) -> TranscriptExport {
        let started_at = Utc.with_ymd_and_hms(2026, 6, 10, 12, 0, 0).unwrap();
        TranscriptExport {
            session: ExportSession {
                id: Uuid::nil(),
                room_name: room.into(),
                started_at,
                ended_at: Some(started_at + Duration::seconds(3725)),
                duration_seconds: 3725,
                participants: vec![
                    ExportParticipant {
                        id: "peer-0".into(),
                        name: "Speaker 0".into(),
                        language: "it".into(),
                    },
                    ExportParticipant {
                        id: "peer-1".into(),
                        name: "Speaker 1".into(),
                        language: "ja".into(),
                    },
                ],
            },
            events,
            bookmarks: vec![],
            exported_at: started_at + Duration::seconds(4000),
        }
    }

    fn render(export: &TranscriptExport) -> PdfRender {
        let doc = build_pdf_doc(export, chrono_tz::Europe::Rome, "en");
        render_transcript_pdf(&serde_json::to_string(&doc).unwrap()).expect("render ok")
    }

    #[test]
    fn renders_speech_chat_and_cjk() {
        let mut speech = event("speech", 0, "ciao a tutti, come va?", "it", 5);
        speech
            .translations
            .insert("en".into(), "hi everyone, how are you?".into());
        let ja = event("speech", 1, "こんにちは、元気ですか", "ja", 12);
        let zh = event("chat", 1, "你好世界 — chat in 中文", "zh", 20);
        let pdf = render(&export("daily-standup", vec![speech, ja, zh]));
        assert!(pdf.bytes.starts_with(b"%PDF-"), "PDF magic bytes");
        assert!(pdf.pages >= 1);
    }

    #[test]
    fn long_transcript_spans_multiple_pages() {
        let events = (0..300)
            .map(|i| {
                event(
                    if i % 5 == 0 { "chat" } else { "speech" },
                    i % 2,
                    &format!("line {i}: the quick brown fox jumps over the lazy dog"),
                    "en",
                    i as i64,
                )
            })
            .collect();
        let pdf = render(&export("marathon", events));
        assert!(pdf.bytes.starts_with(b"%PDF-"));
        assert!(pdf.pages >= 2, "300 events fit on {} page(s)?", pdf.pages);
    }

    #[test]
    fn empty_transcript_is_one_friendly_page() {
        let pdf = render(&export("quiet-room", vec![]));
        assert!(pdf.bytes.starts_with(b"%PDF-"));
        assert_eq!(pdf.pages, 1);
    }

    /// Dev affordance, not a CI test: dump a sample PDF for visual inspection.
    /// `cargo test --lib pdf -- --ignored` -> target/sample-transcript.pdf
    #[test]
    #[ignore]
    fn dump_sample_pdf() {
        let mut speech = event("speech", 0, "ciao a tutti, come va oggi?", "it", 5);
        speech
            .translations
            .insert("en".into(), "hi everyone, how is it going today?".into());
        let mut ja = event(
            "speech",
            1,
            "こんにちは、元気ですか。今日は良い天気ですね。",
            "ja",
            12,
        );
        ja.translations.insert(
            "en".into(),
            "Hello, how are you? Nice weather today.".into(),
        );
        let mut zh = event("chat", 1, "你好世界，这是一条聊天消息", "zh", 20);
        zh.translations
            .insert("en".into(), "Hello world, this is a chat message".into());
        let chat = event(
            "chat",
            0,
            "perfetto, ci vediamo dopo la chiamata!",
            "it",
            30,
        );
        let pdf = render(&export("daily-standup", vec![speech, ja, zh, chat]));
        std::fs::write("target/sample-transcript.pdf", &pdf.bytes).unwrap();
    }

    #[test]
    fn bookmarks_interleave_as_marker_rows() {
        let base = Utc.with_ymd_and_hms(2026, 6, 10, 12, 0, 0).unwrap();
        let mut doc = export(
            "review",
            vec![event("speech", 0, "important point", "en", 5)],
        );
        doc.bookmarks = vec![
            ExportBookmark {
                ts: base + Duration::seconds(6),
                label: Some("decision made".into()),
                by: "Speaker 0".into(),
            },
            ExportBookmark {
                ts: base + Duration::seconds(8),
                label: None,
                by: "Speaker 1".into(),
            },
            // Labels are user text — must render literally like everything else.
            ExportBookmark {
                ts: base + Duration::seconds(9),
                label: Some("#eval(\"1+1\") ] #pagebreak() [ *bold?*".into()),
                by: "] #pagebreak() [".into(),
            },
        ];
        // The marker lands between the two stream positions chronologically.
        let json = build_pdf_doc(&doc, chrono_tz::UTC, "en");
        let rows = json["events"].as_array().unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0]["marker"], false);
        assert_eq!(rows[1]["marker"], true);
        assert_eq!(rows[1]["label"], "decision made");
        let pdf = render(&doc);
        assert!(pdf.bytes.starts_with(b"%PDF-"));
        assert_eq!(pdf.pages, 1, "injection fixture altered layout");
    }

    #[test]
    fn user_text_never_executes_as_typst() {
        // Markup, code-eval, and bracket fixtures must render literally, not
        // change the document structure or fail to compile.
        let fixtures = [
            "#eval(\"1+1\")",
            "*bold?* _italic?_ `code?`",
            "] #pagebreak() [",
            "\\u{0} backslash \\ and \"quotes\"",
            "$ integral x dif x $",
        ];
        for fixture in fixtures {
            let ev = event("chat", 0, fixture, "en", 1);
            let mut room = export(fixture, vec![ev]);
            room.session.participants[0].name = fixture.into();
            let pdf = render(&room);
            assert!(pdf.bytes.starts_with(b"%PDF-"), "fixture {fixture:?}");
            assert_eq!(pdf.pages, 1, "fixture {fixture:?} altered layout");
        }
    }
}
