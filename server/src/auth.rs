//! Authentication: Google ID-token verification + our own JWT session tokens,
//! plus the `/api/auth/google` and `/api/user/me` route handlers.
//!
//! The [`TokenVerifier`] seam lets tests inject a [`FakeVerifier`] instead of
//! hitting Google. Sessions are signed JWTs (HS256) carrying the user id.

use std::time::Duration;

use async_trait::async_trait;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{Duration as ChronoDuration, Utc};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::{Pool, User};
use crate::middleware::AuthUser;
use crate::AppState;

/// A verified identity extracted from a Google ID token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoogleIdentity {
    /// Google's stable subject id (`sub`).
    pub google_id: String,
    pub email: String,
    pub name: String,
    #[serde(default)]
    pub avatar_url: Option<String>,
}

/// Verifies a third-party (Google) credential into a [`GoogleIdentity`].
#[async_trait]
pub trait TokenVerifier: Send + Sync {
    async fn verify(&self, id_token: &str) -> Result<GoogleIdentity, AuthError>;
}

/// Real verifier: calls Google's tokeninfo endpoint and checks the audience.
pub struct GoogleVerifier {
    client_id: String,
    http: reqwest::Client,
}

impl GoogleVerifier {
    pub fn new(client_id: String, http: reqwest::Client) -> Self {
        Self { client_id, http }
    }
}

#[async_trait]
impl TokenVerifier for GoogleVerifier {
    async fn verify(&self, id_token: &str) -> Result<GoogleIdentity, AuthError> {
        #[derive(Deserialize)]
        struct TokenInfo {
            sub: String,
            #[serde(default)]
            email: String,
            #[serde(default)]
            name: String,
            #[serde(default)]
            picture: Option<String>,
            aud: String,
        }

        let resp = self
            .http
            .get("https://oauth2.googleapis.com/tokeninfo")
            .query(&[("id_token", id_token)])
            .send()
            .await
            .map_err(|e| AuthError::Verify(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(AuthError::InvalidToken);
        }
        let info: TokenInfo = resp
            .json()
            .await
            .map_err(|e| AuthError::Verify(e.to_string()))?;
        // The token must have been minted for *our* OAuth client.
        if info.aud != self.client_id {
            return Err(AuthError::AudienceMismatch);
        }
        let name = if info.name.trim().is_empty() {
            info.email.clone()
        } else {
            info.name
        };
        Ok(GoogleIdentity {
            google_id: info.sub,
            email: info.email,
            name,
            avatar_url: info.picture,
        })
    }
}

/// Test verifier: the "token" is a JSON-encoded [`GoogleIdentity`]; the literal
/// string `"bad"` is rejected. Lets tests simulate any Google user.
pub struct FakeVerifier;

#[async_trait]
impl TokenVerifier for FakeVerifier {
    async fn verify(&self, id_token: &str) -> Result<GoogleIdentity, AuthError> {
        if id_token == "bad" {
            return Err(AuthError::InvalidToken);
        }
        serde_json::from_str(id_token).map_err(|_| AuthError::InvalidToken)
    }
}

/// JWT claims for a logged-in session.
#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    /// User id (UUID, as string).
    pub sub: String,
    pub email: String,
    pub name: String,
    /// Expiry, seconds since the Unix epoch.
    pub exp: usize,
}

/// Mint a signed session token for a user.
pub fn issue_jwt(
    secret: &str,
    user_id: &Uuid,
    email: &str,
    name: &str,
    expiry_hours: i64,
) -> Result<String, AuthError> {
    let exp = (Utc::now() + ChronoDuration::hours(expiry_hours)).timestamp();
    let claims = Claims {
        sub: user_id.to_string(),
        email: email.to_string(),
        name: name.to_string(),
        exp: exp.max(0) as usize,
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| AuthError::Jwt(e.to_string()))
}

/// Verify a session token's signature and expiry, returning its claims.
pub fn verify_jwt(secret: &str, token: &str) -> Result<Claims, AuthError> {
    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::new(jsonwebtoken::Algorithm::HS256),
    )
    .map_err(|_| AuthError::InvalidToken)?;
    Ok(data.claims)
}

