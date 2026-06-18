//! Groq chat-completion client. `translate()` powers the real-time pipeline;
//! the generic `chat()` / `chat_json()` helpers power the AI features (report,
//! sentiment, email draft, live suggestions). Non-streaming throughout.

use serde::Deserialize;
use std::time::Duration;

const GROQ_URL: &str = "https://api.groq.com/openai/v1/chat/completions";

/// One chat-completion call. Build with [`ChatRequest::new`] then override
/// fields as needed; the defaults match the real-time translation profile.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub system: String,
    pub user: String,
    /// When set, asks for `response_format: json_object` (the prompt must also
    /// mention JSON — a Groq requirement).
    pub json_mode: bool,
    pub max_tokens: u32,
    pub temperature: f32,
    /// Per-request override of the client-wide 15s timeout.
    pub timeout: Duration,
    /// Extra attempts on HTTP 429, with exponential backoff (400ms, 800ms, …).
    pub max_retries: u8,
}

impl ChatRequest {
    pub fn new(
        model: impl Into<String>,
        system: impl Into<String>,
        user: impl Into<String>,
    ) -> Self {
        Self {
            model: model.into(),
            system: system.into(),
            user: user.into(),
            json_mode: false,
            max_tokens: 256,
            temperature: 0.2,
            timeout: Duration::from_secs(15),
            max_retries: 1,
        }
    }

    /// Serialize the request body. Separated from the HTTP call for testing.
    fn body(&self) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": self.system },
                { "role": "user", "content": self.user },
            ],
            "temperature": self.temperature,
            "max_tokens": self.max_tokens,
        });
        if self.json_mode {
            body["response_format"] = serde_json::json!({ "type": "json_object" });
        }
        body
    }
}

/// Cloneable translation client. Wraps a pooled `reqwest::Client` (keep-alive)
/// and the API key.
#[derive(Clone)]
pub struct Groq {
    http: reqwest::Client,
    api_key: String,
    /// Real-time translation model id, env-driven via `GROQ_TRANSLATION_MODEL`
    /// (resolved in `AiConfig`). A Groq decommission becomes a config change, not
    /// a deploy. Latency-critical — keep it a fast/cheap model.
    translation_model: String,
}

impl Groq {
    pub fn new(api_key: String, translation_model: String) -> Self {
        let http = reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(90))
            .timeout(Duration::from_secs(15))
            .build()
            .expect("failed to build reqwest client");
        Self {
            http,
            api_key,
            translation_model,
        }
    }

    /// Translate `text` from `source` to `target` (language codes like "it", "en").
    /// `terms` are room-glossary pairs (already filtered to this direction) that
    /// MUST be translated verbatim. Returns the translation only (no
    /// quotes/preamble). Retries once on HTTP 429.
    pub async fn translate(
        &self,
        text: &str,
        source: &str,
        target: &str,
        terms: &[(String, String)],
    ) -> Result<String, String> {
        let system = translation_prompt(source, target, terms);
        self.chat(ChatRequest::new(
            self.translation_model.as_str(),
            system,
            text,
        ))
        .await
    }

    /// Run one chat completion; returns the assistant message content.
    /// Retries on 429 up to `max_retries` times with exponential backoff.
    pub async fn chat(&self, req: ChatRequest) -> Result<String, String> {
        let body = req.body();
        let attempts = u32::from(req.max_retries) + 1;
        for attempt in 0..attempts {
            let resp = self
                .http
                .post(GROQ_URL)
                .bearer_auth(&self.api_key)
                .timeout(req.timeout)
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("groq request failed: {e}"))?;

            if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt + 1 < attempts {
                tokio::time::sleep(Duration::from_millis(400 << attempt)).await;
                continue;
            }

            if !resp.status().is_success() {
                let status = resp.status();
                let detail = resp.text().await.unwrap_or_default();
                return Err(format!("groq returned {status}: {detail}"));
            }

            let parsed: ChatResponse = resp
                .json()
                .await
                .map_err(|e| format!("groq response parse failed: {e}"))?;

            return parsed
                .choices
                .into_iter()
                .next()
                .map(|c| c.message.content.trim().to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "groq returned empty completion".to_string());
        }

        Err("groq rate limited after retry".to_string())
    }

    /// Like [`chat`](Self::chat) but forces JSON mode and parses the result.
    /// On malformed JSON, re-asks once with a stricter instruction, then errors.
    pub async fn chat_json(&self, mut req: ChatRequest) -> Result<serde_json::Value, String> {
        req.json_mode = true;
        let content = self.chat(req.clone()).await?;
        match serde_json::from_str(&content) {
            Ok(v) => Ok(v),
            Err(_) => {
                req.system.push_str(
                    "\n\nIMPORTANT: Respond with a single VALID JSON object only. \
                     No markdown fences, no preamble, no trailing text.",
                );
                let retry = self.chat(req).await?;
                serde_json::from_str(&retry)
                    .map_err(|e| format!("groq returned malformed JSON after retry: {e}"))
            }
        }
    }
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChatMessage,
}

