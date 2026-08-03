//! Low-level Qwen realtime client (DashScope / Alibaba Cloud Model Studio).
//!
//! One WebSocket session per **target language** against `/api-ws/v1/realtime`. The wire
//! protocol is the OpenAI-Realtime *shape* (`session.update`,
//! `input_audio_buffer.append`, `response.audio.delta`, …), so this module reads much
//! like [`super::openai`] — but four deltas matter:
//!
//! 1. **Auth is a header** (`Authorization: Bearer <DASHSCOPE_API_KEY>`) and the model
//!    is a **query parameter** (`?model=…`), not a body field.
//! 2. Model Studio ships **two incompatible families** behind that one endpoint — see
//!    [`QwenDialect`]. The default is the dedicated interpreter
//!    (`qwen3-livetranslate-flash-realtime`), which takes the source and target
//!    languages as real fields; the general omni family has to be driven by
//!    [`instructions_for`] instead. They also disagree on turn control and on how
//!    transcript frames accumulate — see [`TextUpdate`].
//! 3. Input is PCM16 @ **16 kHz** (our capture is 24 kHz → [`super::gemini::resample_pcm16_mono`])
//!    and output is PCM16 @ **24 kHz**, which is already our playback rate.
//! 4. Realtime models are **not in every region**. They exist in Beijing and Singapore;
//!    the US (Virginia) deployment, for instance, carries only batch ASR and text MT, so
//!    a key issued there authenticates fine and then finds no model to open. Region is
//!    part of the contract, not just latency — see `QWEN_REALTIME_ENDPOINT`.
//!
//! The pure build/parse helpers are unit-tested; the socket I/O needs a live key, which
//! `tests::qwen_live_protocol_probe` exercises end to end.

use base64::Engine as _;
use futures::stream::{SplitSink, SplitStream};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, USER_AGENT};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::config::QwenConfig;

type QwenStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
pub type QwenSink = SplitSink<QwenStream, Message>;
pub type QwenSource = SplitStream<QwenStream>;

/// Sample rate the client captures PCM16 at (spec 0043 / the premium audio path).
pub const CAPTURE_HZ: u32 = 24_000;
/// Sample rate the Qwen-Omni Realtime API expects for input audio.
pub const QWEN_INPUT_HZ: u32 = 16_000;
/// Sample rate Qwen-Omni Realtime returns audio at — already our playback rate, so
/// translated audio is forwarded to the browser untouched.
pub const QWEN_OUTPUT_HZ: u32 = 24_000;

/// Placeholder an operator can put in `QWEN_REALTIME_ENDPOINT` when their Model Studio
/// deployment routes per workspace (`wss://{workspace}.ap-southeast-1.maas.aliyuncs.com/…`).
/// Substituted from `QWEN_WORKSPACE_ID`; when the template has no placeholder the
/// workspace travels in the `X-DashScope-WorkSpace` header instead.
const WORKSPACE_PLACEHOLDER: &str = "{workspace}";

/// ASR model backing `input_audio_transcription` — what turns the speaker's own audio
/// into the ORIGINAL-language transcript. Model Studio's realtime ASR; override-free
/// because both session shapes must agree on it.
const QWEN_ASR_MODEL: &str = "qwen3-asr-flash-realtime";

/// Which wire dialect a model speaks.
///
/// Model Studio ships two families behind one endpoint and they are NOT interchangeable:
/// the translation contract lives in different fields AND the incremental server events
/// have different names. Swapping `QWEN_REALTIME_MODEL` between families without
/// switching dialect yields a session that connects, streams audio, and produces **no
/// subtitles at all** — the failure is silent, which is why this is modelled explicitly
/// instead of being left to a string compare at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QwenDialect {
    /// `qwen3-livetranslate-flash-realtime` & friends — a DEDICATED simultaneous
    /// interpreter. Target language is `session.translation.language`, source language is
    /// `session.input_audio_transcription.language`. No prompt engineering: the model has
    /// no assistant behaviour to suppress. It also owns its turn boundaries — see
    /// [`Self::accepts_manual_turn_control`].
    LiveTranslate,
    /// `qwen3.5-omni-flash-realtime` & friends — a general conversational omni model.
    /// Translation has to be imposed by [`instructions_for`].
    Omni,
}

impl QwenDialect {
    /// Infer the dialect from a model id. Anything containing `livetranslate` is the
    /// dedicated family; everything else is treated as omni, which is the conservative
    /// default (an omni session driven by instructions still translates — just less
    /// well — whereas sending livetranslate fields to an omni model is rejected).
    pub fn from_model(model: &str) -> Self {
        if model.contains("livetranslate") {
            Self::LiveTranslate
        } else {
            Self::Omni
        }
    }

    /// Whether this family accepts `input_audio_buffer.commit` / `response.create`.
    ///
    /// The livetranslate family REJECTS both — the live API answers
    /// `Invalid value: 'input_audio_buffer.commit'` — because a dedicated interpreter owns
    /// its own turn boundaries and never waits to be asked for output. Sending them is
    /// not fatal, but it puts two spurious errors on every session teardown, which then
    /// have to be explained away every time someone reads the logs.
    pub fn accepts_manual_turn_control(self) -> bool {
        matches!(self, Self::Omni)
    }
}

/// How one transcript frame relates to the text that came before it.
///
/// The API mixes both conventions, and the signal is the payload **field**, not the event
/// name — a `"delta"` field carries only what is new, a `"text"` field re-sends the whole
/// utterance so far. The event name is NOT a reliable proxy: the live API sends
/// `conversation.item.input_audio_transcription.delta` whose text is in a `"text"` field,
/// i.e. a snapshot under a delta-shaped name.
///
/// Appending snapshots produces captions like
/// `"Hello everyone,Hello everyone,Hello everyone, welcome…"` and
/// `"CiaoCiao aCiao a tutti…"` — both observed against the live API before this type
/// existed.
#[derive(Debug, Clone, PartialEq)]
pub enum TextUpdate {
    /// Append to what we already have (`*.delta`).
    Delta(String),
    /// Replace everything so far (`*.text`).
    Snapshot(String),
}