/// Find-or-create a user from a verified Google identity. On *first* login the
/// user is granted `free_credits` (recorded as a `free_credit` ledger row) and
/// stamped with the acquisition `source` (first-touch attribution); returning
/// users only get their profile refreshed — balance and source are untouched.
pub async fn upsert_google_user(
    pool: &Pool,
    identity: &GoogleIdentity,
    free_credits: Decimal,
    source: Option<&str>,
    locale: Option<&str>,
) -> Result<User, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let existing: Option<User> = sqlx::query_as("SELECT * FROM users WHERE google_id = $1")
        .bind(&identity.google_id)
        .fetch_optional(&mut *tx)
        .await?;

    let user = match existing {
        Some(_) => {
            // COALESCE keeps the stored locale when this login omitted one.
            sqlx::query_as(
                "UPDATE users SET email = $2, name = $3, avatar_url = $4,
                                  locale = COALESCE($5, locale), updated_at = now()
                 WHERE google_id = $1 RETURNING *",
            )
            .bind(&identity.google_id)
            .bind(&identity.email)
            .bind(&identity.name)
            .bind(&identity.avatar_url)
            .bind(locale)
            .fetch_one(&mut *tx)
            .await?
        }
        None => {
            let user: User = sqlx::query_as(
                "INSERT INTO users (google_id, email, name, avatar_url, balance, source, locale)
                 VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING *",
            )
            .bind(&identity.google_id)
            .bind(&identity.email)
            .bind(&identity.name)
            .bind(&identity.avatar_url)
            .bind(free_credits)
            .bind(source)
            .bind(locale)
            .fetch_one(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO credit_transactions
                     (user_id, amount, kind, balance_after, description)
                 VALUES ($1, $2, 'free_credit', $3, 'Welcome credits')",
            )
            .bind(user.id)
            .bind(free_credits)
            .bind(free_credits)
            .execute(&mut *tx)
            .await?;
            user
        }
    };

    tx.commit().await?;
    Ok(user)
}

/// Public-facing user profile. Balance is the only monetary value sent to the
/// client; raw cost/markup/rate are never serialized.
#[derive(Debug, Serialize)]
pub struct UserProfile {
    pub id: String,
    pub email: String,
    pub name: String,
    pub avatar_url: Option<String>,
    pub balance: f64,
    /// True once the user confirmed 18+ and accepted the ToS/Privacy.
    pub consent_given: bool,
    /// True once the account has a stored Cartesia cloned voice (spec 0108). The client
    /// uses this to skip the pre-join voice-prep prompt on EVERY device, not just the one
    /// that recorded it — the per-device localStorage flag is only a local short-circuit.
    pub has_voice_clone: bool,
}

impl From<User> for UserProfile {
    fn from(u: User) -> Self {
        Self {
            id: u.id.to_string(),
            email: u.email,
            name: u.name,
            avatar_url: u.avatar_url,
            balance: u.balance.to_f64().unwrap_or(0.0),
            consent_given: u.age_confirmed && u.consent_tos_at.is_some(),
            has_voice_clone: u.cartesia_voice_id.is_some(),
        }
    }
}

#[derive(Deserialize)]
pub struct GoogleAuthRequest {
    /// Legacy GSI `credential` (a Google ID token). Still accepted during the
    /// transition to the OAuth code flow; mutually exclusive with `code`.
    #[serde(default)]
    pub credential: Option<String>,
    /// OAuth authorization `code` from `google.accounts.oauth2.initCodeClient`. The
    /// server exchanges it for an id_token + access/refresh tokens (Calendar access).
    #[serde(default)]
    pub code: Option<String>,
    /// Redirect URI the code client used (the app origin for `ux_mode:'popup'`).
    /// Must match an Authorized redirect URI on the OAuth client.
    #[serde(default)]
    pub redirect_uri: Option<String>,
    /// Acquisition source for marketing attribution (`?source` / `utm_source` the
    /// visitor arrived with). Recorded only on first login; ignored thereafter.
    #[serde(default)]
    pub source: Option<String>,
    /// The UI language the user is signing in with (e.g. `it`, `pt-BR`). Stored on the
    /// user so outbound notifications can be localized in their own language. Updated
    /// on every login so a language switch is reflected.
    #[serde(default)]
    pub locale: Option<String>,
}

