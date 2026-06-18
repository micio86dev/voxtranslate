//! Low-level Gemini Live Translate client (spec 0100).
//!
//! One WebSocket session per **target language** against the Gemini Live API
//! (`BidiGenerateContent`). Unlike OpenAI, the API key goes in the **query string**
//! (not a header), and a single `serverContent` frame can bundle several things at
//! once (audio + input/output transcripts + a turn boundary), so [`parse_server_message`]
//! returns a **list** of [`GeminiEvent`]s.
//!
//! Two more deltas from the OpenAI client: Gemini wants **16 kHz** PCM16 input
//! (our capture is 24 kHz, so we [`resample_pcm16_mono`] the feed) and emits an
//! explicit `turnComplete`/`generationComplete` boundary (no idle-only guessing).
//! Output audio is 24 kHz — already our playback rate.
//!
//! The pure build/parse/resample helpers are unit-tested; the socket I/O needs a
//! live `GOOGLE_AI_API_KEY`.

use base64::Engine as _;
use futures::stream::{SplitSink, SplitStream};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::config::GeminiConfig;

type GemStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
pub type GemSink = SplitSink<GemStream, Message>;
pub type GemSource = SplitStream<GemStream>;

/// Sample rate the client captures PCM16 at (spec 0043 / 0093 premium path).
pub const CAPTURE_HZ: u32 = 24_000;
/// Sample rate the Gemini Live API expects for input audio.
pub const GEMINI_INPUT_HZ: u32 = 16_000;

/// A parsed Gemini Live server event. Only the frames we act on are modelled;
/// everything else is dropped (an empty list), so a stray frame never kills the
/// session. A single wire message may yield several of these.
#[derive(Debug, Clone, PartialEq)]
pub enum GeminiEvent {
    /// The session is configured and ready (`setupComplete`).
    SetupComplete,
    /// Append-only fragment of the speaker's **original** transcript.
    InputTranscript(String),
    /// Append-only fragment of the **translated** transcript (this session's lang).
    OutputTranscript(String),
    /// A chunk of translated **audio** (PCM16 @ 24 kHz, base64-decoded).
    OutputAudio(Vec<u8>),
    /// The model finished a turn/generation — a natural caption boundary.
    TurnComplete,
    /// The server will disconnect soon (`goAway`); reconnect proactively.
    GoAway,
    /// A server-side error. The caller decides whether it's fatal.
    Error(String),
}

/// Build the Gemini Live WebSocket URL for `config`. The API key is a **query
/// parameter** — callers MUST NOT log this string (it carries the secret).
pub fn session_url(config: &GeminiConfig) -> String {
    format!(
        "wss://generativelanguage.googleapis.com/ws/\
         google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent?key={}",
        config.api_key
    )
}

/// The `setup` frame opening a translation session for `target_lang`. `generationConfig`
/// carries `responseModalities` + `translationConfig`, but the transcription enablers
/// (`inputAudioTranscription` / `outputAudioTranscription`) are **setup-level** fields —
/// the Live API rejects them inside `generationConfig` (1007 close: "Unknown name
/// 'inputAudioTranscription' at 'setup.generation_config'"). `echoTargetLanguage` makes
/// the model parrot input that's already in the target language (so a speaker briefly
/// using the listener's language isn't mistranslated).
pub fn setup_json(model: &str, target_lang: &str) -> String {
    // The wire wants a fully-qualified `models/<id>`; tolerate either form in config.
    let model_path = if model.starts_with("models/") {
        model.to_string()
    } else {
        format!("models/{model}")
    };
    serde_json::json!({
        "setup": {
            "model": model_path,
            "generationConfig": {
                "responseModalities": ["AUDIO"],
                "translationConfig": {
                    "targetLanguageCode": target_lang,
                    "echoTargetLanguage": true,
                }
            },
            "inputAudioTranscription": {},
            "outputAudioTranscription": {}
        }
    })
    .to_string()
}