impl TextUpdate {
    /// Fold this update into `buffer`, applying the right semantics.
    pub fn apply(&self, buffer: &mut String) {
        match self {
            TextUpdate::Delta(d) => buffer.push_str(d),
            TextUpdate::Snapshot(s) => {
                buffer.clear();
                buffer.push_str(s);
            }
        }
    }

    /// The text this update carries, whatever its kind.
    pub fn text(&self) -> &str {
        match self {
            TextUpdate::Delta(t) | TextUpdate::Snapshot(t) => t,
        }
    }
}

/// A parsed Qwen Realtime server event. Only the frames we act on are modelled;
/// everything else yields nothing, so a stray frame never kills the session.
#[derive(Debug, Clone, PartialEq)]
pub enum QwenEvent {
    /// The session is configured and ready (`session.created` / `session.updated`).
    SessionReady,
    /// A fragment of the speaker's **original** transcript — see [`TextUpdate`] for
    /// whether it appends or replaces.
    InputTranscript(TextUpdate),
    /// The speaker's **original** transcript for one finished utterance, whole. Carries
    /// the same words the preceding [`QwenEvent::InputTranscript`] deltas streamed, so a
    /// consumer must use one or the other — never both, or the caption doubles.
    /// The translation path segments on [`QwenEvent::TurnComplete`] and ignores this; the
    /// transcribe-only path (webinar) uses it as its final-utterance signal.
    InputTranscriptDone(String),
    /// A fragment of the **translated** transcript (this session's lang) — see
    /// [`TextUpdate`] for whether it appends or replaces.
    OutputTranscript(TextUpdate),
    /// A chunk of translated **audio** (PCM16 @ 24 kHz, base64-decoded).
    OutputAudio(Vec<u8>),
    /// The model finished a response — a natural caption boundary.
    TurnComplete,
    /// A server-side error. The caller decides whether it's fatal.
    Error(String),
}

/// Build the Qwen Realtime WebSocket URL for `config`. Unlike Gemini the key is NOT in
/// the URL (it's a header), so this string is safe to log.
pub fn session_url(config: &QwenConfig, model: &str) -> String {
    let base = if config.endpoint.contains(WORKSPACE_PLACEHOLDER) {
        config.endpoint.replace(
            WORKSPACE_PLACEHOLDER,
            config.workspace_id.as_deref().unwrap_or_default(),
        )
    } else {
        config.endpoint.clone()
    };
    let sep = if base.contains('?') { '&' } else { '?' };
    format!("{base}{sep}model={model}")
}

/// Human-readable name for a language code, for the translation prompt. Falls back to
/// the raw code so an unmapped language still produces a usable instruction.
pub fn lang_name(code: &str) -> String {
    super::langmap::languages()
        .iter()
        .find(|l| l.code == code)
        .map(|l| l.english.clone())
        .unwrap_or_else(|| code.to_string())
}

/// The system prompt that turns a conversational omni model into a simultaneous
/// interpreter for `target_lang`.
///
/// Qwen-Omni Realtime has no `translationConfig` equivalent (cf. [`super::gemini`]), so
/// the whole contract lives here. It must be strict on three points or the tier
/// regresses into a chatbot: translate only, never answer, and never narrate. The
/// "already in the target language" clause mirrors Gemini's `echoTargetLanguage`, so a
/// speaker who briefly switches into the listener's language is echoed, not mangled.
pub fn instructions_for(target_lang: &str) -> String {
    let name = lang_name(target_lang);
    format!(
        "You are a simultaneous interpreter. Translate everything you hear into {name} \
         and speak the translation aloud. Rules, without exception: \
         (1) Output ONLY the translation — never answer questions, never add commentary, \
         greetings, apologies, or explanations, even if the speaker addresses you directly. \
         (2) Preserve the speaker's meaning, tone, register, and sentence order; do not \
         summarize, expand, or censor. \
         (3) If the speech is already in {name}, repeat it verbatim. \
         (4) If a segment is silence, noise, or unintelligible, output nothing at all. \
         (5) Keep proper nouns, numbers, and technical terms exact."
    )
}

/// The turn-detection block shared by both session shapes.
fn turn_detection(config: &QwenConfig) -> Value {
    serde_json::json!({
        "type": config.turn_detection,
        "silence_duration_ms": config.silence_duration_ms,
    })
}

/// The `session.update` frame configuring one **translation** session: speech in,
/// translated speech + transcript out. Used by the Standard tier.
///
/// The body depends on the model's [`QwenDialect`]:
///
/// * **LiveTranslate** — `translation.language` carries the target and
///   `input_audio_transcription.language` the source, both as real fields. No
///   `instructions`, and no `turn_detection`: a dedicated interpreter segments the stream
///   itself.
/// * **Omni** — no translation fields exist, so the contract is imposed by
///   [`instructions_for`] and turn detection has to be configured explicitly.
///
/// Common to both: `input_audio_format`/`output_audio_format` are `"pcm"` (the only value
/// the API accepts), and `input_audio_transcription` is what makes the speaker's ORIGINAL
/// words come back — without it the primary session has no original caption to echo.
///
/// `source_lang` may be `"auto"`, in which case the source field is OMITTED so the model
/// detects it. That matters: `input_audio_transcription.language` defaults to `"en"`, so
/// sending `"auto"` verbatim — or leaving it out while assuming detection — would have an
/// Italian speaker transcribed as though they were speaking English.
pub fn session_update_json(config: &QwenConfig, source_lang: &str, target_lang: &str) -> String {
    let dialect = QwenDialect::from_model(&config.model);
    let mut session = serde_json::json!({
        "modalities": ["text", "audio"],
        "input_audio_format": "pcm",
        "output_audio_format": "pcm",
    });

    let mut transcription = serde_json::json!({ "model": QWEN_ASR_MODEL });
    if source_lang != "auto" && !source_lang.is_empty() {
        transcription["language"] = Value::String(source_lang.to_string());
    }
    session["input_audio_transcription"] = transcription;

    match dialect {
        QwenDialect::LiveTranslate => {
            session["translation"] = serde_json::json!({ "language": target_lang });
        }
        QwenDialect::Omni => {
            session["instructions"] = Value::String(instructions_for(target_lang));
            session["turn_detection"] = turn_detection(config);
        }
    }

    // Pin a fixed timbre only when configured (`QWEN_VOICE`); otherwise the model keeps
    // its default voice for the target language.
    if let Some(v) = config.voice.as_deref() {
        session["voice"] = Value::String(v.to_string());
    }
    serde_json::json!({ "type": "session.update", "session": session }).to_string()
}

