//! WHIP publish authentication (F1-1 minting + F1-2 verification).
//!
//! At go-live the host is handed a short-lived HMAC token bound to their webinar
//! path. MediaMTX forwards each publish attempt to
//! `POST /internal/media-auth/{caller_secret}`; we authorize only when the caller
//! secret matches (constant-time) AND the token's HMAC + expiry + path binding all
//! check out. Read/playback are excluded at MediaMTX (`authHTTPExclude`), so only
//! `publish` reaches here — anything else is default-denied.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::AppState;

type HmacSha256 = Hmac<Sha256>;

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// HMAC-SHA256(secret, "<path>|<exp>") — the signature bytes for a publish token.
fn sign(secret: &[u8], path: &str, exp: u64) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(secret).expect("hmac accepts any key length");
    mac.update(format!("{path}|{exp}").as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// Mint a publish token bound to `path` (e.g. `webinar/AbC123`), valid for
/// `ttl_secs`. Format: `"<exp_unix>.<hex_sig>"`.
pub fn mint_publish_token(secret: &[u8], path: &str, ttl_secs: u64) -> String {
    let exp = now_unix() + ttl_secs;
    format!("{exp}.{}", hex::encode(sign(secret, path, exp)))
}

/// Verify a publish token against `path`: HMAC intact (constant-time) and not
/// expired. A token minted for a different path or secret, or past its expiry,
/// fails.
pub fn verify_publish_token(secret: &[u8], path: &str, token: &str) -> bool {
    let Some((exp_str, sig_hex)) = token.split_once('.') else {
        return false;
    };
    let Ok(exp) = exp_str.parse::<u64>() else {
        return false;
    };
    if exp < now_unix() {
        return false;
    }
    let Ok(sig) = hex::decode(sig_hex) else {
        return false;
    };
    let mut mac = HmacSha256::new_from_slice(secret).expect("hmac accepts any key length");
    mac.update(format!("{path}|{exp}").as_bytes());
    // verify_slice is constant-time and rejects a wrong-length signature.
    mac.verify_slice(&sig).is_ok()
}

/// Extract `token=<value>` from a `k=v&k2=v2` query string.
pub fn parse_query_token(query: &str) -> Option<String> {
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix("token="))
        .map(str::to_string)
}

/// Build the tokenized WHIP publish URL handed to the host at go-live.
pub fn publish_url(ingest_host: &str, code: &str, token: &str) -> String {
    format!("https://{ingest_host}/webinar/{code}/whip?token={token}")
}

