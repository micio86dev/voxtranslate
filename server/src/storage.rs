//! Supabase Storage client for chat file upload (spec 0018).
//!
//! We talk to the Storage REST API directly with the secret/service key (server
//! only — it never reaches the browser). One object is created per upload at
//! `{bucket}/{session}/{uuid}.{ext}`. The bucket is **private**, so after
//! uploading we mint a time-limited **signed URL** (`create_signed_url`) that the
//! chat `attachment` carries — only the call participants who receive the
//! broadcast can download, and the link expires after the configured TTL. Bytes
//! are uploaded *through* the server because it must read them anyway to
//! transcribe/extract (see spec §4 Key decisions).

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
    /// Signed-URL lifetime in seconds (how long a chat download link is valid).
    signed_ttl_secs: u64,
}

impl SupabaseStorage {
    /// Build from config, reusing the shared HTTP client.
    pub fn new(http: reqwest::Client, cfg: &StorageConfig) -> Self {
        Self {
            http,
            base_url: cfg.supabase_url.clone(),
            service_key: cfg.service_key.clone(),
            bucket: cfg.bucket.clone(),
            signed_ttl_secs: cfg.signed_ttl_secs,
        }
    }

    /// Upload `bytes` to `object_path` (relative to the bucket). `object_path`
    /// should already be sanitized by [`object_path`].
    pub async fn upload(
        &self,
        object_path: &str,
        bytes: Vec<u8>,
        content_type: &str,
    ) -> Result<(), String> {
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
        Ok(())
    }

    /// Mint a time-limited signed download URL for an already-uploaded object.
    /// Works against a **private** bucket; the link is valid for
    /// `signed_ttl_secs`. Returns the absolute, browser-usable URL.
    pub async fn create_signed_url(&self, object_path: &str) -> Result<String, String> {
        let url = sign_request_url(&self.base_url, &self.bucket, object_path);
        let resp = self
            .http
            .post(&url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.service_key),
            )
            .header("apikey", &self.service_key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .timeout(Duration::from_secs(30))
            .json(&serde_json::json!({ "expiresIn": self.signed_ttl_secs }))
            .send()
            .await
            .map_err(|e| format!("supabase sign request failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let detail = resp.text().await.unwrap_or_default();
            return Err(format!("supabase sign returned {status}: {detail}"));
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("supabase sign parse failed: {e}"))?;
        let signed = body
            .get("signedURL")
            .or_else(|| body.get("signedUrl"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or("supabase sign response had no signedURL")?;
        Ok(signed_full_url(&self.base_url, signed))
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

/// The Storage REST endpoint that mints a signed URL for an object.
pub fn sign_request_url(base_url: &str, bucket: &str, object_path: &str) -> String {
    format!(
        "{}/storage/v1/object/sign/{}/{}",
        base_url.trim_end_matches('/'),
        bucket,
        object_path
    )
}

/// Turn the relative `signedURL` Supabase returns (e.g.
/// `/object/sign/chat-files/a/b.txt?token=…`) into an absolute, browser-usable
/// URL under `{base}/storage/v1`. Tolerant of whether Supabase includes the
/// `/storage/v1` prefix or a leading slash.
pub fn signed_full_url(base_url: &str, signed_path: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if signed_path.starts_with("/storage/v1") {
        format!("{base}{signed_path}")
    } else if let Some(rest) = signed_path.strip_prefix('/') {
        format!("{base}/storage/v1/{rest}")
    } else {
        format!("{base}/storage/v1/{signed_path}")
    }
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
    fn builds_upload_and_sign_urls() {
        let base = "https://ref.supabase.co";
        assert_eq!(
            upload_url(base, "chat-files", "sess/abc.mp3"),
            "https://ref.supabase.co/storage/v1/object/chat-files/sess/abc.mp3"
        );
        assert_eq!(
            sign_request_url(base, "chat-files", "sess/abc.mp3"),
            "https://ref.supabase.co/storage/v1/object/sign/chat-files/sess/abc.mp3"
        );
    }

    #[test]
    fn signed_full_url_assembles_browser_url() {
        let base = "https://ref.supabase.co";
        // The shape Supabase actually returns: relative, no /storage/v1 prefix.
        assert_eq!(
            signed_full_url(base, "/object/sign/chat-files/s/f.txt?token=JWT"),
            "https://ref.supabase.co/storage/v1/object/sign/chat-files/s/f.txt?token=JWT"
        );
        // Already-prefixed and no-leading-slash variants are tolerated.
        assert_eq!(
            signed_full_url(base, "/storage/v1/object/sign/b/p?token=x"),
            "https://ref.supabase.co/storage/v1/object/sign/b/p?token=x"
        );
        assert_eq!(
            signed_full_url("https://ref.supabase.co/", "object/sign/b/p?token=x"),
            "https://ref.supabase.co/storage/v1/object/sign/b/p?token=x"
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