/// Normalise a client-supplied acquisition source: trim, drop empties, and cap at
/// 64 chars so a hostile/oversized query param can't bloat the column.
fn clean_source(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(64).collect())
}

/// Normalise a client-supplied UI locale to a short base code (e.g. `pt-BR` → `pt`).
/// Keeps only ASCII letters, lowercases, and caps length so a hostile value can't
/// bloat the column or break the localized-copy lookup. Returns `None` if unusable.
fn clean_locale(raw: Option<&str>) -> Option<String> {
    let base = raw?
        .trim()
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let ok = (2..=8).contains(&base.len()) && base.chars().all(|c| c.is_ascii_lowercase());
    ok.then_some(base)
}

#[derive(Serialize)]
pub struct AuthResponse {
    pub token: String,
    pub user: UserProfile,
    /// True when this session has a usable Google Calendar connection (a stored
    /// refresh token). The frontends gate meeting-scheduling on this and otherwise
    /// prompt a re-consent sign-in.
    pub calendar_connected: bool,
}

/// `POST /api/auth/google` — verify a Google credential, upsert the user
/// (granting free credits on first login), and return a session JWT.
pub async fn auth_google(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<GoogleAuthRequest>,
) -> Response {
    let (Some(billing), Some(pool)) = (state.config.billing.as_ref(), state.pool.as_ref()) else {
        return service_unavailable();
    };

    // Throttle login attempts per client (20 / minute), keyed by the trusted-proxy IP
    // (issue #117 — last X-Forwarded-For hop, not the spoofable left-most one).
    let client_key = crate::observability::client_ip(&headers);
    if !state
        .rate_limiter
        .allow(&format!("auth:{client_key}"), 20, Duration::from_secs(60))
    {
        return (StatusCode::TOO_MANY_REQUESTS, "too many requests").into_response();
    }

    // Resolve a verified identity from EITHER the new OAuth code (which also yields
    // Calendar tokens to persist) OR the legacy ID-token credential.
    let (identity, token_set) = if let Some(code) = body.code.as_deref().filter(|c| !c.is_empty()) {
        if !billing.calendar_enabled() {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "google calendar oauth not configured",
            )
                .into_response();
        }
        let redirect_uri = body.redirect_uri.as_deref().unwrap_or_default();
        let tokens = match crate::google_oauth::exchange_code(
            &state.http,
            &billing.google_client_id,
            &billing.google_client_secret,
            code,
            redirect_uri,
        )
        .await
        {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("google code exchange failed: {e}");
                return (StatusCode::UNAUTHORIZED, "google sign-in failed").into_response();
            }
        };
        match state.verifier.verify(&tokens.id_token).await {
            Ok(id) => (id, Some(tokens)),
            Err(_) => {
                return (StatusCode::UNAUTHORIZED, "invalid Google credential").into_response()
            }
        }
    } else if let Some(credential) = body.credential.as_deref().filter(|c| !c.is_empty()) {
        match state.verifier.verify(credential).await {
            Ok(id) => (id, None),
            Err(_) => {
                return (StatusCode::UNAUTHORIZED, "invalid Google credential").into_response()
            }
        }
    } else {
        return (StatusCode::BAD_REQUEST, "missing credential or code").into_response();
    };

    let free_credits = Decimal::from_f64_retain(billing.pricing.free_credits)
        .unwrap_or(Decimal::ZERO)
        .round_dp(6);
    let source = clean_source(body.source.as_deref());
    let locale = clean_locale(body.locale.as_deref());
    let user = match upsert_google_user(
        pool,
        &identity,
        free_credits,
        source.as_deref(),
        locale.as_deref(),
    )
    .await
    {
        Ok(u) => u,
        Err(e) => {
            tracing::error!("upsert user failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "login failed").into_response();
        }
    };

    // Persist Google tokens (best-effort — a storage hiccup must not block login,
    // it just means calendar features are unavailable until the next sign-in).
    let mut calendar_connected = false;
    if let Some(tokens) = token_set.as_ref() {
        if let Some(enc_key) = billing.google_token_enc_key.as_deref() {
            match crate::google_oauth::store_tokens(pool, enc_key, user.id, tokens).await {
                Ok(()) => calendar_connected = true,
                Err(e) => tracing::error!("store google tokens failed: {e}"),
            }
        }
    } else {
        // Credential (legacy) path — report whether a prior connection still exists.
        calendar_connected = sqlx::query_scalar::<_, bool>(
            "SELECT refresh_token_encrypted IS NOT NULL
             FROM google_oauth_tokens WHERE user_id = $1",
        )
        .bind(user.id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(false);
    }

    let token = match issue_jwt(
        &billing.jwt_secret,
        &user.id,
        &user.email,
        &user.name,
        billing.jwt_expiry_hours,
    ) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("jwt issue failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "login failed").into_response();
        }
    };

    Json(AuthResponse {
        token,
        user: UserProfile::from(user),
        calendar_connected,
    })
    .into_response()
}

