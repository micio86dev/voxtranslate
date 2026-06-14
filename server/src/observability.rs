//! Observability (spec 0050): structured logging, request-id propagation, and a
//! **canonical log line** per request (one wide, structured event summarising the
//! whole request — method, path, status, latency, request id, client ip). Every
//! log emitted while handling a request inherits the `request_id` via a span, so a
//! line and its errors are traceable end-to-end.
//!
//! Set `LOG_FORMAT=json` (e.g. on Railway) for machine-parseable JSON logs that a
//! log tool can index/query; unset → human-readable for local dev.

use std::time::Instant;

use axum::{
    extract::Request,
    http::{HeaderMap, HeaderName, HeaderValue},
    middleware::Next,
    response::Response,
};
use tracing::Instrument;

/// Initialise the global tracing subscriber. JSON when `LOG_FORMAT=json`, else the
/// pretty formatter. Span fields (incl. `request_id`) are flattened into events so
/// every line carries them. When `BETTERSTACK_SOURCE_TOKEN` is set, an additional
/// layer ships the same lines to Better Stack Logs (spec 0063); it's `None` otherwise,
/// so the default path is unchanged.
pub fn init_tracing() {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    use tracing_subscriber::Layer as _;

    // `canonical=info` enables our canonical log lines (they use target "canonical",
    // which wouldn't match the `voxtranslate_server` target filter otherwise).
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "voxtranslate_server=info,canonical=info,tower_http=warn".into());
    let json = std::env::var("LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);

    // Stdout formatter: JSON or pretty. Boxed so both branches share one type and can be
    // layered alongside the optional Better Stack shipping layer.
    let stdout_layer = if json {
        tracing_subscriber::fmt::layer()
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false)
            .boxed()
    } else {
        tracing_subscriber::fmt::layer().boxed()
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(stdout_layer)
        .with(crate::log_shipping::layer())
        .init();
}

/// First hop of `x-forwarded-for` (or `x-real-ip`); `-` when absent. Behind
/// Railway's proxy the real client ip is in these headers.
pub fn client_ip(h: &HeaderMap) -> String {
    h.get("x-forwarded-for")
        .or_else(|| h.get("x-real-ip"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "-".to_string())
}

/// Reuse an upstream request id (`x-request-id` / Railway's `x-railway-request-id`)
/// or mint a short one, so the same id ties together the canonical line, every log
/// during the request, and the `x-request-id` we echo back to the client.
fn request_id(h: &HeaderMap) -> String {
    h.get("x-request-id")
        .or_else(|| h.get("x-railway-request-id"))
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string()[..12].to_string())
}

/// Axum middleware: wraps the request in a `request_id` span and emits exactly one
/// canonical `info` line (target `canonical`) on completion. Echoes `x-request-id`.
pub async fn canonical_log(req: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let req_id = request_id(req.headers());
    let ip = client_ip(req.headers());

    let span = tracing::info_span!(
        "http",
        request_id = %req_id,
        method = %method,
        path = %path,
        ip = %ip,
    );

    let mut res = next.run(req).instrument(span.clone()).await;

    let status = res.status().as_u16();
    let latency_ms = start.elapsed().as_millis() as u64;
    crate::metrics::record_request(status, latency_ms); // /metrics counters (spec 0058)
    span.in_scope(|| {
        tracing::info!(target: "canonical", status, latency_ms, "request");
    });

    if let Ok(hv) = HeaderValue::from_str(&req_id) {
        res.headers_mut()
            .insert(HeaderName::from_static("x-request-id"), hv);
    }
    res
}