/// Constant-time string equality for the caller secret (no timing oracle).
fn ct_eq(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// The subset of MediaMTX's external-auth payload we act on.
#[derive(Deserialize)]
pub struct MediaAuthReq {
    pub action: String,
    pub path: String,
    #[serde(default)]
    pub query: String,
}

/// `POST /internal/media-auth/{caller_secret}` — MediaMTX external-auth hook.
/// 2xx = allow, anything else = deny. Only `publish` should arrive (read/playback
/// are excluded at MediaMTX); we still default-deny every other action.
pub async fn media_auth(
    State(state): State<AppState>,
    Path(caller_secret): Path<String>,
    Json(req): Json<MediaAuthReq>,
) -> StatusCode {
    // Every branch below logs WHY it denied. A silent 401 here surfaces to the host as a
    // bare "authentication error" from MediaMTX, with nothing on either side saying which
    // of the four independent causes fired — the reason a mangled caller secret once took
    // a Railway HTTP-log excavation to find.
    let Some(cfg) = state.config.webinar.as_ref() else {
        tracing::warn!("media-auth denied: webinar config absent (check MEDIA_* env vars)");
        return StatusCode::NOT_FOUND;
    };
    // 1) Authenticate the caller (MediaMTX) via the secret in the path.
    if !ct_eq(&caller_secret, &cfg.caller_secret) {
        // NEVER log either secret. The lengths alone separate the two causes that look
        // identical from outside: a genuinely rotated secret (both plausible lengths) vs.
        // a deploy that injected a placeholder or garbage into mediamtx.yml (tiny length).
        tracing::warn!(
            got_len = caller_secret.len(),
            want_len = cfg.caller_secret.len(),
            path = %req.path,
            "media-auth denied: caller secret mismatch — mediamtx.yml authHTTPAddress does \
             not match MEDIA_CALLER_SECRET"
        );
        return StatusCode::UNAUTHORIZED;
    }
    // 2) Defense in depth: only publish is auth'd here.
    if req.action != "publish" {
        tracing::warn!(
            action = %req.action,
            "media-auth denied: unexpected action (read/playback must be excluded at MediaMTX)"
        );
        return StatusCode::UNAUTHORIZED;
    }
    // 3) Verify the host's token from the WHIP query string.
    match parse_query_token(&req.query) {
        Some(tok) if verify_publish_token(&cfg.auth_secret, &req.path, &tok) => StatusCode::OK,
        Some(_) => {
            tracing::warn!(
                path = %req.path,
                "media-auth denied: publish token invalid, expired, or minted for another path"
            );
            StatusCode::UNAUTHORIZED
        }
        None => {
            tracing::warn!(
                path = %req.path,
                "media-auth denied: no token in the WHIP query string"
            );
            StatusCode::UNAUTHORIZED
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: &[u8] = b"s3cret";

    #[test]
    fn valid_token_ok() {
        let p = "webinar/AbC123";
        let t = mint_publish_token(S, p, 300);
        assert!(verify_publish_token(S, p, &t));
    }

    #[test]
    fn wrong_path_rejected() {
        let t = mint_publish_token(S, "webinar/AbC123", 300);
        assert!(!verify_publish_token(S, "webinar/OTHER", &t));
    }

    #[test]
    fn wrong_secret_rejected() {
        let t = mint_publish_token(S, "webinar/x", 300);
        assert!(!verify_publish_token(b"different-secret", "webinar/x", &t));
    }

    #[test]
    fn expired_rejected() {
        let p = "webinar/x";
        let exp = now_unix() - 1;
        let t = format!("{exp}.{}", hex::encode(sign(S, p, exp)));
        assert!(!verify_publish_token(S, p, &t));
    }

    #[test]
    fn tampered_sig_rejected() {
        let p = "webinar/x";
        let t = mint_publish_token(S, p, 300);
        let last = t.chars().last().unwrap();
        let flipped = if last == 'a' { 'b' } else { 'a' };
        let bad = format!("{}{}", &t[..t.len() - 1], flipped);
        assert!(!verify_publish_token(S, p, &bad));
    }

    #[test]
    fn malformed_token_rejected() {
        let p = "webinar/x";
        assert!(!verify_publish_token(S, p, "nodot"), "no separator");
        assert!(
            !verify_publish_token(S, p, "abc.deadbeef"),
            "exp not numeric"
        );
        assert!(!verify_publish_token(S, p, "123.zz"), "sig not hex");
    }

    #[test]
    fn parse_query_token_finds_token() {
        assert_eq!(
            parse_query_token("token=1720.abcd").as_deref(),
            Some("1720.abcd")
        );
        assert_eq!(parse_query_token("a=1&token=xy&b=2").as_deref(), Some("xy"));
        assert_eq!(parse_query_token("a=1&b=2"), None);
        assert_eq!(parse_query_token(""), None);
        assert_eq!(
            parse_query_token("mytoken=xy"),
            None,
            "prefix must be exact"
        );
    }

    #[test]
    fn publish_url_shape() {
        assert_eq!(
            publish_url("ingest.voxtranslate.app", "AbC123", "1720.sig"),
            "https://ingest.voxtranslate.app/webinar/AbC123/whip?token=1720.sig"
        );
    }
}