/// `DELETE /api/auth/google/connection` — forget the caller's stored Google tokens
/// (account change / explicit disconnect). Login still works; calendar features go
/// dormant until the user signs in again and re-consents.
pub async fn disconnect_google(State(state): State<AppState>, user: AuthUser) -> Response {
    let Some(pool) = state.pool.as_ref() else {
        return service_unavailable();
    };
    match crate::google_oauth::disconnect(pool, user.user_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!("disconnect google failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "disconnect failed").into_response()
        }
    }
}

/// `GET /api/auth/config` — public auth config for the client (the Google OAuth
/// client id, needed to render the Sign-In button). 503 in guest-only mode, so
/// the client also uses this as its "is billing enabled?" probe.
pub async fn auth_config(State(state): State<AppState>) -> Response {
    match state.config.billing.as_ref() {
        Some(b) => Json(serde_json::json!({
            "google_client_id": b.google_client_id,
            // Listener-pays mode (spec 0099): the client reframes the engine-cost copy
            // to "the quality you RECEIVE" instead of "per translation language".
            "listener_pays": state.config.listener_pays,
        }))
        .into_response(),
        None => service_unavailable(),
    }
}

/// `GET /api/user/me` — return the authenticated user's profile + balance.
pub async fn user_me(State(state): State<AppState>, user: AuthUser) -> Response {
    let Some(pool) = state.pool.as_ref() else {
        return service_unavailable();
    };
    match sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = $1")
        .bind(user.user_id)
        .fetch_optional(pool)
        .await
    {
        Ok(Some(u)) => Json(UserProfile::from(u)).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "user not found").into_response(),
        Err(e) => {
            tracing::error!("load user failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

fn service_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "auth/billing not configured",
    )
        .into_response()
}

/// Auth failures. Distinct variants aid logging; the API maps them all to 401.
#[derive(Debug)]
pub enum AuthError {
    InvalidToken,
    AudienceMismatch,
    Jwt(String),
    Verify(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::InvalidToken => write!(f, "invalid token"),
            AuthError::AudienceMismatch => write!(f, "audience mismatch"),
            AuthError::Jwt(e) => write!(f, "jwt error: {e}"),
            AuthError::Verify(e) => write!(f, "verify error: {e}"),
        }
    }
}

