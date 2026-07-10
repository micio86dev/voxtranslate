//! Low-level OpenAI Realtime API client for the Dashboard Help Assistant.
//!
//! Mirrors `voice_assistant_client`, but the session is configured with a STATIC
//! product-knowledge prompt (no RAG). This keeps the module free of embedder
//! dependencies and makes `build_help_instructions` a pure, unit-testable function.
//!
//! Connects to `wss://api.openai.com/v1/realtime?model=…` with
//! `Authorization: Bearer` only — the deprecated `OpenAI-Beta` header is NOT sent.

use futures::stream::{SplitSink, SplitStream};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{HeaderValue, AUTHORIZATION};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::config::HelpAssistantConfig;

// Re-export shared event types and helpers from the VA client so callers can
// use a single import path for the relay loop.
pub use crate::engine::voice_assistant_client::{
    build_cost_tick_json, build_session_end_json, format_cost_display, parse_realtime_event,
    va_audio_append_json as ha_audio_append_json, VaEvent as HaEvent,
};

type HaStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
pub type HaSink = SplitSink<HaStream, Message>;
pub type HaSource = SplitStream<HaStream>;

// ---------------------------------------------------------------------------
// Static product-knowledge instructions
// ---------------------------------------------------------------------------

/// Static system instructions injected into every help-assistant session.
///
/// Covers the full dashboard feature surface: projects, members, voice notes,
/// AI insights, storyboard, semantic search, call history, credits, and billing.
/// The assistant is instructed to answer ONLY about dashboard features — never
/// to access, reveal, or speculate on user data from calls or transcripts.
const DASHBOARD_HELP_INSTRUCTIONS: &str = "\
You are the VoxTranslate Dashboard Assistant — a friendly, concise product guide \
available to all Business and Enterprise subscribers.\n\
\n\
YOUR SCOPE:\n\
- Answer questions about how to USE the VoxTranslate dashboard features listed below.\n\
- Guide users step-by-step through the interface when they ask how to do something.\n\
- If a user asks about their own call data, transcripts, or account-specific insights, \
  redirect them: 'For questions about your specific call data or AI insights, use the \
  Voice Assistant on the project or member page.'\n\
- NEVER reveal or speculate on transcript content, call recordings, or any user data.\n\
- NEVER answer questions unrelated to the VoxTranslate dashboard product.\n\
\n\
FEATURES YOU CAN EXPLAIN:\n\
\n\
PROJECTS\n\
- Create, name, and describe a project to group related calls and voice notes.\n\
- View the project dashboard: summary, member list, call history.\n\
- Archive or delete a project (owner/admin only).\n\
- Assign calls to a project when creating a room.\n\
\n\
MEMBERS & ROLES\n\
- Invite a new member by email. Set their role: member, admin (team lead), or owner.\n\
- Change a member's role or remove them from the organization.\n\
- Understand role differences: members see their own data; admins see the full org; \
  owners control billing and settings.\n\
\n\
VOICE NOTES\n\
- Record a voice note directly onto a project (no live call needed).\n\
- Transcription is automatic and the note appears in the project timeline.\n\
- Voice notes count against org credits at the standard transcription rate.\n\
\n\
AI INSIGHTS\n\
- Generate an AI insight report for a project or a specific team member.\n\
- Insights summarize patterns across call transcripts: key topics, sentiment trends, \
  recurring issues, and action items.\n\
- Available to admins and owners only; credits are deducted per request.\n\
\n\
STORYBOARD\n\
- An AI-generated visual summary of a project's calls, grouped by theme.\n\
- Storyboards are generated on demand and appear on the project detail page.\n\
\n\
SEMANTIC SEARCH\n\
- Search across all transcript content in the organization or within a specific project.\n\
- Results are ranked by semantic relevance, not keyword match.\n\
- Use natural-language queries: 'When did we discuss pricing?' or 'Show me concerns \
  about the mobile app.'\n\
\n\
CALL HISTORY & TRANSCRIPTS\n\
- View a list of all calls linked to the org, filterable by project, date, or participant.\n\
- Open any call to read the full transcript, listen to the recording (if saved), \
  or export the transcript as PDF or text.\n\
- Correct transcript errors with the AI correction tool on the export page.\n\
\n\
CREDITS & BILLING\n\
- View the org credit balance on the Billing page (owner access).\n\
- Purchase additional credits in the dashboard (one-off top-up).\n\
- Understand what costs credits: live calls (per minute per participant), AI insights \
  (per request), voice notes (transcription time), and the Voice/Help assistants \
  (per minute at 25% markup).\n\
- Manage subscription plan (Business or Enterprise) and see renewal date.\n\
\n\
SETTINGS\n\
- Update org name, logo, and timezone.\n\
- Configure data-retention policy (Enterprise only): set how many days call data is kept.\n\
- Enable or disable cloud recording for the org.\n\
- Manage Google Calendar integration for meeting scheduling.\n\
\n\
COMMUNICATION STYLE:\n\
- Be concise. Speak in 1–3 sentences per answer unless a step-by-step guide is needed.\n\
- Use numbered lists for step-by-step instructions.\n\
- Stay friendly and professional. Avoid jargon.\n\
- If you are unsure about a specific UI detail, say so honestly rather than guessing.\
";