/// The `realtimeInput` frame carrying one PCM16 chunk. `pcm16` MUST already be at
/// [`GEMINI_INPUT_HZ`] (16 kHz) — see [`resample_pcm16_mono`].
pub fn audio_input_json(pcm16_16k: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(pcm16_16k);
    serde_json::json!({
        "realtimeInput": {
            "audio": { "data": b64, "mimeType": "audio/pcm;rate=16000" }
        }
    })
    .to_string()
}

/// Tell the server the audio stream has ended (flush + finish the current turn).
pub fn audio_stream_end_json() -> &'static str {
    r#"{"realtimeInput":{"audioStreamEnd":true}}"#
}

/// Parse one Gemini server frame (JSON text) into zero or more [`GeminiEvent`]s.
/// Pure — for tests. A `serverContent` message can bundle audio, both transcripts,
/// and a turn boundary, so each present piece becomes its own event. Unknown shapes
/// and undecodable audio are skipped rather than erroring.
pub fn parse_server_message(text: &str) -> Vec<GeminiEvent> {
    let v: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();

    if v.get("setupComplete").is_some() {
        out.push(GeminiEvent::SetupComplete);
    }
    if v.get("goAway").is_some() {
        out.push(GeminiEvent::GoAway);
    }
    // Errors can arrive top-level (`error.message`) or as a bare `{"error": "..."}`.
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .or_else(|| err.as_str())
            .unwrap_or("gemini live error");
        out.push(GeminiEvent::Error(msg.to_string()));
    }

    if let Some(sc) = v.get("serverContent") {
        if let Some(t) = sc
            .pointer("/inputTranscription/text")
            .and_then(|t| t.as_str())
        {
            if !t.is_empty() {
                out.push(GeminiEvent::InputTranscript(t.to_string()));
            }
        }
        if let Some(t) = sc
            .pointer("/outputTranscription/text")
            .and_then(|t| t.as_str())
        {
            if !t.is_empty() {
                out.push(GeminiEvent::OutputTranscript(t.to_string()));
            }
        }
        if let Some(parts) = sc.pointer("/modelTurn/parts").and_then(|p| p.as_array()) {
            for part in parts {
                let Some(inline) = part.get("inlineData") else {
                    continue;
                };
                // Only decode audio inlineData; ignore any other part types.
                let is_audio = inline
                    .get("mimeType")
                    .and_then(|m| m.as_str())
                    .map(|m| m.starts_with("audio/"))
                    .unwrap_or(false);
                if !is_audio {
                    continue;
                }
                if let Some(data) = inline.get("data").and_then(|d| d.as_str()) {
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
                        out.push(GeminiEvent::OutputAudio(bytes));
                    }
                }
            }
        }
        let turn_done = sc
            .get("turnComplete")
            .and_then(|b| b.as_bool())
            .unwrap_or(false)
            || sc
                .get("generationComplete")
                .and_then(|b| b.as_bool())
                .unwrap_or(false);
        if turn_done {
            out.push(GeminiEvent::TurnComplete);
        }
    }

    out
}

/// Resample mono PCM16 (little-endian) from `from_hz` to `to_hz` by linear
/// interpolation. Used to turn our 24 kHz capture into the 16 kHz Gemini wants.
/// A trailing odd byte is ignored; a no-op rate change returns the input as-is.
pub fn resample_pcm16_mono(input: &[u8], from_hz: u32, to_hz: u32) -> Vec<u8> {
    if from_hz == to_hz || from_hz == 0 || to_hz == 0 {
        return input.to_vec();
    }
    let n_in = input.len() / 2;
    if n_in == 0 {
        return Vec::new();
    }
    let sample = |i: usize| -> f64 {
        let i = i.min(n_in - 1);
        i16::from_le_bytes([input[i * 2], input[i * 2 + 1]]) as f64
    };
    let n_out = ((n_in as u64) * (to_hz as u64) / (from_hz as u64)) as usize;
    let mut out = Vec::with_capacity(n_out * 2);
    if n_out == 0 {
        return out;
    }
    let step = from_hz as f64 / to_hz as f64; // source samples advanced per output sample
    for j in 0..n_out {
        let pos = j as f64 * step;
        let idx = pos.floor() as usize;
        let frac = pos - idx as f64;
        let interpolated = sample(idx) + (sample(idx + 1) - sample(idx)) * frac;
        let clamped = interpolated.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16;
        out.extend_from_slice(&clamped.to_le_bytes());
    }
    out
}

