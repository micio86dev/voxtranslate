//! A stand-in for the realtime translation providers, for tests.
//!
//! Shipped rather than `#[cfg(test)]`-gated for the same reason
//! [`telephony::mock`](crate::telephony::mock) is: the integration tests live in
//! `tests/`, outside the crate, and cannot reach a test-only module.
//!
//! # Why this exists
//!
//! Every realtime engine — Pro (OpenAI), Premium (Gemini), the voice and help
//! assistants — opens a WebSocket to its provider before doing anything a test can
//! observe. Without somewhere else to point that socket, the reconnect loop, the
//! event fan-out, the credit metering and the teardown were all unreachable, and
//! the engines sat between 4% and 43% covered while the rest of the server was
//! above 80.
//!
//! So the seam is an endpoint, not a trait: `OPENAI_REALTIME_BASE_URL` and
//! `GEMINI_LIVE_BASE_URL` (with `QWEN_ENDPOINT` and `CARTESIA_*_ENDPOINT` already
//! configurable) point the real client code at this server. The client is exactly
//! the one production runs — only the host changes.
//!
//! # What it is not
//!
//! It speaks enough of each wire protocol to drive the engines, and nothing more.
//! It does not translate, does not model latency, and does not attempt to be a
//! reference implementation. A test that would only pass against a faithful model
//! of OpenAI's behaviour should be `#[ignore]`d and run against the real thing —
//! `real_openai_pro_listener_receives_translated_subtitle` still is.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use futures::{SinkExt as _, StreamExt as _};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;

/// What the stand-in answers with when the engine sends it audio.
#[derive(Debug, Clone)]
pub enum Reply {
    /// The speaker's own words and their translation, as the provider would
    /// dribble them out. Sent once, on the first audio frame of a session.
    Transcript {
        /// Emitted as `session.input_transcript.delta` (OpenAI) /
        /// `inputTranscription` (Gemini).
        original: String,
        /// Emitted as `session.output_transcript.delta` / `outputTranscription`.
        translated: String,
    },
    /// One chunk of translated PCM16, base64-encoded on the wire.
    Audio(Vec<u8>),
    /// A server-side error frame. The engine decides whether it is fatal.
    Error(String),
    /// Close the socket without a word — an upstream drop, which is what the
    /// reconnect loop exists for.
    Drop,
}

/// Which wire protocol to speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// `session.*` frames — the Pro engine and both assistants.
    OpenAi,
    /// `serverContent` frames — the Premium engine.
    Gemini,
    /// `response.*` frames — the voice and help assistants. Same host as
    /// [`Dialect::OpenAi`], a different vocabulary: the translations endpoint says
    /// `session.output_transcript.delta`, the assistant one
    /// `response.output_audio_transcript.delta`. Mixing them up produces a socket
    /// that connects and then says nothing anybody listens to.
    OpenAiAssistant,
    /// Qwen realtime — the Standard engine. Close to the assistant vocabulary but
    /// not the same: the speaker's own words arrive under
    /// `conversation.item.input_audio_transcription.*`, and the field decides the
    /// semantics (`delta` appends, `text` replaces), not the event name.
    Qwen,
}

/// A running stand-in. Drop it and the listener goes with the task.
pub struct RealtimeMock {
    addr: SocketAddr,
    dialect: Dialect,
    inner: Arc<Shared>,
}

#[derive(Default)]
struct Shared {
    /// Every text frame the engine sent, in order.
    received: Mutex<Vec<String>>,
    /// How many sockets have been accepted. A reconnect shows up here.
    connections: Mutex<usize>,
    /// Replies to send on the first audio frame of each connection.
    replies: Mutex<Vec<Reply>>,
}

