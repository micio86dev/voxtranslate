//! Low-level OpenAI Realtime API client for the B2B Voice Assistant.
//!
//! Connects to `wss://api.openai.com/v1/realtime?model=…` (NOT the
//! `/realtime/translations` endpoint used by the Pro tier) and exchanges
//! general-purpose voice turns with the model.
//!
//! Pure functions (`build_session_update_json`, `parse_realtime_event`,
//! `va_audio_append_json`, `build_cost_tick_json`, `build_session_end_json`,
//! `build_rag_instructions`) are unit-tested without a live connection.
//! `open_va_session` requires a real key and is exercised only by the
//! `#[ignore]`d integration test.

use base64::Engine as _;
use futures::stream::{SplitSink, SplitStream};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{HeaderValue, AUTHORIZATION};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::config::VoiceAssistantConfig;

type VaStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
pub type VaSink = SplitSink<VaStream, Message>;
pub type VaSource = SplitStream<VaStream>;

// ---------------------------------------------------------------------------
// Event enum (server → voice_assistant relay)
// ---------------------------------------------------------------------------

/// A parsed OpenAI Realtime server event. Only the frames the relay acts on are
/// modelled; everything else (session.created / updated, …) is [`Other`].
///
/// [`Other`]: VaEvent::Other
#[derive(Debug, Clone, PartialEq)]
pub enum VaEvent {
    /// Decoded PCM16 audio chunk from the AI response.
    AudioDelta(Vec<u8>),
    /// Text token from the AI response.
    TextDelta(String),
    /// Transcript token of the AI's spoken words.
    TranscriptDelta(String),
    /// User started speaking (VAD).
    SpeechStarted,
    /// User stopped speaking (VAD).
    SpeechStopped,
    /// The AI finished its current turn.
    ResponseDone,
    /// Server-side error message.
    Error(String),
    /// Lifecycle or unknown frame — not acted on.
    Other,
}

// ---------------------------------------------------------------------------
// Pure builders
// ---------------------------------------------------------------------------

/// Build the `session.update` payload that configures a voice-assistant session.
///
/// The session is set up with:
/// - `modalities`: `["text", "audio"]`
/// - `instructions`: the RAG-grounded system instructions
/// - `voice`: caller-supplied (default `"alloy"`, env-configurable)
/// - `input_audio_format` / `output_audio_format`: `"pcm16"`
/// - `input_audio_transcription`: Whisper-1 (so the relay gets the user's spoken
///   text and can persist it to `voice_assistant_interactions`)
/// - `turn_detection`: server VAD (no manual push-to-talk)
pub fn build_session_update_json(instructions: &str, voice: &str) -> String {
    serde_json::json!({
        "type": "session.update",
        "session": {
            "type": "conversation",
            "modalities": ["text", "audio"],
            "instructions": instructions,
            "voice": voice,
            "input_audio_format": "pcm16",
            "output_audio_format": "pcm16",
            "input_audio_transcription": {
                "model": "whisper-1"
            },
            "turn_detection": {
                "type": "server_vad"
            }
        }
    })
    .to_string()
}

/// Wrap a PCM16 chunk in an `input_audio_buffer.append` frame for the Realtime API.
///
/// **Note**: the voice-assistant endpoint uses `input_audio_buffer.append` (NOT
/// `session.input_audio_buffer.append` — that is the translations endpoint's frame).
pub fn va_audio_append_json(pcm16: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(pcm16);
    serde_json::json!({
        "type": "input_audio_buffer.append",
        "audio": b64,
    })
    .to_string()
}

/// Build a `cost_tick` control event emitted to the browser every 10 seconds.
///
/// Schema:
/// ```json
/// { "type": "cost_tick", "duration_s": 42, "credits_so_far": 15, "cost_display": "€0.03" }
/// ```
pub fn build_cost_tick_json(duration_s: u64, credits_so_far: i32, cost_display: &str) -> String {
    serde_json::json!({
        "type": "cost_tick",
        "duration_s": duration_s,
        "credits_so_far": credits_so_far,
        "cost_display": cost_display,
    })
    .to_string()
}

/// Build a `session_end` control event emitted when the voice-assistant session closes.
///
/// Schema:
/// ```json
/// { "type": "session_end", "duration_s": 72, "credits_used": 25, "cost_display": "€0.05" }
/// ```
pub fn build_session_end_json(duration_s: u64, credits_used: i32, cost_display: &str) -> String {
    serde_json::json!({
        "type": "session_end",
        "duration_s": duration_s,
        "credits_used": credits_used,
        "cost_display": cost_display,
    })
    .to_string()
}

