//! Internal benchmark endpoint (spec 0107): `POST /internal/bench/translate`,
//! guarded by `BENCH_SECRET`. It drives the SAME cached translation path as the
//! live Standard-tier fan-out ([`crate::translator::Translator::translate_one`]),
//! so a benchmark run measures real production latency and reports whether each
//! call was a cache HIT. The endpoint is intentionally absent from the public
//! API / OpenAPI docs.
//!
//! Guard semantics (spec 0107 R9): `404` when `BENCH_SECRET` is unset (the
//! endpoint should look like it doesn't exist), `401` on a missing/wrong bearer
//! token. The secret is never logged.

use std::time::Instant;

use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{header::AUTHORIZATION, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::AppState;

/// Extractor authenticating a benchmark request by the `BENCH_SECRET` bearer
/// token. As a `FromRequestParts` extractor it runs BEFORE the JSON body is
/// parsed, so an unauthorized caller never reaches the handler (and the body is
/// never deserialized). Returns `404` when no secret is configured — the endpoint
/// is meant to be invisible then — and `401` on a missing/wrong token.
pub struct BenchAuth;

impl FromRequestParts<AppState> for BenchAuth {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // No secret configured → behave as if the route doesn't exist (R9).
        let Some(secret) = state.config.bench_secret.as_deref() else {
            return Err(StatusCode::NOT_FOUND.into_response());
        };
        let bearer = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if constant_eq(bearer, secret) {
            Ok(BenchAuth)
        } else {
            Err(StatusCode::UNAUTHORIZED.into_response())
        }
    }
}

/// Length-checked, branch-light comparison so a wrong secret doesn't leak its
/// length via timing — the same approach as `admin::constant_eq` (kept local so
/// the two guards stay independent).
fn constant_eq(given: Option<&str>, secret: &str) -> bool {
    let Some(given) = given else { return false };
    if given.len() != secret.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in given.bytes().zip(secret.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Request body for `POST /internal/bench/translate`. `glossary` is optional —
/// omit it for a no-glossary baseline run; supply a raw term list to exercise the
/// glossary-isolated key path (spec 0107 §8).
#[derive(Deserialize)]
pub struct BenchRequest {
    pub text: String,
    pub src: String,
    pub tgt: String,
    #[serde(default)]
    pub glossary: Option<Vec<String>>,
}

/// Response: the translation, the wall-clock latency of the cached path in
/// milliseconds, and whether it was served from cache.
#[derive(Serialize)]
pub struct BenchResponse {
    pub translation: String,
    pub latency_ms: u128,
    pub cached: bool,
}

/// `POST /internal/bench/translate` — translate one phrase through the live
/// cached path and report `{ translation, latency_ms, cached }`. Honors
/// `TRANSLATION_CACHE_ENABLED` exactly like production, since the cache handle is
/// the same one folded into the live `Translator`. `BenchAuth` (a
/// `FromRequestParts` extractor) is listed first so the guard runs before the
/// body is deserialized.
pub async fn translate(
    _auth: BenchAuth,
    State(state): State<AppState>,
    Json(req): Json<BenchRequest>,
) -> Response {
    let glossary = req.glossary.as_deref();
    let started = Instant::now();
    match state
        .translator
        .translate_one(&req.text, &req.src, &req.tgt, glossary)
        .await
    {
        Ok((translation, cached)) => {
            let latency_ms = started.elapsed().as_millis();
            Json(BenchResponse {
                translation,
                latency_ms,
                cached,
            })
            .into_response()
        }
        Err(e) => {
            // Never echo upstream error detail to the caller; log it server-side.
            tracing::warn!("bench translate failed: {e}");
            (StatusCode::BAD_GATEWAY, "translation failed").into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::constant_eq;

    #[test]
    fn constant_eq_matches_only_exact() {
        assert!(constant_eq(Some("s3cret"), "s3cret"));
        assert!(!constant_eq(Some("s3cret"), "s3creT"));
        assert!(!constant_eq(Some("short"), "longer-secret"));
        assert!(!constant_eq(None, "s3cret"));
    }
}