impl RealtimeMock {
    /// Bind on an ephemeral port and start accepting.
    pub async fn start(dialect: Dialect) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind realtime mock");
        let addr = listener.local_addr().expect("mock addr");
        let inner = Arc::new(Shared::default());
        let shared = inner.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let shared = shared.clone();
                tokio::spawn(async move {
                    serve(stream, dialect, shared).await;
                });
            }
        });
        Self {
            addr,
            dialect,
            inner,
        }
    }

    /// The base URL to put in the provider's config. Callers append their own
    /// path and query, exactly as they do against the real host.
    pub fn base_url(&self) -> String {
        match self.dialect {
            Dialect::OpenAi | Dialect::OpenAiAssistant => {
                format!("ws://{}/v1/realtime", self.addr)
            }
            Dialect::Gemini => format!("ws://{}/ws/live", self.addr),
            Dialect::Qwen => format!("ws://{}/api-ws/v1/realtime", self.addr),
        }
    }

    /// Queue what the stand-in answers with on the next connection's first audio
    /// frame. Replaces anything queued before.
    pub fn reply_with(&self, replies: Vec<Reply>) {
        *self.inner.replies.lock().unwrap() = replies;
    }

    /// Every text frame the engine has sent, in order.
    pub fn received(&self) -> Vec<String> {
        self.inner.received.lock().unwrap().clone()
    }

    /// How many sockets have been accepted. Two means the engine reconnected.
    pub fn connections(&self) -> usize {
        *self.inner.connections.lock().unwrap()
    }

    /// Whether any received frame carries this `"type"`.
    pub fn saw_type(&self, wanted: &str) -> bool {
        self.received().iter().any(|raw| {
            serde_json::from_str::<Value>(raw)
                .ok()
                .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(|t| t == wanted))
                .unwrap_or(false)
        })
    }
}