#[derive(Deserialize)]
struct ChatMessage {
    content: String,
}

/// Build the translation system prompt, with a glossary block when the room
/// has terms for this language direction. Separated from the HTTP call for
/// testing.
fn translation_prompt(source: &str, target: &str, terms: &[(String, String)]) -> String {
    // "auto" = detection still pending (spec 0012): chat can arrive before the
    // speech probe resolves, so let the model identify the source itself.
    let src_clause = if source == "auto" {
        "Detect the source language yourself, then translate".to_string()
    } else {
        format!("Translate from {}", lang_name(source))
    };
    let mut system = format!(
        "You are a real-time speech translator. {src_clause} to {tgt}. \
         Output ONLY the translation. No quotes, no explanation, no preamble. \
         Preserve tone, register, and speech patterns. Handle informal/spoken \
         language naturally. If text is already in target language, return it unchanged.",
        tgt = lang_name(target),
    );
    if !terms.is_empty() {
        system.push_str(
            "\n\nMANDATORY TERMINOLOGY: whenever a term on the left appears \
             (any capitalization), use the exact translation on the right:\n",
        );
        for (src_term, tgt_term) in terms {
            system.push_str(&format!("\"{src_term}\" -> \"{tgt_term}\"\n"));
        }
    }
    system
}

/// Map a short language code to a human-readable name for the prompt. Unknown
/// codes pass through unchanged so the model still gets a usable hint.
pub fn lang_name(code: &str) -> &str {
    match code {
        "it" => "Italian",
        "en" => "English",
        "es" => "Spanish",
        "fr" => "French",
        "de" => "German",
        "pt" => "Portuguese",
        "ja" => "Japanese",
        "zh" => "Chinese",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_name_maps_known_and_passes_through() {
        assert_eq!(lang_name("it"), "Italian");
        assert_eq!(lang_name("en"), "English");
        assert_eq!(lang_name("zh"), "Chinese");
        assert_eq!(lang_name("ja"), "Japanese");
        assert_eq!(lang_name("xx"), "xx");
    }

    #[test]
    fn translation_prompt_auto_source_asks_model_to_detect() {
        let auto = translation_prompt("auto", "en", &[]);
        assert!(auto.contains("Detect the source language yourself"));
        assert!(auto.contains("English"));
        assert!(!auto.contains("Translate from"));
    }

    #[test]
    fn translation_prompt_glossary_block_only_when_terms_exist() {
        let bare = translation_prompt("it", "en", &[]);
        assert!(bare.contains("Italian") && bare.contains("English"));
        assert!(!bare.contains("MANDATORY TERMINOLOGY"));

        let terms = vec![
            ("fattura".to_string(), "invoice".to_string()),
            ("preventivo".to_string(), "quote".to_string()),
        ];
        let with = translation_prompt("it", "en", &terms);
        assert!(with.contains("MANDATORY TERMINOLOGY"));
        assert!(with.contains("\"fattura\" -> \"invoice\""));
        assert!(with.contains("\"preventivo\" -> \"quote\""));
    }

    #[test]
    fn chat_request_body_shape() {
        let req = ChatRequest::new("m", "sys", "usr");
        let body = req.body();
        assert_eq!(body["model"], "m");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "sys");
        assert_eq!(body["messages"][1]["content"], "usr");
        // JSON mode is opt-in: response_format only appears when requested.
        assert!(body.get("response_format").is_none());

        let mut json_req = ChatRequest::new("m", "sys", "usr");
        json_req.json_mode = true;
        assert_eq!(json_req.body()["response_format"]["type"], "json_object");
    }
}

#[cfg(test)]
mod error_tests {
    use super::*;

    #[tokio::test]
    async fn translate_errors_on_bad_key() {
        // A bad key makes Groq return a non-success status -> Err (covers the
        // error-handling branch).
        let g = Groq::new("bad-key-xyz".into(), "openai/gpt-oss-20b".into());
        assert!(g.translate("ciao", "it", "en", &[]).await.is_err());
    }
}
