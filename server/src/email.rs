//! Resend transactional-email client (spec 0016). Raw reqwest against
//! `https://api.resend.com/emails` — no SDK crate. Mirrors the `Groq` client
//! shape: cloneable, pooled HTTP, string errors.

use std::time::Duration;

use crate::config::ResendConfig;

const RESEND_URL: &str = "https://api.resend.com/emails";

/// One outbound email, fully resolved — recipient refs are expanded to
/// addresses in the API layer (server-side only) before this is built.
#[derive(Debug)]
pub struct OutboundEmail {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub html: String,
    pub text: String,
}

#[derive(Clone)]
pub struct Resend {
    http: reqwest::Client,
    api_key: String,
    /// `Name <email>` per RFC 5322, prebuilt from the config.
    from: String,
    /// Where mail is posted. Defaults to [`RESEND_URL`]; a different value comes
    /// only from `RESEND_BASE_URL`, which no deployment sets.
    base_url: String,
}

impl Resend {
    pub fn new(http: reqwest::Client, cfg: &ResendConfig) -> Self {
        Self::with_endpoint(http, cfg, RESEND_URL.to_string())
    }

    /// Same client, posting to `base_url` instead of Resend.
    ///
    /// Every outbound email in this server — bug reports, the contact form, room
    /// invites, call and webinar recaps, notification mail — goes through one
    /// `send()`, so without a seam here none of them can be exercised without a
    /// live key. Plumbed from `RESEND_BASE_URL`
    /// (see [`crate::config::Config::resend_base_url`]), unset everywhere but a
    /// test, so production addresses Resend exactly as before.
    pub fn with_endpoint(http: reqwest::Client, cfg: &ResendConfig, base_url: String) -> Self {
        Self {
            http,
            api_key: cfg.api_key.clone(),
            from: format!("{} <{}>", cfg.from_name, cfg.from_email),
            base_url,
        }
    }

    /// Serialize the API body. Separated from the HTTP call for testing.
    fn body(&self, email: &OutboundEmail) -> serde_json::Value {
        let mut body = serde_json::json!({
            "from": self.from,
            "to": email.to,
            "subject": email.subject,
            "html": email.html,
            "text": email.text,
        });
        if !email.cc.is_empty() {
            body["cc"] = serde_json::json!(email.cc);
        }
        body
    }

    /// Send the email; returns Resend's message id.
    pub async fn send(&self, email: &OutboundEmail) -> Result<String, String> {
        let resp = self
            .http
            .post(&self.base_url)
            .bearer_auth(&self.api_key)
            .timeout(Duration::from_secs(15))
            .json(&self.body(email))
            .send()
            .await
            .map_err(|e| format!("resend request failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let detail = resp.text().await.unwrap_or_default();
            return Err(format!("resend returned {status}: {detail}"));
        }
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("resend response parse failed: {e}"))?;
        v["id"]
            .as_str()
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "resend returned no message id".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> Resend {
        Resend::new(
            reqwest::Client::new(),
            &ResendConfig {
                api_key: "re_test".into(),
                from_email: "noreply@vox.example".into(),
                from_name: "VoxTranslate".into(),
            },
        )
    }

    #[test]
    fn body_formats_from_and_omits_empty_cc() {
        let email = OutboundEmail {
            to: vec!["a@x.com".into()],
            cc: vec![],
            subject: "Recap".into(),
            html: "<p>hi</p>".into(),
            text: "hi".into(),
        };
        let body = client().body(&email);
        assert_eq!(body["from"], "VoxTranslate <noreply@vox.example>");
        assert_eq!(body["to"][0], "a@x.com");
        assert!(body.get("cc").is_none(), "cc omitted when empty: {body}");

        let with_cc = OutboundEmail {
            cc: vec!["b@x.com".into()],
            ..email
        };
        let body = client().body(&with_cc);
        assert_eq!(body["cc"][0], "b@x.com");
    }
}