/// Drive one accepted socket to its end. Every failure here means the client went
/// away, which for a stand-in is the same as the conversation being over — so this
/// returns nothing rather than an error nobody could act on. (Returning
/// `tungstenite::Error` also trips `clippy::result_large_err`: the variant is 136
/// bytes, carried on every read of every frame.)
async fn serve(stream: tokio::net::TcpStream, dialect: Dialect, shared: Arc<Shared>) {
    let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    *shared.connections.lock().unwrap() += 1;
    let (mut sink, mut source) = ws.split();
    let mut answered = false;

    while let Some(frame) = source.next().await {
        let Ok(Message::Text(text)) = frame else {
            continue;
        };
        shared.received.lock().unwrap().push(text.to_string());

        let kind = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string))
            .unwrap_or_default();

        // The engine asks to close; acknowledge the way the provider does.
        if kind == "session.close" {
            if matches!(dialect, Dialect::OpenAi | Dialect::OpenAiAssistant) {
                let _ = sink
                    .send(Message::text(r#"{"type":"session.closed"}"#))
                    .await;
            }
            let _ = sink.close().await;
            return;
        }

        // Audio is what triggers a reply. Everything before it (the session setup)
        // is recorded and acknowledged with silence, like the real one. The Gemini
        // check wants `data` too: `audioStreamEnd` is also a `realtimeInput` frame
        // and it carries no samples.
        let is_audio = match dialect {
            Dialect::OpenAi => kind == "session.input_audio_buffer.append",
            Dialect::OpenAiAssistant | Dialect::Qwen => kind == "input_audio_buffer.append",
            Dialect::Gemini => text.contains("realtimeInput") && text.contains("\"data\""),
        };
        if !is_audio || answered {
            continue;
        }
        answered = true;

        let replies = shared.replies.lock().unwrap().clone();
        for reply in replies {
            match reply {
                Reply::Drop => {
                    let _ = sink.close().await;
                    return;
                }
                other => {
                    for out in frames_for(&other, dialect) {
                        if sink.send(Message::text(out)).await.is_err() {
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// Render one [`Reply`] as the wire frames of `dialect`.
fn frames_for(reply: &Reply, dialect: Dialect) -> Vec<String> {
    let b64 = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
    match (reply, dialect) {
        (
            Reply::Transcript {
                original,
                translated,
            },
            Dialect::OpenAi,
        ) => vec![
            serde_json::json!({
                "type": "session.input_transcript.delta",
                "delta": original,
            })
            .to_string(),
            serde_json::json!({
                "type": "session.output_transcript.delta",
                "delta": translated,
            })
            .to_string(),
        ],
        (
            Reply::Transcript {
                original,
                translated,
            },
            Dialect::Gemini,
        ) => vec![serde_json::json!({
            "serverContent": {
                "inputTranscription": { "text": original },
                "outputTranscription": { "text": translated },
            }
        })
        .to_string()],
        (
            Reply::Transcript {
                original,
                translated,
            },
            Dialect::OpenAiAssistant,
        ) => vec![
            serde_json::json!({ "type": "input_audio_buffer.speech_started" }).to_string(),
            serde_json::json!({
                "type": "conversation.item.input_audio_transcription.delta",
                "delta": original,
            })
            .to_string(),
            serde_json::json!({ "type": "input_audio_buffer.speech_stopped" }).to_string(),
            serde_json::json!({
                "type": "response.output_audio_transcript.delta",
                "delta": translated,
            })
            .to_string(),
            serde_json::json!({ "type": "response.done" }).to_string(),
        ],
        (Reply::Audio(pcm), Dialect::OpenAiAssistant) => vec![serde_json::json!({
            "type": "response.output_audio.delta",
            "delta": b64(pcm),
        })
        .to_string()],
        (Reply::Error(message), Dialect::OpenAiAssistant) => {
            vec![serde_json::json!({ "type": "error", "message": message }).to_string()]
        }
        (
            Reply::Transcript {
                original,
                translated,
            },
            Dialect::Qwen,
        ) => vec![
            serde_json::json!({ "type": "session.updated" }).to_string(),
            // `delta` means "append this"; the `text`/`stash` pair is the snapshot
            // spelling. The increment is the one a test wants to be unambiguous.
            serde_json::json!({
                "type": "conversation.item.input_audio_transcription.delta",
                "delta": original,
            })
            .to_string(),
            serde_json::json!({
                "type": "response.audio_transcript.delta",
                "delta": translated,
            })
            .to_string(),
            serde_json::json!({ "type": "response.done" }).to_string(),
        ],
        (Reply::Audio(pcm), Dialect::Qwen) => vec![serde_json::json!({
            "type": "response.audio.delta",
            "delta": b64(pcm),
        })
        .to_string()],
        (Reply::Error(message), Dialect::Qwen) => vec![serde_json::json!({
            "type": "error",
            "error": { "message": message },
        })
        .to_string()],
        (Reply::Audio(pcm), Dialect::OpenAi) => vec![serde_json::json!({
            "type": "session.output_audio.delta",
            "delta": b64(pcm),
        })
        .to_string()],
        (Reply::Audio(pcm), Dialect::Gemini) => vec![serde_json::json!({
            "serverContent": {
                "modelTurn": {
                    "parts": [{ "inlineData": { "mimeType": "audio/pcm", "data": b64(pcm) } }]
                }
            }
        })
        .to_string()],
        (Reply::Error(message), Dialect::OpenAi) => {
            vec![serde_json::json!({ "type": "error", "message": message }).to_string()]
        }
        (Reply::Error(message), Dialect::Gemini) => {
            vec![serde_json::json!({ "error": { "message": message } }).to_string()]
        }
        (Reply::Drop, _) => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_transcript_renders_both_deltas() {
        let frames = frames_for(
            &Reply::Transcript {
                original: "ciao".into(),
                translated: "hello".into(),
            },
            Dialect::OpenAi,
        );
        assert_eq!(frames.len(), 2, "input and output are separate frames");
        assert!(frames[0].contains("session.input_transcript.delta"));
        assert!(frames[1].contains("session.output_transcript.delta"));
    }

    #[test]
    fn gemini_transcript_is_one_server_content_frame() {
        let frames = frames_for(
            &Reply::Transcript {
                original: "ciao".into(),
                translated: "hello".into(),
            },
            Dialect::Gemini,
        );
        assert_eq!(frames.len(), 1, "Gemini carries both in one serverContent");
        assert!(frames[0].contains("inputTranscription"));
        assert!(frames[0].contains("outputTranscription"));
    }

    #[test]
    fn audio_is_base64_on_both_wires() {
        for dialect in [Dialect::OpenAi, Dialect::Gemini] {
            let frames = frames_for(&Reply::Audio(vec![1, 2, 3, 4]), dialect);
            assert!(frames[0].contains("AQIDBA=="), "{dialect:?}: {frames:?}");
        }
    }

    #[test]
    fn a_drop_renders_nothing_because_it_is_a_close_not_a_frame() {
        assert!(frames_for(&Reply::Drop, Dialect::OpenAi).is_empty());
    }

    #[tokio::test]
    async fn the_base_url_carries_the_path_each_client_appends_to() {
        let openai = RealtimeMock::start(Dialect::OpenAi).await;
        assert!(openai.base_url().ends_with("/v1/realtime"));
        let gemini = RealtimeMock::start(Dialect::Gemini).await;
        assert!(gemini.base_url().starts_with("ws://"));
    }
}
