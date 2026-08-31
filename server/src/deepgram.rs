//! Deepgram **batch** (prerecorded) transcription.
//!
//! No LIVE tier runs on Deepgram any more: calls, webinars, and the widget all stream to
//! Qwen-Omni Realtime ([`crate::engine::qwen`]). What survives here is the REST
//! `/v1/listen` work that has no realtime equivalent in our pipeline:
//!
//! * [`transcribe_file`] — uploaded chat audio (spec 0018) and voice messages
//! * [`transcribe_url_diarized`] — meeting recordings, with speaker diarization
//!
//! Consequently `DEEPGRAM_API_KEY` is now OPTIONAL: unset it and those features degrade,
//! but the server boots and every live tier works.
//!
//! [`SpeakerCtx`] also still lives here. It is engine-agnostic (every engine takes one),
//! so its home is now a misnomer — moving it to `engine::` is a mechanical follow-up kept
//! out of this change to keep the diff reviewable.

use std::time::Duration;

use uuid::Uuid;

use crate::config::Config;
use crate::glossary::GlossaryService;

/// Identity + context of the speaker behind one speaking session: who they are,
/// where they are, and which call session their words belong to.
pub struct SpeakerCtx {
    pub room: String,
    pub speaker_id: String,
    pub speaker_name: String,
    pub speaker_lang: String,
    /// The room's call-session id (transcript persistence).
    pub session_id: Uuid,
    /// `None` for guests.
    pub speaker_user_id: Option<Uuid>,
    /// Room-glossary handle (spec 0011); `None` without a database. Terms are
    /// resolved per utterance via the synchronous cache, so mid-call edits
    /// apply to the very next sentence.
    pub glossary: Option<GlossaryService>,
    /// Per-session override of how speech is cut into segments; `None` keeps the
    /// engine's configured defaults. See [`Segmentation`].
    pub segmentation: Option<Segmentation>,
}

/// How aggressively one session cuts speech into segments.
///
/// A global default cannot serve both shapes of conversation. A call wants captions to
/// land fast, and a clipped sentence is a caption problem. Talk to Anyone is two people
/// speaking through one device: there, a cut costs a beat of silence while the direction
/// of the NEXT fragment is worked out, so cutting often makes speech feel chopped. This
/// lets that one mode trade latency for whole sentences without moving the number under
/// every call and webinar in the product.
#[derive(Debug, Clone, Copy)]
pub struct Segmentation {
    /// Silence the provider's VAD needs before it calls a turn over.
    pub silence_duration_ms: u64,
    /// Gap with nothing arriving before we close a caption segment ourselves.
    pub segment_idle_ms: u64,
}

/// Transcribe a prerecorded audio file (spec 0018 chat upload). Asks the REST
/// `/v1/listen` endpoint for the full transcript with smart formatting, and returns both
/// the transcript and the detected language so the chat fan-out knows the source
/// language.
///
/// `content_type` is the uploaded file's MIME type (e.g. `audio/mpeg`,
/// `audio/wav`); Deepgram sniffs the container so we pass it through verbatim.
pub async fn transcribe_file(
    http: &reqwest::Client,
    config: &Config,
    bytes: Vec<u8>,
    content_type: &str,
) -> Result<(String, Option<String>), String> {
    let resp = http
        .post(
            "https://api.deepgram.com/v1/listen\
             ?detect_language=true&model=nova-2&smart_format=true&punctuate=true",
        )
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Token {}", config.deepgram_key),
        )
        .header(reqwest::header::CONTENT_TYPE, content_type)
        // Prerecorded files can be large; allow more time than the live probe.
        .timeout(Duration::from_secs(120))
        .body(bytes)
        .send()
        .await
        .map_err(|e| format!("deepgram transcribe request failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let detail = resp.text().await.unwrap_or_default();
        return Err(format!("deepgram transcribe returned {status}: {detail}"));
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("deepgram transcribe parse failed: {e}"))?;
    Ok(parse_prerecorded_response(&json))
}

