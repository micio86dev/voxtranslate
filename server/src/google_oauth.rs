//! Google OAuth 2.0 authorization-code flow + encrypted token storage (spec:
//! scheduled meetings, Phase 1a).
//!
//! Login now uses GIS `oauth2.initCodeClient`, which hands the frontend a short
//! authorization **code**. The server exchanges it for an `id_token` (verified the
//! same way as the legacy GSI credential), an `access_token`, and — on first consent
//! — a long-lived `refresh_token`. The refresh token is encrypted at rest
//! (XChaCha20-Poly1305) so the server can mint fresh access tokens to call the
//! Google Calendar API on the user's behalf.

use base64::Engine;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Deserialize;
use uuid::Uuid;

use crate::db::Pool;
use crate::AppState;

const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
/// Refresh a little before true expiry so an in-flight call never races the clock.
const EXPIRY_SKEW_SECS: i64 = 60;

/// Tokens returned by the authorization-code exchange.
#[derive(Debug, Clone)]
pub struct TokenSet {
    pub id_token: String,
    pub access_token: String,
    /// Present only on the first consent (or with `prompt=consent`). Never clobber a
    /// stored refresh token with `None` on a later sign-in.
    pub refresh_token: Option<String>,
    pub expires_at: DateTime<Utc>,
    pub scopes: String,
}

#[derive(Debug)]
pub enum OauthError {
    /// Calendar OAuth is not configured (missing client secret or encryption key).
    NotConfigured,
    /// No stored connection for this user (never consented, or disconnected).
    NoConnection,
    Http(String),
    Google(String),
    Crypto(String),
    Db(String),
}

impl std::fmt::Display for OauthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OauthError::NotConfigured => write!(f, "google calendar not configured"),
            OauthError::NoConnection => write!(f, "no google connection for user"),
            OauthError::Http(e) => write!(f, "google http error: {e}"),
            OauthError::Google(e) => write!(f, "google token error: {e}"),
            OauthError::Crypto(e) => write!(f, "token crypto error: {e}"),
            OauthError::Db(e) => write!(f, "db error: {e}"),
        }
    }
}
impl std::error::Error for OauthError {}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    id_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: i64,
    #[serde(default)]
    scope: String,
}

#[derive(Deserialize)]
struct TokenError {
    #[serde(default)]
    error: String,
    #[serde(default)]
    error_description: String,
}

/// Exchange an authorization `code` (from the frontend code-client) for tokens.
pub async fn exchange_code(
    http: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
) -> Result<TokenSet, OauthError> {
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("redirect_uri", redirect_uri),
    ];
    post_token(http, &params).await
}

/// Use a stored refresh token to mint a fresh access token. The response carries no
/// new refresh token, so the caller keeps the existing one.
async fn refresh_access_token(
    http: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<TokenSet, OauthError> {
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
        ("client_secret", client_secret),
    ];
    post_token(http, &params).await
}

async fn post_token(
    http: &reqwest::Client,
    params: &[(&str, &str)],
) -> Result<TokenSet, OauthError> {
    let resp = http
        .post(TOKEN_ENDPOINT)
        .form(params)
        .send()
        .await
        .map_err(|e| OauthError::Http(e.to_string()))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| OauthError::Http(e.to_string()))?;
    if !status.is_success() {
        let msg = serde_json::from_str::<TokenError>(&body)
            .map(|e| format!("{}: {}", e.error, e.error_description))
            .unwrap_or(body);
        return Err(OauthError::Google(msg));
    }
    let t: TokenResponse =
        serde_json::from_str(&body).map_err(|e| OauthError::Google(e.to_string()))?;
    Ok(TokenSet {
        id_token: t.id_token,
        access_token: t.access_token,
        refresh_token: t.refresh_token,
        expires_at: Utc::now() + ChronoDuration::seconds(t.expires_in.max(0)),
        scopes: t.scope,
    })
}

// --- Encryption (XChaCha20-Poly1305; 24-byte random nonce prepended) -------------

fn encrypt_token(key: &[u8], plaintext: &str) -> Result<String, OauthError> {
    let cipher = XChaCha20Poly1305::new_from_slice(key)
        .map_err(|e| OauthError::Crypto(e.to_string()))?;
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ct = cipher
        .encrypt(&nonce, plaintext.as_bytes())
        .map_err(|e| OauthError::Crypto(e.to_string()))?;
    let mut blob = nonce.to_vec();
    blob.extend_from_slice(&ct);
    Ok(base64::engine::general_purpose::STANDARD.encode(blob))
}