/// Build the system `instructions` string injected into `session.update`, grounded
/// by RAG context chunks (from `transcript_embeddings`).
///
/// When `chunks` is empty, a notice is injected so the assistant answers from
/// general knowledge without fabricating transcript content (product decision 4).
pub fn build_rag_instructions(
    chunks: &[String],
    org_name: &str,
    project_name: Option<&str>,
    member_name: Option<&str>,
) -> String {
    let mut instructions = format!(
        "You are a helpful voice assistant for {org_name}. \
         You answer questions about the organization's calls, projects, and team data. \
         Ground EVERY claim in the provided transcript excerpts — never invent facts \
         beyond the evidence. Be concise and conversational (you are speaking, not writing). \
         Do not reveal raw transcript text verbatim; instead, synthesize and summarize."
    );
    if let Some(proj) = project_name {
        instructions.push_str(&format!("\nFocus on project: {proj}."));
    }
    if let Some(member) = member_name {
        instructions.push_str(&format!("\nFocus on team member: {member}."));
    }
    if chunks.is_empty() {
        instructions.push_str(
            "\n\nKNOWLEDGE BASE: There are no prior transcriptions available for this scope. \
             Answer from general knowledge and let the user know that no prior transcriptions \
             exist in the system yet.",
        );
    } else {
        instructions.push_str("\n\nTRANSCRIPT EXCERPTS (most relevant):\n");
        for chunk in chunks {
            instructions.push_str(chunk);
            instructions.push('\n');
        }
    }
    instructions
}

// ---------------------------------------------------------------------------
// Event parser
// ---------------------------------------------------------------------------

/// Parse one OpenAI Realtime server frame into a [`VaEvent`].
///
/// Unknown shapes and undecodable audio fall back to [`VaEvent::Other`] rather
/// than erroring, so a stray frame never kills the relay.
pub fn parse_realtime_event(text: &str) -> VaEvent {
    let v: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return VaEvent::Other,
    };
    let delta = || {
        v.get("delta")
            .and_then(|d| d.as_str())
            .unwrap_or_default()
            .to_string()
    };
    match v.get("type").and_then(|t| t.as_str()).unwrap_or_default() {
        "response.audio.delta" => {
            let d = delta();
            match base64::engine::general_purpose::STANDARD.decode(&d) {
                Ok(bytes) => VaEvent::AudioDelta(bytes),
                Err(_) => VaEvent::Other,
            }
        }
        "response.text.delta" => VaEvent::TextDelta(delta()),
        "response.audio_transcript.delta" => VaEvent::TranscriptDelta(delta()),
        "input_audio_buffer.speech_started" => VaEvent::SpeechStarted,
        "input_audio_buffer.speech_stopped" => VaEvent::SpeechStopped,
        "response.done" => VaEvent::ResponseDone,
        "error" => {
            let msg = v
                .get("message")
                .and_then(|m| m.as_str())
                .or_else(|| v.pointer("/error/message").and_then(|m| m.as_str()))
                .unwrap_or("openai realtime error");
            VaEvent::Error(msg.to_string())
        }
        _ => VaEvent::Other,
    }
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

/// Open a voice-assistant WebSocket session to the OpenAI Realtime API and send
/// the initial `session.update`. Returns the split sink/source.
///
/// The URL is `wss://api.openai.com/v1/realtime?model={config.model}`.
/// This is the general-purpose realtime endpoint (NOT the translations endpoint
/// used by the Pro tier).
pub async fn open_va_session(config: &VoiceAssistantConfig) -> Result<(VaSink, VaSource), String> {
    use futures::StreamExt as _;

    let url = format!("wss://api.openai.com/v1/realtime?model={}", config.model);
    let mut request = url
        .into_client_request()
        .map_err(|e| format!("invalid voice assistant url: {e}"))?;
    let auth = HeaderValue::from_str(&format!("Bearer {}", config.api_key))
        .map_err(|e| format!("invalid voice assistant key header: {e}"))?;
    request.headers_mut().insert(AUTHORIZATION, auth);

    let (ws, _resp) = connect_async(request)
        .await
        .map_err(|e| format!("voice assistant connect failed: {e}"))?;

    let (sink, source) = ws.split();
    tracing::info!(model = %config.model, "voice_assistant: session connected");
    Ok((sink, source))
}