/// Extract `(transcript, detected_language)` from a prerecorded `/v1/listen`
/// response. Pure, for tests. The transcript lives at
/// `results.channels[0].alternatives[0].transcript`; the language (when
/// `detect_language` was requested) at `results.channels[0].detected_language`.
/// A missing transcript yields an empty string (the caller still posts the file
/// chip, just without translatable text).
pub fn parse_prerecorded_response(json: &serde_json::Value) -> (String, Option<String>) {
    let channel = json.pointer("/results/channels/0");
    let transcript = channel
        .and_then(|c| c.pointer("/alternatives/0/transcript"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let lang = channel
        .and_then(|c| c.get("detected_language"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    (transcript, lang)
}

/// One diarized utterance from a prerecorded transcription (spec 0106 business
/// recordings): a single speaker's contiguous speech with millisecond offsets.
#[derive(Debug, Clone, PartialEq)]
pub struct DiarizedUtterance {
    /// Deepgram's numeric speaker label (0, 1, …).
    pub speaker: i64,
    pub text: String,
    pub start_ms: i64,
    pub end_ms: i64,
}

/// Result of a diarized prerecorded transcription.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DiarizedTranscript {
    pub utterances: Vec<DiarizedUtterance>,
    pub language: Option<String>,
    pub duration_seconds: Option<f64>,
}

/// Transcribe a prerecorded recording with **diarization** (spec 0106), fetched
/// by Deepgram from a (signed) URL rather than uploaded as bytes. This keeps the
/// recording — a full call **video**, up to ~1 GB on a long call — off the server
/// entirely: Deepgram pulls it straight from storage. Asks for speaker-labelled
/// utterances (`diarize=true&utterances=true`) so the post-call transcript can
/// attribute each segment to a speaker.
pub async fn transcribe_url_diarized(
    http: &reqwest::Client,
    config: &Config,
    media_url: &str,
) -> Result<DiarizedTranscript, String> {
    let resp = http
        .post(
            "https://api.deepgram.com/v1/listen\
             ?diarize=true&utterances=true&punctuate=true&smart_format=true\
             &detect_language=true&model=nova-2",
        )
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Token {}", config.deepgram_key),
        )
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        // Deepgram downloads + transcribes the full recording; allow generous time.
        .timeout(Duration::from_secs(300))
        .json(&serde_json::json!({ "url": media_url }))
        .send()
        .await
        .map_err(|e| format!("deepgram diarize request failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let detail = resp.text().await.unwrap_or_default();
        return Err(format!("deepgram diarize returned {status}: {detail}"));
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("deepgram diarize parse failed: {e}"))?;
    Ok(parse_diarized_response(&json))
}

/// Parse a diarized `/v1/listen` response into utterances + language + duration.
/// Pure, for tests. Utterances live at `results.utterances[]` (each with
/// `speaker`, `transcript`, `start`, `end` in seconds); duration at
/// `metadata.duration`; language at `results.channels[0].detected_language`.
pub fn parse_diarized_response(json: &serde_json::Value) -> DiarizedTranscript {
    let language = json
        .pointer("/results/channels/0/detected_language")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let duration_seconds = json.pointer("/metadata/duration").and_then(|v| v.as_f64());
    let utterances = json
        .pointer("/results/utterances")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|u| {
                    let text = u
                        .get("transcript")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim();
                    if text.is_empty() {
                        return None;
                    }
                    Some(DiarizedUtterance {
                        speaker: u.get("speaker").and_then(|v| v.as_i64()).unwrap_or(0),
                        text: text.to_string(),
                        start_ms: (u.get("start").and_then(|v| v.as_f64()).unwrap_or(0.0) * 1000.0)
                            as i64,
                        end_ms: (u.get("end").and_then(|v| v.as_f64()).unwrap_or(0.0) * 1000.0)
                            as i64,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    DiarizedTranscript {
        utterances,
        language,
        duration_seconds,
    }
}

/// Extract `(detected_language, confidence)` from a `/v1/listen` REST response.
/// Pure, for tests; the language lives at `results.channels[0].detected_language`.
/// `{"type":"CloseStream"}` to flush pending transcripts before closing.
///   `subtitle_final` with a `{ lang: text }` map; each client picks its own.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_prerecorded_extracts_transcript_and_lang() {
        let json = serde_json::json!({
            "results": { "channels": [{
                "detected_language": "it",
                "alternatives": [{ "transcript": "  buongiorno a tutti  ", "confidence": 0.9 }]
            }]}
        });
        let (text, lang) = parse_prerecorded_response(&json);
        assert_eq!(text, "buongiorno a tutti"); // trimmed
        assert_eq!(lang.as_deref(), Some("it"));
    }

    #[test]
    fn parse_prerecorded_tolerates_missing_fields() {
        // No transcript -> empty string, language still surfaced when present.
        let no_text = serde_json::json!({
            "results": { "channels": [{ "detected_language": "en" }] }
        });
        let (text, lang) = parse_prerecorded_response(&no_text);
        assert_eq!(text, "");
        assert_eq!(lang.as_deref(), Some("en"));

        // Completely empty response: empty transcript, no language.
        let (text, lang) = parse_prerecorded_response(&serde_json::json!({}));
        assert_eq!(text, "");
        assert!(lang.is_none());

        // Empty detected_language is treated as absent.
        let empty_lang = serde_json::json!({
            "results": { "channels": [{
                "detected_language": "",
                "alternatives": [{ "transcript": "hi" }]
            }]}
        });
        let (text, lang) = parse_prerecorded_response(&empty_lang);
        assert_eq!(text, "hi");
        assert!(lang.is_none());
    }
}