/// Return the static dashboard help instructions.
///
/// This is a pure function — always returns the same value — making it
/// trivially testable and requiring no context arguments.
pub fn build_help_instructions() -> &'static str {
    DASHBOARD_HELP_INSTRUCTIONS
}

// ---------------------------------------------------------------------------
// Session update builder
// ---------------------------------------------------------------------------

/// Build the `session.update` payload for a help-assistant session.
///
/// Uses the static [`build_help_instructions`] prompt; the `voice` parameter
/// is configurable via `HELP_ASSISTANT_VOICE` (env var read by the relay, not here).
pub fn build_ha_session_update_json(voice: &str) -> String {
    serde_json::json!({
        "type": "session.update",
        "session": {
            "type": "realtime",
            "output_modalities": ["audio"],
            "instructions": build_help_instructions(),
            "audio": {
                "input": {
                    "format": { "type": "audio/pcm", "rate": 24000 },
                    "transcription": { "model": "gpt-realtime-whisper" },
                    "turn_detection": { "type": "semantic_vad" }
                },
                "output": {
                    "format": { "type": "audio/pcm", "rate": 24000 },
                    "voice": voice
                }
            }
        }
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

/// Open a help-assistant WebSocket session to the OpenAI Realtime API and return
/// the split sink/source.
///
/// The URL is `wss://api.openai.com/v1/realtime?model={config.model}`.
/// Only `Authorization: Bearer` is sent — the deprecated `OpenAI-Beta` header is
/// intentionally omitted (the GA Realtime API no longer accepts it).
pub async fn open_ha_session(config: &HelpAssistantConfig) -> Result<(HaSink, HaSource), String> {
    use futures::StreamExt as _;

    let url = format!("wss://api.openai.com/v1/realtime?model={}", config.model);
    let mut request = url
        .into_client_request()
        .map_err(|e| format!("invalid help assistant url: {e}"))?;
    let auth = HeaderValue::from_str(&format!("Bearer {}", config.api_key))
        .map_err(|e| format!("invalid help assistant key header: {e}"))?;
    request.headers_mut().insert(AUTHORIZATION, auth);

    let (ws, _resp) = connect_async(request)
        .await
        .map_err(|e| format!("help assistant connect failed: {e}"))?;

    let (sink, source) = ws.split();
    tracing::info!(model = %config.model, "help_assistant: session connected");
    Ok((sink, source))
}

// ---------------------------------------------------------------------------
// Unit tests (run without a live OpenAI connection)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    #[test]
    fn help_instructions_non_empty() {
        assert!(!build_help_instructions().is_empty());
    }

    #[test]
    fn help_instructions_contains_dashboard_features() {
        let instr = build_help_instructions().to_ascii_lowercase();
        for kw in ["project", "member", "voice", "insight", "credit", "search"] {
            assert!(instr.contains(kw), "help instructions must mention '{kw}'");
        }
    }

    #[test]
    fn ha_session_update_structure() {
        let json_str = build_ha_session_update_json("alloy");
        let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(v["type"], "session.update");
        let sess = &v["session"];
        assert_eq!(sess["type"], "realtime");
        assert_eq!(sess["output_modalities"][0], "audio");
        assert_eq!(sess["audio"]["input"]["format"]["type"], "audio/pcm");
        assert_eq!(sess["audio"]["input"]["format"]["rate"], 24000);
        assert_eq!(sess["audio"]["input"]["transcription"]["model"], "gpt-realtime-whisper");
        assert_eq!(sess["audio"]["input"]["turn_detection"]["type"], "semantic_vad");
        assert_eq!(sess["audio"]["output"]["format"]["type"], "audio/pcm");
        assert_eq!(sess["audio"]["output"]["format"]["rate"], 24000);
        assert_eq!(sess["audio"]["output"]["voice"], "alloy");
        let instr = sess["instructions"].as_str().unwrap();
        assert!(!instr.is_empty());
    }

    #[test]
    fn ha_audio_append_round_trips() {
        let pcm: Vec<u8> = vec![5, 10, 15];
        let json_str = ha_audio_append_json(&pcm);
        let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(v["type"], "input_audio_buffer.append");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(v["audio"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, pcm);
    }
}
