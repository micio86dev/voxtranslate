//! Persistent per-speaker Deepgram streaming WebSocket client.
//!
//! Each speaker gets one Deepgram connection for the whole session. Audio chunks
//! are forwarded as binary frames; transcripts come back as JSON text frames,
//! are broadcast to the room, and finals additionally trigger an async Groq
//! translation.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::MissedTickBehavior;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{HeaderValue, AUTHORIZATION};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

use crate::config::Config;
use crate::glossary::GlossaryService;
use crate::moderation::{Moderator, Severity};
use crate::protocol::{DeepgramResponse, ServerMessage};
use crate::rooms::RoomManager;
use crate::transcripts::{EventKind, TranscriptEvent, TranscriptService};
use crate::translator::Translator;

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
}

type DgStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type DgSink = SplitSink<DgStream, Message>;
type DgSource = SplitStream<DgStream>;

/// Open a persistent Deepgram streaming connection for `source_lang` and split
/// it into a sink (send audio) and a stream (receive transcripts).
pub async fn open_deepgram_ws(
    source_lang: &str,
    config: &Config,
) -> Result<(DgSink, DgSource), String> {
    // `container=webm` lets Deepgram auto-detect the Opus encoding, sample rate,
    // and channels from the WebM header — which is what the browser's
    // MediaRecorder actually produces (Opus is internally 48kHz, not 16k). Passing
    // explicit `encoding`/`sample_rate`/`channels` here breaks container demuxing
    // (Deepgram then decodes only ~0.1s of audio), so we deliberately omit them.
    let url = format!(
        "wss://api.deepgram.com/v1/listen\
         ?container=webm&model=nova-2&language={source_lang}\
         &punctuate=true&interim_results=true&utterance_end_ms=1000\
         &vad_events=true&smart_format=true"
    );

    let mut request = url
        .into_client_request()
        .map_err(|e| format!("invalid deepgram url: {e}"))?;

    let auth = HeaderValue::from_str(&format!("Token {}", config.deepgram_key))
        .map_err(|e| format!("invalid deepgram key header: {e}"))?;
    request.headers_mut().insert(AUTHORIZATION, auth);

    let (ws, _resp) = connect_async(request)
        .await
        .map_err(|e| format!("deepgram connect failed: {e}"))?;

    Ok(ws.split())
}

/// Detect the spoken language from a short WebM clip (spec 0012).
///
/// `detect_language` is REST-only — the streaming WS has no equivalent — so the
/// auto-detect flow buffers the first ~3s of MediaRecorder output and probes it
/// here before opening the real streaming connection.
pub async fn detect_language(
    http: &reqwest::Client,
    config: &Config,
    webm: Vec<u8>,
) -> Result<(String, Option<f64>), String> {
    let resp = http
        .post("https://api.deepgram.com/v1/listen?detect_language=true&model=nova-2")
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Token {}", config.deepgram_key),
        )
        .header(reqwest::header::CONTENT_TYPE, "audio/webm")
        .timeout(Duration::from_secs(10))
        .body(webm)
        .send()
        .await
        .map_err(|e| format!("deepgram detect request failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let detail = resp.text().await.unwrap_or_default();
        return Err(format!("deepgram detect returned {status}: {detail}"));
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("deepgram detect parse failed: {e}"))?;
    parse_detect_response(&json)
}

/// Transcribe a prerecorded audio file (spec 0018 chat upload). Uses the same
/// REST `/v1/listen` endpoint as [`detect_language`] but asks for the full
/// transcript with smart formatting, and returns both the transcript and the
/// detected language so the chat fan-out knows the source language.
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

/// Extract `(detected_language, confidence)` from a `/v1/listen` REST response.
/// Pure, for tests; the language lives at `results.channels[0].detected_language`.
pub fn parse_detect_response(json: &serde_json::Value) -> Result<(String, Option<f64>), String> {
    let channel = json
        .pointer("/results/channels/0")
        .ok_or("deepgram detect response has no channels")?;
    let lang = channel
        .get("detected_language")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("deepgram detect response has no detected_language")?;
    let confidence = channel.get("language_confidence").and_then(|v| v.as_f64());
    Ok((lang.to_string(), confidence))
}

