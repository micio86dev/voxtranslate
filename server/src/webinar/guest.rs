//! Guest sessions (F0-4): a stable, anonymous `guest_id` for un-authenticated
//! webinar participants — the anchor for presence + join/leave history (Fase 4).
//!
//! [`guest_session`] mints the id on first contact and sets it as an HttpOnly,
//! Secure, SameSite=Lax cookie; the public payload also echoes it so the client
//! can mirror it into localStorage. Reused as-is when the cookie is already set.

use axum::extract::Request;
use axum::http::header::{COOKIE, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use uuid::Uuid;

/// Cookie name carrying the anonymous guest id.
pub const GUEST_COOKIE: &str = "guest_id";

/// The resolved guest id for a request, injected into extensions by
/// [`guest_session`] and read by handlers via `Extension<GuestId>`.
#[derive(Debug, Clone, Copy)]
pub struct GuestId(pub Uuid);

/// Parse a valid `guest_id` UUID from the request's `Cookie` header, if present.
fn guest_from_cookies(headers: &HeaderMap) -> Option<Uuid> {
    let raw = headers.get(COOKIE)?.to_str().ok()?;
    raw.split(';')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| k.trim() == GUEST_COOKIE)
        .and_then(|(_, v)| Uuid::parse_str(v.trim()).ok())
}

/// Middleware: ensure the request has a stable `guest_id`. Reuses a valid cookie;
/// otherwise mints a fresh UUID v4 and sets it as an HttpOnly, Secure,
/// SameSite=Lax, 1-year cookie on the response. The id is exposed to handlers via
/// `Extension<GuestId>`.
pub async fn guest_session(mut req: Request, next: Next) -> Response {
    let existing = guest_from_cookies(req.headers());
    let guest = existing.unwrap_or_else(Uuid::new_v4);
    req.extensions_mut().insert(GuestId(guest));
    let mut resp = next.run(req).await;
    if existing.is_none() {
        // HttpOnly (XSS can't read it) — the client mirrors the id from the
        // response body into localStorage. Secure so it only rides HTTPS.
        let cookie = format!(
            "{GUEST_COOKIE}={guest}; Path=/; Max-Age=31536000; HttpOnly; Secure; SameSite=Lax"
        );
        if let Ok(v) = HeaderValue::from_str(&cookie) {
            resp.headers_mut().append(SET_COOKIE, v);
        }
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_guest_cookie() {
        let id = Uuid::new_v4();
        let mut h = HeaderMap::new();
        h.insert(COOKIE, format!("a=1; guest_id={id}; b=2").parse().unwrap());
        assert_eq!(guest_from_cookies(&h), Some(id));
    }

    #[test]
    fn ignores_missing_or_invalid_cookie() {
        let mut h = HeaderMap::new();
        assert_eq!(guest_from_cookies(&h), None, "no cookie header");
        h.insert(COOKIE, "guest_id=not-a-uuid".parse().unwrap());
        assert_eq!(guest_from_cookies(&h), None, "malformed uuid");
        h.insert(COOKIE, "other=x".parse().unwrap());
        assert_eq!(guest_from_cookies(&h), None, "unrelated cookie");
    }
}