impl std::error::Error for AuthError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_source_trims_filters_and_caps() {
        assert_eq!(clean_source(None), None);
        assert_eq!(clean_source(Some("   ")), None);
        assert_eq!(clean_source(Some("  reddit ")), Some("reddit".to_string()));
        // Capped at 64 chars.
        let long = "x".repeat(100);
        assert_eq!(clean_source(Some(&long)).unwrap().len(), 64);
    }

    #[test]
    fn clean_locale_normalizes_to_base_code() {
        assert_eq!(clean_locale(None), None);
        assert_eq!(clean_locale(Some("  ")), None);
        assert_eq!(clean_locale(Some("it")), Some("it".to_string()));
        assert_eq!(clean_locale(Some("PT-BR")), Some("pt".to_string())); // region dropped + lowercased
        assert_eq!(clean_locale(Some("zh_Hans")), Some("zh".to_string())); // underscore variant
        assert_eq!(clean_locale(Some("e")), None); // too short
        assert_eq!(clean_locale(Some("123")), None); // non-letters rejected
        assert_eq!(clean_locale(Some("toolonglocale")), None); // > 8 chars
    }

    #[test]
    fn jwt_round_trip_and_reject_tampered() {
        let id = Uuid::new_v4();
        let token = issue_jwt("secret", &id, "a@b.com", "Alice", 168).unwrap();
        let claims = verify_jwt("secret", &token).unwrap();
        assert_eq!(claims.sub, id.to_string());
        assert_eq!(claims.email, "a@b.com");
        assert_eq!(claims.name, "Alice");
        // Wrong signing key is rejected.
        assert!(verify_jwt("other-secret", &token).is_err());
    }

    #[test]
    fn jwt_expired_is_rejected() {
        let id = Uuid::new_v4();
        // Expiry one hour in the past (beyond jsonwebtoken's 60s leeway).
        let token = issue_jwt("secret", &id, "a@b.com", "Alice", -1).unwrap();
        assert!(verify_jwt("secret", &token).is_err());
    }

    #[tokio::test]
    async fn fake_verifier_parses_and_rejects() {
        let v = FakeVerifier;
        let tok = serde_json::json!({"google_id":"g1","email":"e@x.com","name":"E"}).to_string();
        let id = v.verify(&tok).await.unwrap();
        assert_eq!(id.google_id, "g1");
        assert_eq!(id.email, "e@x.com");
        assert!(v.verify("bad").await.is_err());
        assert!(v.verify("{not json").await.is_err());
    }

    /// DB-gated: first login grants free credits exactly once; later logins
    /// refresh the profile but never re-grant. Skipped without `DATABASE_URL`.
    #[tokio::test]
    async fn upsert_grants_free_credit_once() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        let pool = crate::db::connect(&url).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();

        let gid = format!("g-{}", Uuid::new_v4());
        let identity = GoogleIdentity {
            google_id: gid.clone(),
            email: format!("{}@x.com", Uuid::new_v4()),
            name: "First".into(),
            avatar_url: Some("http://img/1".into()),
        };
        let free = Decimal::new(200, 2); // 2.00

        let u1 = upsert_google_user(&pool, &identity, free, Some("reddit-launch"), Some("it"))
            .await
            .unwrap();
        assert_eq!(u1.balance, free);
        assert_eq!(u1.name, "First");
        assert_eq!(u1.source.as_deref(), Some("reddit-launch"));
        assert_eq!(u1.locale.as_deref(), Some("it"));

        // Second login: profile refresh, balance unchanged.
        let identity2 = GoogleIdentity {
            name: "Renamed".into(),
            ..identity.clone()
        };
        // Second login with a *different* source: profile refreshes, but the
        // first-touch source (and balance) is preserved.
        // ...and a login that omits the locale must not wipe the stored one (COALESCE).
        let u2 = upsert_google_user(&pool, &identity2, free, Some("twitter-ad"), None)
            .await
            .unwrap();
        assert_eq!(u2.id, u1.id);
        assert_eq!(u2.name, "Renamed");
        assert_eq!(u2.locale.as_deref(), Some("it"));
        assert_eq!(u2.balance, free);
        assert_eq!(u2.source.as_deref(), Some("reddit-launch"));

        // Exactly one free-credit ledger row.
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM credit_transactions WHERE user_id = $1 AND kind = 'free_credit'",
        )
        .bind(u1.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1);
    }
}