/// The `session.update` frame configuring a **transcribe-only** session: speech in,
/// source-language transcript out, no translation and no spoken response.
///
/// This runs against [`QWEN_ASR_MODEL`], NOT the tier's translate model. That is not an
/// optimisation, it is a correctness requirement: the livetranslate family REQUIRES a
/// `translation` parameter and closes the socket with `Invalid translation parameter.`
/// if asked to merely transcribe. A translator cannot be talked out of translating, so
/// the webinar uses the dedicated realtime ASR model instead — which is also what it
/// actually wants.
///
/// `source_lang` may be `"auto"`, in which case the field is OMITTED so the model
/// detects it; sending `"auto"` verbatim, or omitting it while assuming detection, gets
/// the server-side default of `"en"` and transcribes every other language as English.
pub fn transcribe_session_update_json(config: &QwenConfig, source_lang: &str) -> String {
    let mut transcription = serde_json::json!({ "model": QWEN_ASR_MODEL });
    if source_lang != "auto" && !source_lang.is_empty() {
        transcription["language"] = Value::String(source_lang.to_string());
    }
    serde_json::json!({
        "type": "session.update",
        "session": {
            "modalities": ["text"],
            "input_audio_format": "pcm",
            "input_audio_transcription": transcription,
            "turn_detection": turn_detection(config),
        }
    })
    .to_string()
}

/// The `input_audio_buffer.append` frame carrying one PCM16 chunk. `pcm16_16k` MUST
/// already be at [`QWEN_INPUT_HZ`] — see [`super::gemini::resample_pcm16_mono`].
pub fn audio_append_json(pcm16_16k: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(pcm16_16k);
    serde_json::json!({ "type": "input_audio_buffer.append", "audio": b64 }).to_string()
}

/// Commit the buffered audio, ending the current input turn. Sent when the speaker
/// stops so the tail of the last utterance is still translated (server VAD would
/// otherwise wait for silence that never arrives on a closed stream).
///
/// **Omni only** — see [`QwenDialect::accepts_manual_turn_control`].
pub fn audio_commit_json() -> &'static str {
    r#"{"type":"input_audio_buffer.commit"}"#
}

/// Ask the model to produce the response for the committed audio. Needed after a manual
/// commit; with `turn_detection` active the server also creates responses on its own.
///
/// **Omni only** — see [`QwenDialect::accepts_manual_turn_control`].
pub fn response_create_json() -> &'static str {
    r#"{"type":"response.create"}"#
}