/// Open a Gemini Live translation WebSocket for `target_lang` and send the initial
/// `setup` frame. Returns the split sink (send audio) and source (read events).
pub async fn open_session(
    config: &GeminiConfig,
    target_lang: &str,
) -> Result<(GemSink, GemSource), String> {
    let request = session_url(config)
        .into_client_request()
        .map_err(|e| format!("invalid gemini url: {e}"))?;

    let (ws, _resp) = connect_async(request)
        .await
        // Don't surface the URL (it carries the key) in the error.
        .map_err(|e| format!("gemini connect failed: {e}"))?;

    use futures::SinkExt as _;
    use futures::StreamExt as _;
    let (mut sink, source) = ws.split();
    sink.send(Message::text(setup_json(&config.model, target_lang)))
        .await
        .map_err(|e| format!("gemini setup failed: {e}"))?;
    // NB: never log the URL/key — only the model + target language.
    tracing::info!(%target_lang, model = %config.model, "gemini: session connecting");
    Ok((sink, source))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> GeminiConfig {
        GeminiConfig {
            api_key: "SECRET_KEY".into(),
            model: "gemini-3.5-live-translate-preview".into(),
            cost_per_minute: 0.023,
            markup: 0.5,
            max_sessions: 16,
        }
    }

    #[test]
    fn setup_frame_places_fields_where_the_api_wants_them() {
        let v: Value =
            serde_json::from_str(&setup_json("gemini-3.5-live-translate-preview", "pl")).unwrap();
        let setup = &v["setup"];
        assert_eq!(setup["model"], "models/gemini-3.5-live-translate-preview");
        let gc = &setup["generationConfig"];
        assert_eq!(gc["responseModalities"][0], "AUDIO");
        // translationConfig lives in generationConfig …
        assert_eq!(gc["translationConfig"]["targetLanguageCode"], "pl");
        assert_eq!(gc["translationConfig"]["echoTargetLanguage"], true);
        // … but the transcription enablers are SETUP-level (the API 1007-rejects them
        // inside generationConfig).
        assert!(setup.get("inputAudioTranscription").is_some());
        assert!(setup.get("outputAudioTranscription").is_some());
        assert!(gc.get("inputAudioTranscription").is_none());
    }

    #[test]
    fn setup_frame_tolerates_prequalified_model() {
        let v: Value = serde_json::from_str(&setup_json("models/foo", "es")).unwrap();
        // Already `models/…` → not double-prefixed.
        assert_eq!(v["setup"]["model"], "models/foo");
    }

    #[test]
    fn audio_input_frame_carries_b64_pcm_at_16k() {
        let v: Value = serde_json::from_str(&audio_input_json(&[0u8, 255, 128, 7])).unwrap();
        assert_eq!(
            v["realtimeInput"]["audio"]["mimeType"],
            "audio/pcm;rate=16000"
        );
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(v["realtimeInput"]["audio"]["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, vec![0u8, 255, 128, 7]);
    }

    #[test]
    fn url_carries_key_in_query_not_a_header() {
        // Gemini auths via a `?key=` query param (no Authorization header). The key
        // lands IN the URL, so call sites must never log this string — `open_session`
        // traces only model + lang (see its body).
        let url = session_url(&cfg());
        assert!(url.starts_with("wss://generativelanguage.googleapis.com/ws/"));
        // The string-continuation `\` in session_url must not leak whitespace.
        assert!(!url.contains(' '), "url must not contain whitespace");
        assert!(url.ends_with("BidiGenerateContent?key=SECRET_KEY"));
    }

    #[test]
    fn parses_setup_complete_and_goaway() {
        assert_eq!(
            parse_server_message(r#"{"setupComplete":{}}"#),
            vec![GeminiEvent::SetupComplete]
        );
        assert_eq!(
            parse_server_message(r#"{"goAway":{"timeLeft":{"seconds":10}}}"#),
            vec![GeminiEvent::GoAway]
        );
    }

    #[test]
    fn parses_bundled_server_content() {
        // A single frame with both transcripts, an audio part, and a turn boundary.
        let audio_b64 = base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3, 4]);
        let frame = format!(
            r#"{{"serverContent":{{
                "inputTranscription":{{"text":"ciao "}},
                "outputTranscription":{{"text":"hello "}},
                "modelTurn":{{"parts":[{{"inlineData":{{"mimeType":"audio/pcm;rate=24000","data":"{audio_b64}"}}}}]}},
                "turnComplete":true
            }}}}"#
        );
        let events = parse_server_message(&frame);
        assert!(events.contains(&GeminiEvent::InputTranscript("ciao ".into())));
        assert!(events.contains(&GeminiEvent::OutputTranscript("hello ".into())));
        assert!(events.contains(&GeminiEvent::OutputAudio(vec![1, 2, 3, 4])));
        assert!(events.contains(&GeminiEvent::TurnComplete));
    }

    #[test]
    fn generation_complete_also_ends_a_turn() {
        let events = parse_server_message(r#"{"serverContent":{"generationComplete":true}}"#);
        assert_eq!(events, vec![GeminiEvent::TurnComplete]);
    }

    #[test]
    fn skips_non_audio_parts_and_bad_audio_without_panicking() {
        // A text part is ignored; only audio inlineData is decoded.
        let events = parse_server_message(
            r#"{"serverContent":{"modelTurn":{"parts":[{"text":"x"},{"inlineData":{"mimeType":"image/png","data":"AAAA"}}]}}}"#,
        );
        assert!(events.is_empty());
        // Garbage base64 on an audio part degrades to nothing, never panics.
        let events = parse_server_message(
            r#"{"serverContent":{"modelTurn":{"parts":[{"inlineData":{"mimeType":"audio/pcm","data":"!!!!"}}]}}}"#,
        );
        assert!(events.is_empty());
    }

    #[test]
    fn parses_error_shapes_and_ignores_junk() {
        assert_eq!(
            parse_server_message(r#"{"error":{"message":"quota exceeded"}}"#),
            vec![GeminiEvent::Error("quota exceeded".into())]
        );
        // Non-JSON and empty transcripts produce no events.
        assert!(parse_server_message("not json").is_empty());
        assert!(
            parse_server_message(r#"{"serverContent":{"inputTranscription":{"text":""}}}"#)
                .is_empty()
        );
    }

    #[test]
    fn resample_24k_to_16k_yields_two_thirds_length() {
        // 3 input samples (6 bytes) → 2 output samples (4 bytes).
        let input = vec![0u8; 6];
        let out = resample_pcm16_mono(&input, CAPTURE_HZ, GEMINI_INPUT_HZ);
        assert_eq!(out.len(), 4);
        // 30 samples → 20 samples.
        let out = resample_pcm16_mono(&[0u8; 60], CAPTURE_HZ, GEMINI_INPUT_HZ);
        assert_eq!(out.len(), 40);
    }

    #[test]
    fn resample_preserves_a_constant_signal() {
        // A DC signal (every sample = 1000) must stay ~1000 after resampling.
        let mut input = Vec::new();
        for _ in 0..30 {
            input.extend_from_slice(&1000i16.to_le_bytes());
        }
        let out = resample_pcm16_mono(&input, CAPTURE_HZ, GEMINI_INPUT_HZ);
        assert_eq!(out.len(), 40);
        for s in out.chunks_exact(2) {
            let v = i16::from_le_bytes([s[0], s[1]]);
            assert_eq!(v, 1000, "constant signal must be preserved exactly");
        }
    }

    #[test]
    fn resample_is_noop_for_equal_rates_and_handles_empty() {
        let input = vec![1u8, 2, 3, 4];
        assert_eq!(resample_pcm16_mono(&input, 16_000, 16_000), input);
        assert!(resample_pcm16_mono(&[], CAPTURE_HZ, GEMINI_INPUT_HZ).is_empty());
    }
}