/// Forward audio chunks from the channel to Deepgram, keeping the connection
/// alive during silence and flushing on close.
///
/// Runs until the audio channel is dropped (speaker disconnected) or the sink
/// errors. Sends `{"type":"KeepAlive"}` every 8s of inactivity and a final
/// `{"type":"CloseStream"}` to flush pending transcripts before closing.
pub async fn forward_audio(mut audio_rx: UnboundedReceiver<Vec<u8>>, mut sink: DgSink) {
    let mut keepalive = tokio::time::interval(Duration::from_secs(8));
    keepalive.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            maybe_chunk = audio_rx.recv() => {
                match maybe_chunk {
                    Some(chunk) => {
                        if sink.send(Message::binary(chunk)).await.is_err() {
                            break;
                        }
                        // Reset the keepalive window now that audio is flowing.
                        keepalive.reset();
                    }
                    None => {
                        // Speaker disconnected: flush finals, then close cleanly.
                        let _ = sink.send(Message::text(r#"{"type":"CloseStream"}"#)).await;
                        let _ = sink.close().await;
                        break;
                    }
                }
            }
            _ = keepalive.tick() => {
                if sink.send(Message::text(r#"{"type":"KeepAlive"}"#)).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// Read Deepgram transcripts for one speaking session and broadcast subtitles to
/// the whole room so each peer can render them on the speaker's video cell:
/// - interim → `subtitle_interim` (original language, live),
/// - final → translated into every language present (fan-out), broadcast as
///   `subtitle_final` with a `{ lang: text }` map; each client picks its own.
pub async fn process_transcripts(
    mut source: DgSource,
    rooms: Arc<RoomManager>,
    translator: Translator,
    moderator: Arc<Moderator>,
    ctx: SpeakerCtx,
    transcripts: Option<TranscriptService>,
) {
    let SpeakerCtx {
        room,
        speaker_id,
        speaker_name,
        speaker_lang,
        session_id,
        speaker_user_id,
        glossary,
    } = ctx;
    while let Some(msg) = source.next().await {
        let text = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Close(_)) => break,
            Ok(_) => continue, // ping/pong/binary — ignore
            Err(e) => {
                tracing::warn!("deepgram stream error: {e}");
                break;
            }
        };

        let parsed: DeepgramResponse = match serde_json::from_str(text.as_str()) {
            Ok(p) => p,
            Err(_) => continue, // non-Results frame we don't model
        };

        let Some((transcript, confidence)) = parsed.best_alternative() else {
            continue;
        };
        if confidence < 0.4 {
            continue;
        }

        if !parsed.is_final {
            // Live partial subtitle, shown (untranslated) on the speaker's cell.
            rooms.broadcast(
                &room,
                &ServerMessage::SubtitleInterim {
                    speaker_id: speaker_id.clone(),
                    speaker_name: speaker_name.clone(),
                    text: transcript.to_string(),
                    lang: speaker_lang.clone(),
                }
                .to_json(),
            );
            continue;
        }

        let transcript = transcript.to_string();

        // Moderation: drop a flagged final (don't translate/broadcast it) and warn
        // only the speaker, so abusive speech isn't shown/translated to the room.
        if moderator.severity(&transcript) == Severity::Severe {
            tracing::info!(%room, speaker = %speaker_id, "moderation: dropped flagged transcript");
            rooms.relay_to_peer(
                &room,
                &speaker_id,
                &ServerMessage::ModerationWarning {
                    message: "Your message was blocked by moderation.".to_string(),
                }
                .to_json(),
            );
            continue;
        }

        // Fan out a translation per distinct language in the room, then broadcast
        // the final subtitle with the full map so every peer picks its language.
        // `ts` is captured *now* (when the words were spoken), not after the
        // translation round-trip, so transcript ordering matches reality.
        let rooms = rooms.clone();
        let translator = translator.clone();
        let transcripts = transcripts.clone();
        let glossary = glossary.clone();
        let room = room.clone();
        let speaker_id = speaker_id.clone();
        let speaker_name = speaker_name.clone();
        let speaker_lang = speaker_lang.clone();
        let ts = Utc::now();
        tokio::spawn(async move {
            let target_langs = rooms.get_room_languages(&room, &speaker_id);
            // Snapshot of the room glossary (cache populated at room join).
            let glo = glossary.as_ref().and_then(|g| g.cached(&room));
            let translations = translator
                .translate_fanout(&transcript, &speaker_lang, &target_langs, glo.as_deref())
                .await;
            if let Some(svc) = transcripts.as_ref() {
                svc.record(TranscriptEvent {
                    session_id,
                    kind: EventKind::Speech,
                    speaker_peer_id: speaker_id.clone(),
                    speaker_user_id,
                    speaker_name: speaker_name.clone(),
                    original_text: transcript.clone(),
                    original_lang: speaker_lang.clone(),
                    translations: translations.clone(),
                    ts,
                });
            }
            rooms.broadcast(
                &room,
                &ServerMessage::SubtitleFinal {
                    speaker_id,
                    speaker_name,
                    original: transcript,
                    lang: speaker_lang,
                    translations,
                }
                .to_json(),
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_detect_response_extracts_lang_and_confidence() {
        let json = serde_json::json!({
            "results": { "channels": [{
                "detected_language": "it",
                "language_confidence": 0.93,
                "alternatives": [{ "transcript": "ciao a tutti", "confidence": 0.98 }]
            }]}
        });
        let (lang, conf) = parse_detect_response(&json).unwrap();
        assert_eq!(lang, "it");
        assert!((conf.unwrap() - 0.93).abs() < 1e-9);
    }

    #[test]
    fn parse_detect_response_confidence_optional() {
        let json = serde_json::json!({
            "results": { "channels": [{ "detected_language": "en" }]}
        });
        let (lang, conf) = parse_detect_response(&json).unwrap();
        assert_eq!(lang, "en");
        assert!(conf.is_none());
    }

    #[test]
    fn parse_detect_response_rejects_missing_or_empty() {
        // No channels at all.
        assert!(parse_detect_response(&serde_json::json!({})).is_err());
        // Channel present but no detected_language field.
        let no_lang = serde_json::json!({ "results": { "channels": [{}] } });
        assert!(parse_detect_response(&no_lang).is_err());
        // Empty string is as useless as missing.
        let empty = serde_json::json!({
            "results": { "channels": [{ "detected_language": "" }] }
        });
        assert!(parse_detect_response(&empty).is_err());
    }

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
