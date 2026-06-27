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

    /// Build an uploader for an arbitrary bucket/TTL on the same Supabase project,
    /// reusing the credentials from a [`StorageConfig`]. Used for the private
    /// `recordings` bucket (spec 0106), which has a different name + signed-URL TTL
    /// than chat files.
    pub fn for_bucket(
        http: reqwest::Client,
        cfg: &StorageConfig,
        bucket: String,
        signed_ttl_secs: u64,
    ) -> Self {
        Self {
            http,
            base_url: cfg.supabase_url.clone(),
            service_key: cfg.service_key.clone(),
            bucket,
            signed_ttl_secs,
        }
    }

    /// Download an object's bytes (server-side, service key). Used internally to
    /// feed a recording to transcription — never exposed to the browser.
    pub async fn download(&self, object_path: &str) -> Result<Vec<u8>, String> {
        let url = upload_url(&self.base_url, &self.bucket, object_path);
        let resp = self
            .http
            .get(&url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.service_key),
            )
            .header("apikey", &self.service_key)
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .map_err(|e| format!("supabase download request failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let detail = resp.text().await.unwrap_or_default();
            return Err(format!("supabase download returned {status}: {detail}"));
        }
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| format!("supabase download read failed: {e}"))
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

    /// Permanently delete an object (server-side, service key). Used by the
    /// Enterprise data-retention sweep (spec 0106). A 404 counts as success —
    /// the goal is "the object is gone", and a re-run over an already-cleared
    /// session must not error.
    pub async fn delete(&self, object_path: &str) -> Result<(), String> {
        let url = upload_url(&self.base_url, &self.bucket, object_path);
        let resp = self
            .http
            .delete(&url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.service_key),
            )
            .header("apikey", &self.service_key)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| format!("supabase delete request failed: {e}"))?;
        if resp.status().is_success() || resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        let status = resp.status();
        let detail = resp.text().await.unwrap_or_default();
        Err(format!("supabase delete returned {status}: {detail}"))
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

    /// Mint a one-shot **signed upload** URL so the browser can PUT an object
    /// straight to the private bucket — the server never proxies the bytes (spec
    /// 0106: recordings are large call videos). The returned absolute URL embeds a
    /// short-lived token; the client PUTs the file to it with `x-upsert: true`.
    pub async fn create_signed_upload_url(&self, object_path: &str) -> Result<String, String> {
        let url = sign_upload_request_url(&self.base_url, &self.bucket, object_path);
        let resp = self
            .http
            .post(&url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.service_key),
            )
            .header("apikey", &self.service_key)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| format!("supabase sign-upload request failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let detail = resp.text().await.unwrap_or_default();
            return Err(format!("supabase sign-upload returned {status}: {detail}"));
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("supabase sign-upload parse failed: {e}"))?;
        let signed = body
            .get("url")
            .or_else(|| body.get("signedUrl"))
            .or_else(|| body.get("signedURL"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or("supabase sign-upload response had no url")?;
        Ok(signed_full_url(&self.base_url, signed))
    }
}

/// The Storage REST endpoint that mints a signed UPLOAD URL for an object.
pub fn sign_upload_request_url(base_url: &str, bucket: &str, object_path: &str) -> String {
    format!(
        "{}/storage/v1/object/upload/sign/{}/{}",
        base_url.trim_end_matches('/'),
        bucket,
        object_path
    )
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

    // ---- HTTP method tests against a raw mock Supabase Storage endpoint --------
    // A tiny TCP listener replies with a fixed HTTP/1.1 message and closes the socket,
    // so reqwest reads the body to EOF (no Content-Length bookkeeping). Each test points
    // a SupabaseStorage at its own mock, exercising the success AND error branches of the
    // upload/download/delete/sign methods without any real Supabase.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn cfg(base: &str) -> crate::config::StorageConfig {
        crate::config::StorageConfig {
            supabase_url: base.to_string(),
            service_key: "svc-key".into(),
            bucket: "recordings".into(),
            max_bytes: 25 * 1024 * 1024,
            signed_ttl_secs: 60,
        }
    }

    async fn spawn_mock(response: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = sock.read(&mut buf).await; // consume the request
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.flush().await;
                    // socket drops here → connection close delimits the body
                });
            }
        });
        format!("http://{addr}")
    }

    fn storage(base: &str) -> SupabaseStorage {
        SupabaseStorage::new(reqwest::Client::new(), &cfg(base))
    }

    const OK_EMPTY: &str = "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n";
    const ERR_500: &str = "HTTP/1.1 500 Internal Server Error\r\nConnection: close\r\n\r\nboom";
    const NOT_FOUND: &str = "HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n";

    #[tokio::test]
    async fn upload_ok_and_error() {
        let ok = spawn_mock(OK_EMPTY).await;
        assert!(storage(&ok).upload("s/f.webm", vec![1, 2, 3], "audio/webm").await.is_ok());
        let bad = spawn_mock(ERR_500).await;
        let e = storage(&bad).upload("s/f.webm", vec![1], "audio/webm").await.unwrap_err();
        assert!(e.contains("500"), "{e}");
    }

    #[tokio::test]
    async fn download_ok_and_error() {
        let ok = spawn_mock("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nfilebytes").await;
        assert_eq!(storage(&ok).download("s/f.webm").await.unwrap(), b"filebytes");
        let bad = spawn_mock(NOT_FOUND).await;
        assert!(storage(&bad).download("s/f.webm").await.is_err());
    }

    #[tokio::test]
    async fn delete_ok_404_is_ok_and_error() {
        let ok = spawn_mock(OK_EMPTY).await;
        assert!(storage(&ok).delete("s/f.webm").await.is_ok());
        // A 404 means "already gone" → success (idempotent retention sweep).
        let gone = spawn_mock(NOT_FOUND).await;
        assert!(storage(&gone).delete("s/f.webm").await.is_ok());
        let bad = spawn_mock(ERR_500).await;
        assert!(storage(&bad).delete("s/f.webm").await.is_err());
    }

    #[tokio::test]
    async fn create_signed_url_ok_error_and_missing_field() {
        let ok = spawn_mock(
            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{\"signedURL\":\"/object/sign/recordings/p?token=abc\"}",
        )
        .await;
        let url = storage(&ok).create_signed_url("s/f.webm").await.unwrap();
        assert!(url.contains("token=abc") && url.contains("/storage/v1/"), "{url}");
        let bad = spawn_mock(ERR_500).await;
        assert!(storage(&bad).create_signed_url("s/f.webm").await.is_err());
        let empty = spawn_mock("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{}").await;
        let e = storage(&empty).create_signed_url("s/f.webm").await.unwrap_err();
        assert!(e.contains("signedURL"), "{e}");
    }

    #[tokio::test]
    async fn create_signed_upload_url_ok_and_error() {
        let ok = spawn_mock(
            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{\"url\":\"/object/upload/sign/recordings/p?token=xyz\"}",
        )
        .await;
        let url = storage(&ok).create_signed_upload_url("s/f.webm").await.unwrap();
        assert!(url.contains("token=xyz"), "{url}");
        let bad = spawn_mock(ERR_500).await;
        assert!(storage(&bad).create_signed_upload_url("s/f.webm").await.is_err());
    }
}