/// Format a credit cost as a euro display string (e.g. `"€0.03"`).
///
/// Assumes 100 credits = €1 (the org credit unit rate). This is the same
/// conversion used by the billing dashboard.
pub fn format_cost_display(credits: i32) -> String {
    let euros = credits as f64 / 100.0;
    format!("€{:.2}", euros)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_update_has_all_required_fields() {
        let json_str = build_session_update_json("sys prompt", "alloy");
        let v: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(v["type"], "session.update");
        let sess = &v["session"];
        assert_eq!(sess["type"], "conversation");
        let mods = sess["modalities"].as_array().unwrap();
        let mods: Vec<&str> = mods.iter().filter_map(|m| m.as_str()).collect();
        assert!(mods.contains(&"text"));
        assert!(mods.contains(&"audio"));
        assert_eq!(sess["instructions"], "sys prompt");
        assert_eq!(sess["voice"], "alloy");
        assert_eq!(sess["input_audio_format"], "pcm16");
        assert_eq!(sess["output_audio_format"], "pcm16");
        assert_eq!(sess["input_audio_transcription"]["model"], "whisper-1");
        assert_eq!(sess["turn_detection"]["type"], "server_vad");
    }

    #[test]
    fn audio_append_encodes_correctly() {
        let pcm: Vec<u8> = vec![1, 2, 3];
        let json_str = va_audio_append_json(&pcm);
        let v: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(v["type"], "input_audio_buffer.append");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(v["audio"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, pcm);
    }

    #[test]
    fn parse_audio_delta() {
        let b64 = base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3]);
        let frame = format!(r#"{{"type":"response.audio.delta","delta":"{b64}"}}"#);
        assert!(matches!(
            parse_realtime_event(&frame),
            VaEvent::AudioDelta(b) if b == vec![1u8, 2, 3]
        ));
    }

    #[test]
    fn parse_text_delta() {
        let frame = r#"{"type":"response.text.delta","delta":"hello"}"#;
        assert!(matches!(
            parse_realtime_event(frame),
            VaEvent::TextDelta(s) if s == "hello"
        ));
    }

    #[test]
    fn parse_transcript_delta() {
        let frame = r#"{"type":"response.audio_transcript.delta","delta":"world"}"#;
        assert!(matches!(
            parse_realtime_event(frame),
            VaEvent::TranscriptDelta(s) if s == "world"
        ));
    }

    #[test]
    fn parse_vad_events() {
        assert!(matches!(
            parse_realtime_event(r#"{"type":"input_audio_buffer.speech_started"}"#),
            VaEvent::SpeechStarted
        ));
        assert!(matches!(
            parse_realtime_event(r#"{"type":"input_audio_buffer.speech_stopped"}"#),
            VaEvent::SpeechStopped
        ));
    }

    #[test]
    fn parse_response_done() {
        assert!(matches!(
            parse_realtime_event(r#"{"type":"response.done"}"#),
            VaEvent::ResponseDone
        ));
    }

    #[test]
    fn parse_error() {
        let frame = r#"{"type":"error","error":{"message":"rate limited"}}"#;
        assert!(matches!(
            parse_realtime_event(frame),
            VaEvent::Error(e) if e == "rate limited"
        ));
    }

    #[test]
    fn parse_unknown_and_invalid() {
        assert!(matches!(
            parse_realtime_event(r#"{"type":"session.created"}"#),
            VaEvent::Other
        ));
        assert!(matches!(parse_realtime_event("not json"), VaEvent::Other));
        // Bad base64 in audio delta → Other (never panic)
        let frame = r#"{"type":"response.audio.delta","delta":"!!!!"}"#;
        assert!(matches!(parse_realtime_event(frame), VaEvent::Other));
    }

    #[test]
    fn cost_tick_structure() {
        let tick = build_cost_tick_json(30, 12, "€0.12");
        let v: Value = serde_json::from_str(&tick).unwrap();
        assert_eq!(v["type"], "cost_tick");
        assert_eq!(v["duration_s"], 30);
        assert_eq!(v["credits_so_far"], 12);
        assert_eq!(v["cost_display"], "€0.12");
    }

    #[test]
    fn session_end_structure() {
        let end = build_session_end_json(120, 50, "€0.50");
        let v: Value = serde_json::from_str(&end).unwrap();
        assert_eq!(v["type"], "session_end");
        assert_eq!(v["duration_s"], 120);
        assert_eq!(v["credits_used"], 50);
        assert_eq!(v["cost_display"], "€0.50");
    }

    #[test]
    fn rag_instructions_empty_kb() {
        let ctx = build_rag_instructions(&[], "AcmeCorp", None, None);
        assert!(ctx.contains("no prior transcriptions"));
        assert!(ctx.contains("AcmeCorp"));
    }

    #[test]
    fn rag_instructions_with_chunks() {
        let chunks = vec!["chunk A".to_string(), "chunk B".to_string()];
        let ctx = build_rag_instructions(&chunks, "AcmeCorp", Some("Alpha"), Some("Alice"));
        assert!(ctx.contains("chunk A"));
        assert!(ctx.contains("chunk B"));
        assert!(ctx.contains("Alpha"));
        assert!(ctx.contains("Alice"));
    }

    #[test]
    fn format_cost_display_rounding() {
        assert_eq!(format_cost_display(0), "€0.00");
        assert_eq!(format_cost_display(100), "€1.00");
        assert_eq!(format_cost_display(38), "€0.38");
    }
}