/// Parse one Qwen Realtime server frame (JSON text) into zero or more [`QwenEvent`]s.
/// Pure — for tests. Unknown types, empty deltas, and undecodable audio are skipped
/// rather than erroring, so an unmodelled frame can never end a live session.
pub fn parse_server_message(text: &str) -> Vec<QwenEvent> {
    let v: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let Some(kind) = v.get("type").and_then(|t| t.as_str()) else {
        return Vec::new();
    };
    let delta = |key: &str| -> Option<String> {
        v.get(key)
            .and_then(|d| d.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };

    // Text arrives with DIFFERENT semantics, and the signal is the payload FIELD, not the
    // event name — verified against the live API, which sends
    // `conversation.item.input_audio_transcription.delta` carrying its text in a `"text"`
    // field. Trusting the name there appends cumulative snapshots and yields
    // `"CiaoCiao aCiao a tutti…"`.
    //
    //   field "delta" → an increment, append it
    //   field "text"  → the whole utterance so far, replace with it
    //
    // A snapshot is additionally split in two: `text` is the CONFIRMED prefix and `stash`
    // the tentative tail still being recognised. Early frames look like
    // `{"text":"","stash":"ciao a tutti"}` — so reading `text` alone shows an empty
    // caption for most of an utterance, which is exactly how the speaker's own subtitle
    // came back blank. The live partial is their concatenation; once the recognizer
    // commits, `stash` empties and `text` carries the final on its own.
    let snapshot = || -> Option<String> {
        let text = v.get("text").and_then(|t| t.as_str()).unwrap_or_default();
        let stash = v.get("stash").and_then(|t| t.as_str()).unwrap_or_default();
        let joined = format!("{text}{stash}");
        (!joined.is_empty()).then_some(joined)
    };
    let update = || -> Option<TextUpdate> {
        if let Some(d) = delta("delta") {
            return Some(TextUpdate::Delta(d));
        }
        snapshot().map(TextUpdate::Snapshot)
    };

    match kind {
        "session.created" | "session.updated" => vec![QwenEvent::SessionReady],

        // The speaker's own words. The incremental form streams the utterance;
        // `.completed` repeats it whole. Both are surfaced as DISTINCT events so each
        // consumer picks the one it needs — emitting them into the same buffer would
        // double the caption. `.text` is the livetranslate spelling of `.delta`.
        "conversation.item.input_audio_transcription.delta"
        | "conversation.item.input_audio_transcription.text" => update()
            .map(|u| vec![QwenEvent::InputTranscript(u)])
            .unwrap_or_default(),
        "conversation.item.input_audio_transcription.completed" => delta("transcript")
            .or_else(|| delta("text"))
            .map(|t| vec![QwenEvent::InputTranscriptDone(t)])
            .unwrap_or_default(),

        // The translation, as text. `response.audio_transcript.*` is the transcript OF
        // the spoken audio; `response.text.*` appears when a session runs text-only.
        // Both mean the same thing to us, in either spelling.
        "response.audio_transcript.delta"
        | "response.audio_transcript.text"
        | "response.text.delta"
        | "response.text.text" => update()
            .map(|u| vec![QwenEvent::OutputTranscript(u)])
            .unwrap_or_default(),

        // Translated speech, base64 PCM16 @ 24 kHz.
        "response.audio.delta" => delta("delta")
            .and_then(|d| base64::engine::general_purpose::STANDARD.decode(d).ok())
            .filter(|b| !b.is_empty())
            .map(|b| vec![QwenEvent::OutputAudio(b)])
            .unwrap_or_default(),

        // End of one model response = a caption boundary. `session.finished` is the
        // livetranslate family's end-of-session frame; treat it as a final boundary so a
        // trailing segment is still flushed.
        "response.done" | "session.finished" => vec![QwenEvent::TurnComplete],

        "error" => {
            let msg = v
                .pointer("/error/message")
                .and_then(|m| m.as_str())
                .or_else(|| v.get("message").and_then(|m| m.as_str()))
                .unwrap_or("qwen realtime error");
            vec![QwenEvent::Error(msg.to_string())]
        }

        _ => Vec::new(),
    }
}

/// Open a Qwen Realtime WebSocket for `target_lang` and send the initial
/// `session.update`. Returns the split sink (send audio) and source (read events).
pub async fn open_session(
    config: &QwenConfig,
    source_lang: &str,
    target_lang: &str,
) -> Result<(QwenSink, QwenSource), String> {
    let session = session_update_json(config, source_lang, target_lang);
    let opened = connect(config, &config.model, session).await?;
    tracing::info!(
        %source_lang,
        %target_lang,
        model = %config.model,
        dialect = ?QwenDialect::from_model(&config.model),
        "qwen: translate session connecting"
    );
    Ok(opened)
}

/// Open a **transcribe-only** Qwen Realtime WebSocket (see
/// [`transcribe_session_update_json`]) — source-language transcript, no translation,
/// no spoken response. Dials [`QWEN_ASR_MODEL`], not the tier's translate model.
pub async fn open_transcribe_session(
    config: &QwenConfig,
    source_lang: &str,
) -> Result<(QwenSink, QwenSource), String> {
    let session = transcribe_session_update_json(config, source_lang);
    let opened = connect(config, QWEN_ASR_MODEL, session).await?;
    tracing::info!(%source_lang, model = %QWEN_ASR_MODEL, "qwen: transcribe session connecting");
    Ok(opened)
}

/// Dial the endpoint, authenticate, and send `session` as the opening frame. Shared by
/// both session shapes so the auth/workspace/header handling exists once.
async fn connect(
    config: &QwenConfig,
    model: &str,
    session: String,
) -> Result<(QwenSink, QwenSource), String> {
    let url = session_url(config, model);
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|e| format!("invalid qwen url: {e}"))?;
    {
        let headers = request.headers_mut();
        headers.insert(
            AUTHORIZATION,
            format!("Bearer {}", config.api_key)
                .parse()
                .map_err(|_| "invalid qwen api key".to_string())?,
        );
        headers.insert(USER_AGENT, "voxtranslate/1".parse().unwrap());
        // Only needed when the account routes by workspace AND the endpoint template
        // didn't already carry it in the host.
        if let Some(ws) = config.workspace_id.as_deref() {
            if !config.endpoint.contains(WORKSPACE_PLACEHOLDER) {
                if let Ok(v) = ws.parse() {
                    headers.insert("X-DashScope-WorkSpace", v);
                }
            }
        }
    }

    let (ws, _resp) = connect_async(request)
        .await
        .map_err(|e| format!("qwen connect failed: {e}"))?;

    use futures::SinkExt as _;
    use futures::StreamExt as _;
    let (mut sink, source) = ws.split();
    sink.send(Message::text(session))
        .await
        .map_err(|e| format!("qwen session.update failed: {e}"))?;
    Ok((sink, source))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LiveTranslate config — the shipped default.
    fn cfg() -> QwenConfig {
        QwenConfig {
            model: "qwen3-livetranslate-flash-realtime".into(),
            ..omni_cfg()
        }
    }

    /// Omni config — the alternative family, kept working behind the dialect switch.
    fn omni_cfg() -> QwenConfig {
        QwenConfig {
            api_key: "SECRET_KEY".into(),
            model: "qwen3.5-omni-flash-realtime".into(),
            endpoint: "wss://dashscope-intl.aliyuncs.com/api-ws/v1/realtime".into(),
            workspace_id: None,
            voice: None,
            turn_detection: "semantic_vad".into(),
            silence_duration_ms: 500,
            cost_per_minute: 0.0036,
            markup: 0.25,
            max_sessions: 32,
        }
    }

    /// LIVE probe against the real Qwen-Omni Realtime API: open a translation session,
    /// stream a PCM16 sample in real time, and print every event the server sends plus
    /// the raw frames we did NOT model. This is how the wire contract in this module was
    /// validated — the docs give the event names, this proves them.
    ///
    /// `#[ignore]` because it needs a real key + network and bills Model Studio minutes.
    /// Run it explicitly (it prints with --nocapture):
    /// ```text
    /// QWEN_PROBE_PCM=/tmp/utt_24k.pcm cargo test -p voxtranslate-server --lib \
    ///   engine::qwen::tests::qwen_live_protocol_probe -- --ignored --nocapture
    /// ```
    /// `QWEN_PROBE_PCM` must be mono PCM16 little-endian at 24 kHz (the capture rate).
    /// `QWEN_PROBE_LANG` overrides the target language (default `en`).
    #[tokio::test]
    #[ignore]
    async fn qwen_live_protocol_probe() {
        use futures::{SinkExt as _, StreamExt as _};
        use std::collections::BTreeSet;
        use tokio::time::{sleep, timeout, Duration, Instant};

        let _ = dotenvy::dotenv();
        let key = match crate::config::QwenConfig::from_env() {
            c if !c.api_key.trim().is_empty() => c,
            _ => {
                eprintln!("skip: set DASHSCOPE_API_KEY (or QWEN_API_KEY)");
                return;
            }
        };
        let pcm_path = match std::env::var("QWEN_PROBE_PCM") {
            Ok(p) if !p.trim().is_empty() => p,
            _ => {
                eprintln!("skip: set QWEN_PROBE_PCM to a 24kHz mono PCM16 file");
                return;
            }
        };
        let pcm = std::fs::read(&pcm_path).expect("read PCM sample");
        let target = std::env::var("QWEN_PROBE_LANG").unwrap_or_else(|_| "en".into());
        // The bundled sample is Italian speech; override for another clip.
        let source_lang = std::env::var("QWEN_PROBE_SOURCE").unwrap_or_else(|_| "it".into());

        println!("\n=== Qwen live probe ===");
        // Key SHAPE only — never the value. A key that fails header parsing almost always
        // has a stray quote, space, or newline picked up from .env.
        let k = key.api_key.as_str();
        let bad: Vec<(usize, u8)> = k
            .bytes()
            .enumerate()
            .filter(|(_, b)| !b.is_ascii_graphic())
            .collect();
        println!(
            "  key      : len={} prefix={:?} bad_bytes={:?} segments={:?}",
            k.len(),
            k.chars().take(3).collect::<String>(),
            bad,
            k.split_whitespace().map(str::len).collect::<Vec<_>>()
        );
        println!("  endpoint : {}", session_url(&key, &key.model));
        println!("  target   : {target}");
        println!("  audio    : {:.2}s", pcm.len() as f64 / 2.0 / 24000.0);

        let dialect = QwenDialect::from_model(&key.model);
        println!("  dialect  : {dialect:?}");
        let (mut sink, mut source) = match open_session(&key, &source_lang, &target).await {
            Ok(pair) => pair,
            Err(e) => panic!("CONNECT FAILED: {e}"),
        };
        println!("  connect  : OK\n");

        // Feed in real time (100 ms chunks) so server VAD behaves as it would live.
        const BYTES_PER_CHUNK: usize = 24000 * 2 * 100 / 1000;
        let feeder = tokio::spawn(async move {
            for ch in pcm.chunks(BYTES_PER_CHUNK) {
                let c16 = super::super::gemini::resample_pcm16_mono(ch, CAPTURE_HZ, QWEN_INPUT_HZ);
                if sink
                    .send(Message::text(audio_append_json(&c16)))
                    .await
                    .is_err()
                {
                    return;
                }
                sleep(Duration::from_millis(100)).await;
            }
            // Close the turn explicitly, as the engine does when a speaker stops — but
            // only where the family accepts it (livetranslate rejects both frames).
            if dialect.accepts_manual_turn_control() {
                let _ = sink.send(Message::text(audio_commit_json())).await;
                let _ = sink.send(Message::text(response_create_json())).await;
            }
        });

        let mut original = String::new();
        let mut translated = String::new();
        let mut audio_bytes = 0usize;
        let mut modelled: BTreeSet<String> = BTreeSet::new();
        let mut unmodelled: BTreeSet<String> = BTreeSet::new();
        // event type -> payload keys, for the transcript-bearing frames.
        let mut shapes: BTreeSet<String> = BTreeSet::new();
        let mut raw_input_frames = 0usize;
        let mut errors: Vec<String> = Vec::new();
        let mut first_audio: Option<u128> = None;
        let t0 = Instant::now();
        let deadline = Instant::now() + Duration::from_secs(45);

        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let Ok(Some(Ok(msg))) = timeout(left, source.next()).await else {
                break;
            };
            let text = match msg {
                Message::Text(t) => t.to_string(),
                Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
                Message::Close(c) => {
                    println!("  << CLOSE {c:?}");
                    break;
                }
                _ => continue,
            };
            // Record the raw `type` AND its payload keys. The keys are what actually decide
            // how a frame must be folded — the event NAME turned out to be an unreliable
            // proxy (an event called `….delta` can still carry its text in a `text` field),
            // so print the ground truth rather than inferring it.
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(k) = v.get("type").and_then(|t| t.as_str()) {
                    let keys: Vec<String> = v
                        .as_object()
                        .map(|o| {
                            o.keys()
                                .filter(|k| k.as_str() != "type" && k.as_str() != "event_id")
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default();
                    if k.contains("transcript") || k.contains("text") {
                        shapes.insert(format!("{k} -> {keys:?}"));
                    }
                    // Raw dump of the INPUT-transcription frames: the probe's own test
                    // audio, so there is nothing sensitive to redact, and the values are
                    // the only way to see why a modelled event yields no text.
                    if k.starts_with("conversation.item.input_audio_transcription")
                        && raw_input_frames < 6
                    {
                        println!("  RAW {k}: {text}");
                        raw_input_frames += 1;
                    }
                    if parse_server_message(&text).is_empty() {
                        unmodelled.insert(k.to_string());
                    } else {
                        modelled.insert(k.to_string());
                    }
                }
            }
            for event in parse_server_message(&text) {
                match event {
                    QwenEvent::SessionReady => println!("  << session ready"),
                    QwenEvent::InputTranscript(u) => u.apply(&mut original),
                    QwenEvent::InputTranscriptDone(t) => println!("  << input done: {t:?}"),
                    QwenEvent::OutputTranscript(u) => u.apply(&mut translated),
                    QwenEvent::OutputAudio(b) => {
                        if first_audio.is_none() {
                            first_audio = Some(t0.elapsed().as_millis());
                        }
                        audio_bytes += b.len();
                    }
                    QwenEvent::TurnComplete => println!("  << turn complete"),
                    QwenEvent::Error(e) => {
                        println!("  << ERROR: {e}");
                        errors.push(e);
                    }
                }
            }
            if !translated.is_empty() && audio_bytes > 0 && !modelled.is_empty() {
                // Keep reading a little past the first results to catch the turn end.
                if t0.elapsed() > Duration::from_secs(25) {
                    break;
                }
            }
        }
        feeder.abort();

        println!("\n--- results ---");
        println!("  original   : {original:?}");
        println!("  translated : {translated:?}");
        println!(
            "  audio out  : {audio_bytes} bytes ({:.2}s @24kHz), first at {first_audio:?} ms",
            audio_bytes as f64 / 2.0 / 24000.0
        );
        println!("  modelled   : {modelled:?}");
        println!("  UNMODELLED : {unmodelled:?}");
        println!("  SHAPES     :");
        for s in &shapes {
            println!("    {s}");
        }
        println!("  errors     : {errors:?}");

        assert!(errors.is_empty(), "server reported errors: {errors:?}");
        assert!(
            !original.is_empty() || !translated.is_empty(),
            "no transcript came back — the session shape or event names are wrong"
        );
    }

    /// LIVE probe of the TRANSCRIBE-ONLY session — the shape the webinar ingest uses.
    /// Same harness as `qwen_live_protocol_probe`, but exercising
    /// [`open_transcribe_session`] and printing what the webinar consumer would fold.
    ///
    /// `#[ignore]`: needs a real key + network.
    #[tokio::test]
    #[ignore]
    async fn qwen_live_transcribe_probe() {
        use futures::{SinkExt as _, StreamExt as _};
        use std::collections::BTreeSet;
        use tokio::time::{sleep, timeout, Duration, Instant};

        let _ = dotenvy::dotenv();
        let cfg = crate::config::QwenConfig::from_env();
        if cfg.api_key.trim().is_empty() {
            eprintln!("skip: set DASHSCOPE_API_KEY (or QWEN_API_KEY)");
            return;
        }
        let Ok(pcm_path) = std::env::var("QWEN_PROBE_PCM") else {
            eprintln!("skip: set QWEN_PROBE_PCM");
            return;
        };
        let pcm = std::fs::read(&pcm_path).expect("read PCM sample");
        // The bundled sample is Italian; the webinar passes its host's source language.
        let source_lang = std::env::var("QWEN_PROBE_SOURCE").unwrap_or_else(|_| "it".into());

        println!("\n=== Qwen TRANSCRIBE-only probe (webinar shape) ===");
        println!("  model    : {}", cfg.model);
        println!(
            "  session  : {}",
            transcribe_session_update_json(&cfg, &source_lang)
        );

        let (mut sink, mut source) = match open_transcribe_session(&cfg, &source_lang).await {
            Ok(p) => p,
            Err(e) => panic!("CONNECT FAILED: {e}"),
        };
        println!("  connect  : OK\n");

        const BYTES_PER_CHUNK: usize = 24000 * 2 * 100 / 1000;
        let feeder = tokio::spawn(async move {
            for ch in pcm.chunks(BYTES_PER_CHUNK) {
                let c16 = super::super::gemini::resample_pcm16_mono(ch, CAPTURE_HZ, QWEN_INPUT_HZ);
                if sink
                    .send(Message::text(audio_append_json(&c16)))
                    .await
                    .is_err()
                {
                    return;
                }
                sleep(Duration::from_millis(100)).await;
            }
            // Keep streaming SILENCE, as a live host's mic does between sentences. Turn
            // detection keys off silence in the AUDIO, not off the socket going quiet, so
            // without this the utterance is never finalised.
            let silence = vec![0u8; BYTES_PER_CHUNK];
            for _ in 0..30 {
                let c16 =
                    super::super::gemini::resample_pcm16_mono(&silence, CAPTURE_HZ, QWEN_INPUT_HZ);
                if sink
                    .send(Message::text(audio_append_json(&c16)))
                    .await
                    .is_err()
                {
                    return;
                }
                sleep(Duration::from_millis(100)).await;
            }
        });

        // Exactly what webinar::stt::fold_event does with the stream.
        let mut buf = String::new();
        let mut interims = 0usize;
        let mut finals: Vec<String> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut errors: Vec<String> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(35);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let Ok(Some(Ok(msg))) = timeout(left, source.next()).await else {
                break;
            };
            let text = match msg {
                Message::Text(t) => t.to_string(),
                Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
                Message::Close(c) => {
                    println!("  << CLOSE {c:?}");
                    break;
                }
                _ => continue,
            };
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(k) = v.get("type").and_then(|t| t.as_str()) {
                    seen.insert(k.to_string());
                }
            }
            for ev in parse_server_message(&text) {
                if let QwenEvent::Error(e) = &ev {
                    errors.push(e.clone());
                }
                match ev {
                    QwenEvent::InputTranscript(u) => {
                        u.apply(&mut buf);
                        interims += 1;
                    }
                    QwenEvent::InputTranscriptDone(t) => {
                        finals.push(t);
                        buf.clear();
                    }
                    _ => {}
                }
            }
        }
        feeder.abort();

        println!("\n--- results ---");
        println!("  interim frames : {interims}");
        println!("  last partial   : {buf:?}");
        println!("  FINALS         : {finals:?}");
        println!("  event types    : {seen:?}");
        println!("  errors         : {errors:?}");
    }

    #[test]
    fn url_carries_the_model_and_never_the_key() {
        let url = session_url(&cfg(), &cfg().model);
        assert_eq!(
            url,
            "wss://dashscope-intl.aliyuncs.com/api-ws/v1/realtime?model=qwen3-livetranslate-flash-realtime"
        );
        // Qwen auths via an Authorization HEADER, so — unlike Gemini — the URL is safe
        // to log. Guard that nobody moves the key into the query string.
        assert!(!url.contains("SECRET_KEY"));
    }

    #[test]
    fn url_substitutes_a_workspace_host_template() {
        let mut c = cfg();
        c.endpoint = "wss://{workspace}.ap-southeast-1.maas.aliyuncs.com/api-ws/v1/realtime".into();
        c.workspace_id = Some("llm-abc123".into());
        assert_eq!(
            session_url(&c, &c.model),
            "wss://llm-abc123.ap-southeast-1.maas.aliyuncs.com/api-ws/v1/realtime?model=qwen3-livetranslate-flash-realtime"
        );
    }

    #[test]
    fn url_appends_model_with_ampersand_when_query_present() {
        let mut c = cfg();
        c.endpoint = "wss://example.test/realtime?region=intl".into();
        assert!(session_url(&c, &c.model)
            .ends_with("?region=intl&model=qwen3-livetranslate-flash-realtime"));
    }

    #[test]
    fn dialect_is_inferred_from_the_model_family() {
        assert_eq!(
            QwenDialect::from_model("qwen3-livetranslate-flash-realtime"),
            QwenDialect::LiveTranslate
        );
        assert_eq!(
            QwenDialect::from_model("qwen3-livetranslate-flash-realtime-2025-09-22"),
            QwenDialect::LiveTranslate
        );
        assert_eq!(
            QwenDialect::from_model("qwen3.5-omni-flash-realtime"),
            QwenDialect::Omni
        );
        // Unknown ids fall back to omni — the conservative side (an instruction-driven
        // session still translates; livetranslate fields sent to omni are rejected).
        assert_eq!(QwenDialect::from_model("something-new"), QwenDialect::Omni);
    }

    #[test]
    fn livetranslate_session_uses_real_language_fields_not_a_prompt() {
        let v: Value = serde_json::from_str(&session_update_json(&cfg(), "it", "pl")).unwrap();
        assert_eq!(v["type"], "session.update");
        let s = &v["session"];
        // Only "pcm" is accepted by the API for both directions.
        assert_eq!(s["input_audio_format"], "pcm");
        assert_eq!(s["output_audio_format"], "pcm");
        // Speech + text: we need the audio AND its transcript for the caption.
        assert_eq!(s["modalities"][0], "text");
        assert_eq!(s["modalities"][1], "audio");
        // Target and source are FIELDS on this family, not prose.
        assert_eq!(s["translation"]["language"], "pl");
        assert_eq!(s["input_audio_transcription"]["language"], "it");
        assert_eq!(s["input_audio_transcription"]["model"], QWEN_ASR_MODEL);
        // A dedicated interpreter needs neither a prompt nor explicit turn detection.
        assert!(s.get("instructions").is_none());
        assert!(s.get("turn_detection").is_none());
        // No voice configured → the model keeps its default.
        assert!(s.get("voice").is_none());
    }

    #[test]
    fn auto_source_omits_the_language_so_it_is_detected() {
        // `input_audio_transcription.language` DEFAULTS TO "en" server-side, so sending
        // "auto" verbatim — or setting it wrongly — would transcribe an Italian speaker
        // as English. Omitting the field is the only correct encoding of "detect it".
        let v: Value = serde_json::from_str(&session_update_json(&cfg(), "auto", "en")).unwrap();
        let t = &v["session"]["input_audio_transcription"];
        assert!(
            t.get("language").is_none(),
            "auto must omit the source language"
        );
        assert_eq!(t["model"], QWEN_ASR_MODEL); // transcription itself stays enabled
        assert_eq!(v["session"]["translation"]["language"], "en");
    }

    #[test]
    fn omni_session_falls_back_to_the_instruction_contract() {
        let v: Value = serde_json::from_str(&session_update_json(&omni_cfg(), "it", "pl")).unwrap();
        let s = &v["session"];
        // No translation field exists on this family — the contract is the prompt.
        assert!(s.get("translation").is_none());
        let instructions = s["instructions"].as_str().unwrap();
        assert!(
            instructions.contains("Polish"),
            "must name the target language"
        );
        assert!(instructions.contains("ONLY the translation"));
        // And turn detection has to be configured explicitly here.
        assert_eq!(s["turn_detection"]["type"], "semantic_vad");
        // The source language is still pinned for transcription.
        assert_eq!(s["input_audio_transcription"]["language"], "it");
    }

    #[test]
    fn session_update_pins_voice_when_configured() {
        let mut c = cfg();
        c.voice = Some("Tina".into());
        let v: Value = serde_json::from_str(&session_update_json(&c, "it", "es")).unwrap();
        assert_eq!(v["session"]["voice"], "Tina");
    }

    #[test]
    fn lang_name_resolves_known_codes_and_falls_back() {
        assert_eq!(lang_name("it"), "Italian");
        assert_eq!(lang_name("en"), "English");
        // Unknown code degrades to itself rather than producing an empty instruction.
        assert_eq!(lang_name("zz"), "zz");
    }

    #[test]
    fn audio_append_frame_carries_b64_pcm() {
        let v: Value = serde_json::from_str(&audio_append_json(&[0u8, 255, 128, 7])).unwrap();
        assert_eq!(v["type"], "input_audio_buffer.append");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(v["audio"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, vec![0u8, 255, 128, 7]);
    }

    #[test]
    fn parses_session_ready_and_turn_boundary() {
        assert_eq!(
            parse_server_message(r#"{"type":"session.updated","session":{}}"#),
            vec![QwenEvent::SessionReady]
        );
        assert_eq!(
            parse_server_message(r#"{"type":"response.done","response":{}}"#),
            vec![QwenEvent::TurnComplete]
        );
    }

    #[test]
    fn parses_input_and_output_transcript_deltas() {
        assert_eq!(
            parse_server_message(
                r#"{"type":"conversation.item.input_audio_transcription.delta","delta":"ciao "}"#
            ),
            vec![QwenEvent::InputTranscript(TextUpdate::Delta(
                "ciao ".into()
            ))]
        );
        assert_eq!(
            parse_server_message(r#"{"type":"response.audio_transcript.delta","delta":"hello "}"#),
            vec![QwenEvent::OutputTranscript(TextUpdate::Delta(
                "hello ".into()
            ))]
        );
        // A text-only session reports the translation as response.text.delta instead.
        assert_eq!(
            parse_server_message(r#"{"type":"response.text.delta","delta":"hello"}"#),
            vec![QwenEvent::OutputTranscript(TextUpdate::Delta(
                "hello".into()
            ))]
        );
    }

    #[test]
    fn text_events_are_snapshots_delta_events_are_increments() {
        // The distinction is ONLY in the event name, and getting it wrong is what produced
        // `"Hello everyone,Hello everyone,Hello everyone, welcome…"` against the live API.
        let snap = parse_server_message(
            r#"{"type":"response.audio_transcript.text","text":"Hello everyone,"}"#,
        );
        assert_eq!(
            snap,
            vec![QwenEvent::OutputTranscript(TextUpdate::Snapshot(
                "Hello everyone,".into()
            ))]
        );
        let inc = parse_server_message(
            r#"{"type":"response.audio_transcript.delta","delta":" welcome"}"#,
        );
        assert_eq!(
            inc,
            vec![QwenEvent::OutputTranscript(TextUpdate::Delta(
                " welcome".into()
            ))]
        );

        // And folding them does the right thing in each case.
        let mut buf = String::from("Hello everyone,");
        if let QwenEvent::OutputTranscript(u) = &inc[0] {
            u.apply(&mut buf);
        }
        assert_eq!(buf, "Hello everyone, welcome");
        if let QwenEvent::OutputTranscript(u) = &snap[0] {
            u.apply(&mut buf);
        }
        assert_eq!(
            buf, "Hello everyone,",
            "a snapshot must REPLACE, not append"
        );
    }

    #[test]
    fn snapshot_joins_the_confirmed_prefix_with_the_pending_stash() {
        // Alibaba's realtime ASR streams `text` = confirmed prefix, `stash` = tail still
        // being recognised. For most of an utterance `text` is EMPTY and everything is in
        // `stash`, so reading `text` alone left the speaker's own caption blank for the
        // whole sentence — observed live before this join existed.
        let early = parse_server_message(
            r#"{"type":"conversation.item.input_audio_transcription.text","text":"","stash":"ciao a tutti"}"#,
        );
        assert_eq!(
            early,
            vec![QwenEvent::InputTranscript(TextUpdate::Snapshot(
                "ciao a tutti".into()
            ))]
        );

        // Once the recognizer commits, the confirmed prefix carries it and the tail
        // continues in `stash` — the caption is their concatenation, in order.
        let mid = parse_server_message(
            r#"{"type":"conversation.item.input_audio_transcription.text","text":"ciao a tutti, ","stash":"benvenuti"}"#,
        );
        assert_eq!(
            mid,
            vec![QwenEvent::InputTranscript(TextUpdate::Snapshot(
                "ciao a tutti, benvenuti".into()
            ))]
        );

        // Fully committed: no stash left, `text` stands alone.
        let done = parse_server_message(
            r#"{"type":"conversation.item.input_audio_transcription.text","text":"ciao a tutti, benvenuti","stash":""}"#,
        );
        assert_eq!(
            done,
            vec![QwenEvent::InputTranscript(TextUpdate::Snapshot(
                "ciao a tutti, benvenuti".into()
            ))]
        );

        // Both empty → nothing to show, not an empty caption.
        assert!(parse_server_message(
            r#"{"type":"conversation.item.input_audio_transcription.text","text":"","stash":""}"#
        )
        .is_empty());
    }

    #[test]
    fn livetranslate_rejects_manual_turn_control() {
        // The live API answers `Invalid value: 'input_audio_buffer.commit'` for this
        // family — it owns its own turn boundaries.
        assert!(!QwenDialect::LiveTranslate.accepts_manual_turn_control());
        assert!(QwenDialect::Omni.accepts_manual_turn_control());
    }

    #[test]
    fn completed_input_transcription_is_a_distinct_event() {
        // `.completed` repeats the whole segment the deltas already streamed, so it gets
        // its OWN variant — a consumer that used both would print the sentence twice.
        assert_eq!(
            parse_server_message(
                r#"{"type":"conversation.item.input_audio_transcription.completed","transcript":"ciao a tutti"}"#
            ),
            vec![QwenEvent::InputTranscriptDone("ciao a tutti".into())]
        );
    }

    #[test]
    fn transcribe_session_is_asr_shaped_not_translator_shaped() {
        let v: Value = serde_json::from_str(&transcribe_session_update_json(&cfg(), "it")).unwrap();
        let s = &v["session"];
        // Text only: the webinar renders subtitles, and paying for a spoken reply nobody
        // plays would be pure waste.
        assert_eq!(s["modalities"][0], "text");
        assert_eq!(s["modalities"].as_array().unwrap().len(), 1);
        assert!(s.get("output_audio_format").is_none());
        // Transcription on, and pinned to the HOST's language — the server-side default
        // is "en", which would transcribe an Italian webinar as English.
        assert_eq!(s["input_audio_transcription"]["model"], QWEN_ASR_MODEL);
        assert_eq!(s["input_audio_transcription"]["language"], "it");
        // NO `instructions` and NO `translation`. Sending either to the ASR model, or
        // asking the livetranslate model to merely transcribe, closes the socket with
        // `Invalid translation parameter.` — observed against the live API.
        assert!(s.get("instructions").is_none());
        assert!(s.get("translation").is_none());
        assert_eq!(s["turn_detection"]["type"], "semantic_vad");
    }

    #[test]
    fn transcribe_session_omits_an_auto_source_language() {
        let v: Value =
            serde_json::from_str(&transcribe_session_update_json(&cfg(), "auto")).unwrap();
        let tr = &v["session"]["input_audio_transcription"];
        assert!(tr.get("language").is_none(), "auto must omit the language");
        assert_eq!(tr["model"], QWEN_ASR_MODEL);
    }

    #[test]
    fn both_session_shapes_agree_on_the_asr_model() {
        // A mismatch would silently give the two surfaces different transcript quality.
        let translate: Value =
            serde_json::from_str(&session_update_json(&cfg(), "it", "en")).unwrap();
        let transcribe: Value =
            serde_json::from_str(&transcribe_session_update_json(&cfg(), "it")).unwrap();
        assert_eq!(
            translate["session"]["input_audio_transcription"]["model"],
            transcribe["session"]["input_audio_transcription"]["model"]
        );
    }

    #[test]
    fn parses_audio_delta_and_skips_bad_payloads() {
        let b64 = base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3, 4]);
        assert_eq!(
            parse_server_message(&format!(
                r#"{{"type":"response.audio.delta","delta":"{b64}"}}"#
            )),
            vec![QwenEvent::OutputAudio(vec![1, 2, 3, 4])]
        );
        // Garbage base64 and empty deltas degrade to nothing, never panic.
        assert!(
            parse_server_message(r#"{"type":"response.audio.delta","delta":"!!!!"}"#).is_empty()
        );
        assert!(parse_server_message(r#"{"type":"response.audio.delta","delta":""}"#).is_empty());
    }

    #[test]
    fn parses_error_shapes_and_ignores_junk() {
        assert_eq!(
            parse_server_message(r#"{"type":"error","error":{"message":"quota exceeded"}}"#),
            vec![QwenEvent::Error("quota exceeded".into())]
        );
        assert!(parse_server_message("not json").is_empty());
        assert!(parse_server_message(r#"{"no_type":1}"#).is_empty());
        // An unmodelled frame must be inert, not fatal.
        assert!(parse_server_message(r#"{"type":"input_audio_buffer.speech_started"}"#).is_empty());
    }
}
