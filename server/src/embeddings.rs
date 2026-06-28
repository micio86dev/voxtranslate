//! OpenAI embeddings REST client for semantic transcript search. Mirrors the
//! [`groq`](crate::groq) client's shape: a cloneable struct over a pooled
//! `reqwest::Client`, bearer auth, and retry-on-429/5xx with exponential backoff.
//! Used to embed transcript chunks at ingest (`business::transcripts`) and the
//! user's query at search time (`business::search`).

use serde::Deserialize;
use std::time::Duration;

const OPENAI_EMBEDDINGS_URL: &str = "https://api.openai.com/v1/embeddings";

/// Cloneable embeddings client. Wraps a pooled `reqwest::Client` (keep-alive) and
/// the API key + model id.
#[derive(Clone)]
pub struct OpenAiEmbeddings {
    http: reqwest::Client,
    api_key: String,
    model: String,
}

#[derive(Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
    index: usize,
    embedding: Vec<f32>,
}

impl OpenAiEmbeddings {
    pub fn new(api_key: String, model: String) -> Self {
        let http = reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(90))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build reqwest client");
        Self {
            http,
            api_key,
            model,
        }
    }

    /// Build from the parsed [`EmbeddingsConfig`](crate::config::EmbeddingsConfig).
    pub fn from_config(cfg: &crate::config::EmbeddingsConfig) -> Self {
        Self::new(cfg.api_key.clone(), cfg.model.clone())
    }

    /// Embed a single string. Returns the one vector, or an error.
    pub async fn embed_one(&self, text: &str) -> Result<Vec<f32>, String> {
        let inputs = [text.to_string()];
        let mut out = self.embed_batch(&inputs).await?;
        out.pop()
            .ok_or_else(|| "empty embeddings response".to_string())
    }

    /// Embed a batch of strings; returns one vector per input, **in input order**.
    /// Retries up to 3 times on 429 / 5xx / transport errors with exponential
    /// backoff (400ms, 800ms), mirroring [`Groq::chat`](crate::groq::Groq::chat).
    pub async fn embed_batch(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, String> {
        if inputs.is_empty() {
            return Ok(vec![]);
        }
        let body = serde_json::json!({ "model": self.model, "input": inputs });
        const MAX_ATTEMPTS: u32 = 3;
        let mut last_err = String::new();
        for attempt in 0..MAX_ATTEMPTS {
            let send = self
                .http
                .post(OPENAI_EMBEDDINGS_URL)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await;

            let resp = match send {
                Ok(r) => r,
                // Timeout / connection reset: back off and retry like a 429.
                Err(e) => {
                    last_err = format!("embeddings request failed: {e}");
                    if attempt + 1 < MAX_ATTEMPTS {
                        tokio::time::sleep(Duration::from_millis(400 << attempt)).await;
                        continue;
                    }
                    return Err(last_err);
                }
            };

            let status = resp.status();
            if (status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
                && attempt + 1 < MAX_ATTEMPTS
            {
                last_err = format!("embeddings returned {status}");
                tokio::time::sleep(Duration::from_millis(400 << attempt)).await;
                continue;
            }
            if !status.is_success() {
                let detail = resp.text().await.unwrap_or_default();
                return Err(format!("embeddings returned {status}: {detail}"));
            }

            let mut parsed: EmbeddingsResponse = resp
                .json()
                .await
                .map_err(|e| format!("embeddings parse failed: {e}"))?;
            if parsed.data.len() != inputs.len() {
                return Err(format!(
                    "embeddings count mismatch: got {}, want {}",
                    parsed.data.len(),
                    inputs.len()
                ));
            }
            // OpenAI may return rows out of order; sort by `index` to realign.
            parsed.data.sort_by_key(|d| d.index);
            return Ok(parsed.data.into_iter().map(|d| d.embedding).collect());
        }

        Err(if last_err.is_empty() {
            "embeddings exhausted retries".to_string()
        } else {
            last_err
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_reorders_by_index() {
        // OpenAI returns `data` with an explicit `index`; we must realign to input order.
        let raw = r#"{"data":[
            {"index":1,"embedding":[0.4,0.5,0.6]},
            {"index":0,"embedding":[0.1,0.2,0.3]}
        ]}"#;
        let mut parsed: EmbeddingsResponse = serde_json::from_str(raw).unwrap();
        parsed.data.sort_by_key(|d| d.index);
        let vecs: Vec<Vec<f32>> = parsed.data.into_iter().map(|d| d.embedding).collect();
        assert_eq!(vecs[0], vec![0.1, 0.2, 0.3]);
        assert_eq!(vecs[1], vec![0.4, 0.5, 0.6]);
    }

    #[tokio::test]
    async fn embed_batch_empty_is_noop() {
        let e = OpenAiEmbeddings::new("k".into(), "text-embedding-3-small".into());
        assert_eq!(e.embed_batch(&[]).await.unwrap(), Vec::<Vec<f32>>::new());
    }
}
