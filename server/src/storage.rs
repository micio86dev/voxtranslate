//! Supabase Storage client for chat file upload (spec 0018).
//!
//! We talk to the Storage REST API directly with the service-role key (server
//! only — it never reaches the browser). One object is created per upload at
//! `{bucket}/{session}/{uuid}.{ext}`; the returned public URL is what the chat
//! `attachment` carries. Bytes are uploaded *through* the server because it must
//! read them anyway to transcribe/extract (see spec §4 Key decisions).

use std::time::Duration;

use crate::config::StorageConfig;

/// A configured Supabase Storage uploader. Cheap to clone (shares the pooled
/// HTTP client).
#[derive(Clone)]
pub struct SupabaseStorage {
    http: reqwest::Client,
    base_url: String,
    service_key: String,
    bucket: String,
}

impl SupabaseStorage {
    /// Build from config, reusing the shared HTTP client.
    pub fn new(http: reqwest::Client, cfg: &StorageConfig) -> Self {
        Self {
            http,
            base_url: cfg.supabase_url.clone(),
            service_key: cfg.service_key.clone(),
            bucket: cfg.bucket.clone(),
        }
    }

    /// Upload `bytes` to `object_path` (relative to the bucket) and return the
    /// public URL. `object_path` should already be sanitized by [`object_path`].
    pub async fn upload(
        &self,
        object_path: &str,
        bytes: Vec<u8>,
        content_type: &str,
    ) -> Result<String, String> {
        let url = upload_url(&self.base_url, &self.bucket, object_path);
        let resp = self
            .http
            .post(&url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.service_key),
            )
            // The service key doubles as the apikey on Supabase Storage.
            .header("apikey", &self.service_key)
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .header(reqwest::header::CACHE_CONTROL, "max-age=3600")
            // Idempotent on the (uuid-keyed) path; avoids 409 on rare retries.
            .header("x-upsert", "true")
            .timeout(Duration::from_secs(60))
            .body(bytes)
            .send()
            .await
            .map_err(|e| format!("supabase upload request failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let detail = resp.text().await.unwrap_or_default();
            return Err(format!("supabase upload returned {status}: {detail}"));
        }
        Ok(public_url(&self.base_url, &self.bucket, object_path))
    }
}

/// The Storage REST endpoint that accepts an object's bytes (POST/PUT).
pub fn upload_url(base_url: &str, bucket: &str, object_path: &str) -> String {
    format!(
        "{}/storage/v1/object/{}/{}",
        base_url.trim_end_matches('/'),
        bucket,
        object_path
    )
}

/// The public read URL for an object (requires the bucket to be public).
pub fn public_url(base_url: &str, bucket: &str, object_path: &str) -> String {
    format!(
        "{}/storage/v1/object/public/{}/{}",
        base_url.trim_end_matches('/'),
        bucket,
        object_path
    )
}

/// Build a safe, collision-free object path: `{session}/{uuid}.{ext}`. The file
/// name from the client is NOT used in the path (it can be hostile); only a
/// validated extension is appended. `ext` is lowercased and stripped to
/// `[a-z0-9]`; empty/oversized extensions are dropped.
pub fn object_path(session: &str, file_id: &str, ext: &str) -> String {
    let session = sanitize_segment(session);
    let file_id = sanitize_segment(file_id);
    let ext: String = ext
        .trim_start_matches('.')
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    if ext.is_empty() {
        format!("{session}/{file_id}")
    } else {
        format!("{session}/{file_id}.{ext}")
    }
}

/// Keep only path-safe characters for a single path segment.
fn sanitize_segment(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    if cleaned.is_empty() {
        "x".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_upload_and_public_urls() {
        let base = "https://ref.supabase.co";
        assert_eq!(
            upload_url(base, "chat-files", "sess/abc.mp3"),
            "https://ref.supabase.co/storage/v1/object/chat-files/sess/abc.mp3"
        );
        assert_eq!(
            public_url(base, "chat-files", "sess/abc.mp3"),
            "https://ref.supabase.co/storage/v1/object/public/chat-files/sess/abc.mp3"
        );
    }

    #[test]
    fn trailing_slash_is_tolerated() {
        let base = "https://ref.supabase.co/";
        assert_eq!(
            upload_url(base, "b", "p"),
            "https://ref.supabase.co/storage/v1/object/b/p"
        );
    }

    #[test]
    fn object_path_sanitizes_and_keys_by_session_and_id() {
        assert_eq!(object_path("sess-1", "uuid-2", "MP3"), "sess-1/uuid-2.mp3");
        // Leading dot + mixed case + junk chars in ext are normalized.
        assert_eq!(object_path("s", "i", ".PdF"), "s/i.pdf");
        // Path-traversal / hostile segments are stripped to safe chars.
        let p = object_path("../../etc", "a/b", "wav");
        assert_eq!(p, "etc/ab.wav");
        assert!(!p.contains(".."));
        // Empty/garbage extension is dropped, not left as a bare dot.
        assert_eq!(object_path("s", "i", "!!"), "s/i");
        assert_eq!(object_path("s", "i", ""), "s/i");
    }

    #[test]
    fn empty_segments_get_a_placeholder() {
        assert_eq!(object_path("", "", "txt"), "x/x.txt");
    }
}
