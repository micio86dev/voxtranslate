//! Low-level OpenAI Realtime Translation client (spec 0093).
//!
//! One WebSocket session per **output language**:
//! `wss://api.openai.com/v1/realtime/translations?model=…`, Bearer auth. After
//! the socket opens we send `session.update` to set the output language, then
//! stream the speaker's PCM16 audio as base64 `session.input_audio_buffer.append`
//! frames and read back translated audio + transcript deltas.
//!
//! The server emits only append-only deltas — there are **no** segment/`done`
//! boundary events (verified against the API reference), so the caller segments
//! captions by idle debounce. The pure [`parse_openai_event`] is unit-tested; the
//! socket I/O needs a live key.

use base64::Engine as _;
use futures::stream::{SplitSink, SplitStream};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{HeaderValue, AUTHORIZATION};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::config::OpenAiConfig;

type OaStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
pub type OaSink = SplitSink<OaStream, Message>;
pub type OaSource = SplitStream<OaStream>;

/// A parsed OpenAI realtime-translation server event. Only the frames we act on
/// are modelled; everything else (`session.created`/`updated`, …) is [`Other`].
///
/// [`Other`]: OpenAiEvent::Other
#[derive(Debug, Clone, PartialEq)]
pub enum OpenAiEvent {
    /// Append-only fragment of the speaker's **original** transcript.
    InputTranscriptDelta(String),
    /// Append-only fragment of the **translated** transcript (this session's lang).
    OutputTranscriptDelta(String),
    /// A chunk of translated **audio** (PCM16, base64-decoded).
    OutputAudioDelta(Vec<u8>),
    /// The session was closed by the server (after our `session.close`).
    Closed,
    /// A server-side error (`message`). Most are recoverable; the caller decides.
    Error(String),
    /// A frame we don't act on (lifecycle, unknown, or undecodable audio).
    Other,
}

/// Parse one OpenAI server frame (JSON text) into an [`OpenAiEvent`]. Pure — for
/// tests. Unknown shapes and undecodable audio fall back to [`OpenAiEvent::Other`]
/// rather than erroring, so a stray frame never kills the session.
pub fn parse_openai_event(text: &str) -> OpenAiEvent {
    let v: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return OpenAiEvent::Other,
    };
    let delta = || v.get("delta").and_then(|d| d.as_str()).unwrap_or_default();
    match v.get("type").and_then(|t| t.as_str()).unwrap_or_default() {
        "session.input_transcript.delta" => OpenAiEvent::InputTranscriptDelta(delta().to_string()),
        "session.output_transcript.delta" => {
            OpenAiEvent::OutputTranscriptDelta(delta().to_string())
        }
        "session.output_audio.delta" => {
            match base64::engine::general_purpose::STANDARD.decode(delta()) {
                Ok(bytes) => OpenAiEvent::OutputAudioDelta(bytes),
                Err(_) => OpenAiEvent::Other,
            }
        }
        "session.closed" => OpenAiEvent::Closed,
        "error" => {
            // The message can sit at the top level or nested under `error`.
            let msg = v
                .get("message")
                .and_then(|m| m.as_str())
                .or_else(|| v.pointer("/error/message").and_then(|m| m.as_str()))
                .unwrap_or("openai realtime error");
            OpenAiEvent::Error(msg.to_string())
        }
        _ => OpenAiEvent::Other,
    }
}

/// The `session.update` frame that sets the output language for a session.
pub fn session_update_json(output_lang: &str) -> String {
    serde_json::json!({
        "type": "session.update",
        "session": { "audio": { "output": { "language": output_lang } } }
    })
    .to_string()
}

/// The `session.input_audio_buffer.append` frame carrying one PCM16 chunk.
pub fn audio_append_json(pcm16: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(pcm16);
    serde_json::json!({ "type": "session.input_audio_buffer.append", "audio": b64 }).to_string()
}

/// The `session.close` frame — flushes pending input and asks the server to close.
pub fn session_close_json() -> &'static str {
    r#"{"type":"session.close"}"#
}

/// Open a realtime-translation WebSocket for `output_lang` and send the initial
/// `session.update`. Returns the split sink (send audio) and source (read events).
pub async fn open_session(
    config: &OpenAiConfig,
    output_lang: &str,
) -> Result<(OaSink, OaSource), String> {
    let url = format!(
        "wss://api.openai.com/v1/realtime/translations?model={}",
        config.model
    );
    let mut request = url
        .into_client_request()
        .map_err(|e| format!("invalid openai url: {e}"))?;
    let auth = HeaderValue::from_str(&format!("Bearer {}", config.api_key))
        .map_err(|e| format!("invalid openai key header: {e}"))?;
    request.headers_mut().insert(AUTHORIZATION, auth);

    let (ws, _resp) = connect_async(request)
        .await
        .map_err(|e| format!("openai connect failed: {e}"))?;

    use futures::SinkExt as _;
    use futures::StreamExt as _;
    let (mut sink, source) = ws.split();
    sink.send(Message::text(session_update_json(output_lang)))
        .await
        .map_err(|e| format!("openai session.update failed: {e}"))?;
    tracing::info!(%output_lang, model = %config.model, "openai: session connected");
    Ok((sink, source))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_transcript_deltas() {
        assert_eq!(
            parse_openai_event(r#"{"type":"session.input_transcript.delta","delta":"ciao "}"#),
            OpenAiEvent::InputTranscriptDelta("ciao ".to_string())
        );
        assert_eq!(
            parse_openai_event(r#"{"type":"session.output_transcript.delta","delta":"hello "}"#),
            OpenAiEvent::OutputTranscriptDelta("hello ".to_string())
        );
    }

    #[test]
    fn decodes_audio_delta_from_base64() {
        let b64 = base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3, 4]);
        let frame = format!(r#"{{"type":"session.output_audio.delta","delta":"{b64}"}}"#);
        assert_eq!(
            parse_openai_event(&frame),
            OpenAiEvent::OutputAudioDelta(vec![1, 2, 3, 4])
        );
        // Garbage base64 degrades to Other, never panics.
        assert_eq!(
            parse_openai_event(r#"{"type":"session.output_audio.delta","delta":"!!!!"}"#),
            OpenAiEvent::Other
        );
    }

    #[test]
    fn parses_close_error_and_unknown() {
        assert_eq!(
            parse_openai_event(r#"{"type":"session.closed"}"#),
            OpenAiEvent::Closed
        );
        assert_eq!(
            parse_openai_event(r#"{"type":"error","message":"rate limited"}"#),
            OpenAiEvent::Error("rate limited".to_string())
        );
        // Nested error message shape.
        assert_eq!(
            parse_openai_event(r#"{"type":"error","error":{"message":"bad audio"}}"#),
            OpenAiEvent::Error("bad audio".to_string())
        );
        // Lifecycle + unknown + non-JSON all collapse to Other.
        assert_eq!(
            parse_openai_event(r#"{"type":"session.created"}"#),
            OpenAiEvent::Other
        );
        assert_eq!(parse_openai_event("not json"), OpenAiEvent::Other);
    }

    #[test]
    fn builds_control_frames() {
        let upd: Value = serde_json::from_str(&session_update_json("es")).unwrap();
        assert_eq!(upd["type"], "session.update");
        assert_eq!(upd["session"]["audio"]["output"]["language"], "es");

        let app: Value = serde_json::from_str(&audio_append_json(&[0u8, 255, 128])).unwrap();
        assert_eq!(app["type"], "session.input_audio_buffer.append");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(app["audio"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, vec![0u8, 255, 128]);

        assert!(session_close_json().contains("session.close"));
    }
}