fn decrypt_token(key: &[u8], b64: &str) -> Result<String, OauthError> {
    let blob = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| OauthError::Crypto(e.to_string()))?;
    if blob.len() < 24 {
        return Err(OauthError::Crypto("ciphertext too short".into()));
    }
    let (nonce_bytes, ct) = blob.split_at(24);
    let cipher = XChaCha20Poly1305::new_from_slice(key)
        .map_err(|e| OauthError::Crypto(e.to_string()))?;
    let pt = cipher
        .decrypt(XNonce::from_slice(nonce_bytes), ct)
        .map_err(|e| OauthError::Crypto(e.to_string()))?;
    String::from_utf8(pt).map_err(|e| OauthError::Crypto(e.to_string()))
}

// --- Persistence -----------------------------------------------------------------

/// Upsert the user's Google tokens. A `None` refresh token never overwrites a stored
/// one (Google only returns it on first consent / `prompt=consent`).
pub async fn store_tokens(
    pool: &Pool,
    enc_key: &[u8],
    user_id: Uuid,
    tokens: &TokenSet,
) -> Result<(), OauthError> {
    let refresh_enc = match tokens.refresh_token.as_deref() {
        Some(rt) if !rt.is_empty() => Some(encrypt_token(enc_key, rt)?),
        _ => None,
    };
    sqlx::query(
        "INSERT INTO google_oauth_tokens
             (user_id, refresh_token_encrypted, access_token, expires_at, scopes, updated_at)
         VALUES ($1, $2, $3, $4, $5, now())
         ON CONFLICT (user_id) DO UPDATE SET
             refresh_token_encrypted =
                 COALESCE(EXCLUDED.refresh_token_encrypted, google_oauth_tokens.refresh_token_encrypted),
             access_token = EXCLUDED.access_token,
             expires_at   = EXCLUDED.expires_at,
             scopes       = EXCLUDED.scopes,
             updated_at   = now()",
    )
    .bind(user_id)
    .bind(refresh_enc)
    .bind(&tokens.access_token)
    .bind(tokens.expires_at)
    .bind(&tokens.scopes)
    .execute(pool)
    .await
    .map_err(|e| OauthError::Db(e.to_string()))?;
    Ok(())
}

/// Forget a user's Google connection (account change / explicit disconnect).
pub async fn disconnect(pool: &Pool, user_id: Uuid) -> Result<(), OauthError> {
    sqlx::query("DELETE FROM google_oauth_tokens WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .map_err(|e| OauthError::Db(e.to_string()))?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct TokenRow {
    refresh_token_encrypted: Option<String>,
    access_token: Option<String>,
    expires_at: Option<DateTime<Utc>>,
}

/// Return a currently-valid Google access token for `user_id`, refreshing via the
/// stored refresh token when the cached one is missing or about to expire. This is
/// the single accessor the Calendar module uses.
pub async fn valid_access_token(state: &AppState, user_id: Uuid) -> Result<String, OauthError> {
    let billing = state
        .config
        .billing
        .as_ref()
        .filter(|b| b.calendar_enabled())
        .ok_or(OauthError::NotConfigured)?;
    let enc_key = billing
        .google_token_enc_key
        .as_deref()
        .ok_or(OauthError::NotConfigured)?;
    let pool = state.pool.as_ref().ok_or(OauthError::NotConfigured)?;

    let row: Option<TokenRow> = sqlx::query_as(
        "SELECT refresh_token_encrypted, access_token, expires_at
         FROM google_oauth_tokens WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| OauthError::Db(e.to_string()))?;
    let row = row.ok_or(OauthError::NoConnection)?;

    // Cached access token still good?
    if let (Some(token), Some(exp)) = (row.access_token.as_ref(), row.expires_at) {
        if exp > Utc::now() + ChronoDuration::seconds(EXPIRY_SKEW_SECS) {
            return Ok(token.clone());
        }
    }

    // Refresh.
    let refresh_enc = row
        .refresh_token_encrypted
        .as_deref()
        .ok_or(OauthError::NoConnection)?;
    let refresh_token = decrypt_token(enc_key, refresh_enc)?;
    let fresh = refresh_access_token(
        &state.http,
        &billing.google_client_id,
        &billing.google_client_secret,
        &refresh_token,
    )
    .await?;
    store_tokens(pool, enc_key, user_id, &fresh).await?;
    Ok(fresh.access_token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_round_trip() {
        let key = [7u8; 32];
        let blob = encrypt_token(&key, "1//refresh-token-value").unwrap();
        // Ciphertext is not the plaintext, and decrypts back exactly.
        assert!(!blob.contains("refresh-token-value"));
        assert_eq!(decrypt_token(&key, &blob).unwrap(), "1//refresh-token-value");
    }

    #[test]
    fn decrypt_rejects_wrong_key() {
        let blob = encrypt_token(&[1u8; 32], "secret").unwrap();
        assert!(decrypt_token(&[2u8; 32], &blob).is_err());
    }
}
