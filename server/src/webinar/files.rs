//! Webinar chat file upload — `POST /api/w/{code}/files`.
//!
//! Accepts a multipart body with a single `file` field, validates the webinar
//! exists and has `chat_enabled`, enforces file-type and size limits, uploads to
//! Supabase Storage, and returns the attachment metadata the client embeds in its
//! `POST /api/w/{code}/chat` body. No billing, no transcription — just storage + URL.

#![allow(clippy::result_large_err)]

use axum::extract::{Multipart, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use uuid::Uuid;

use crate::business::{db_err, not_found, require_pool};
use crate::files::{classify_ext, content_type_for, ext_of, MAX_BODY_BYTES};
use crate::webinar::find_by_code;
use crate::{observability, storage, AppState};

/// `POST /api/w/{code}/files` — upload a single chat file for a webinar.
pub async fn upload_webinar_file(
    State(state): State<AppState>,
    Path(code): Path<String>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    let Some(storage_client) = state.storage.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "file upload is not configured",
        )
            .into_response();
    };

    let ip = observability::client_ip(&headers);
    if !state.rate_limiter.allow(
        &format!("wv-upload-ip:{ip}"),
        10,
        std::time::Duration::from_secs(60),
    ) {
        return (StatusCode::TOO_MANY_REQUESTS, "too many requests").into_response();
    }

    let pool = match require_pool(&state) {
        Ok(p) => p,
        Err(r) => return r,
    };

    let w = match find_by_code(pool, &code).await {
        Ok(Some(w)) => w,
        Ok(None) => return not_found("webinar not found"),
        Err(e) => return db_err(e),
    };

    // Uploading attaches content to the webinar's chat and consumes its storage —
    // participation on both counts, so a members-only webinar requires a sign-in.
    if let Err(resp) = crate::webinar::require_member_access(&w, &state, &headers) {
        return resp;
    }

    if !w.chat_enabled {
        return (
            StatusCode::FORBIDDEN,
            "chat is not enabled for this webinar",
        )
            .into_response();
    }

    if !state.rate_limiter.allow(
        &format!("wv-upload:{}", w.id),
        20,
        std::time::Duration::from_secs(60),
    ) {
        return (StatusCode::TOO_MANY_REQUESTS, "too many requests").into_response();
    }

    let max_bytes = state
        .config
        .storage
        .as_ref()
        .map(|c| c.max_bytes)
        .unwrap_or(MAX_BODY_BYTES);
    let mut file_name = String::new();
    let mut bytes: Vec<u8> = Vec::new();
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                tracing::warn!("webinar file upload multipart error: {e}");
                return (StatusCode::BAD_REQUEST, "invalid multipart body").into_response();
            }
        };
        let name = field.name().unwrap_or("").to_string();
        if name == "file" {
            file_name = field.file_name().unwrap_or("upload").to_string();
            bytes = match field.bytes().await {
                Ok(b) => b.to_vec(),
                Err(e) => {
                    tracing::warn!("webinar file upload read error: {e}");
                    return (StatusCode::BAD_REQUEST, "failed to read file bytes").into_response();
                }
            };
        }
    }

    if bytes.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty or missing file").into_response();
    }
    if bytes.len() > max_bytes {
        return (StatusCode::PAYLOAD_TOO_LARGE, "file too large").into_response();
    }

    let ext = ext_of(&file_name);
    let Some(_kind) = classify_ext(&ext) else {
        return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported file type").into_response();
    };
    let content_type = content_type_for(&ext);
    let size = bytes.len() as i64;

    let object = storage::object_path(&format!("wv-{code}"), &Uuid::new_v4().to_string(), &ext);
    if let Err(e) = storage_client.upload(&object, bytes, content_type).await {
        tracing::error!("webinar chat file upload failed: {e}");
        return (StatusCode::BAD_GATEWAY, "storage upload failed").into_response();
    }

    let file_url = match storage_client.create_signed_url(&object).await {
        Ok(url) => url,
        Err(e) => {
            tracing::error!("webinar chat file sign-url failed: {e}");
            return (StatusCode::BAD_GATEWAY, "storage sign failed").into_response();
        }
    };

    Json(serde_json::json!({
        "url": file_url,
        "name": file_name,
        "content_type": content_type,
        "size": size,
    }))
    .into_response()
}
