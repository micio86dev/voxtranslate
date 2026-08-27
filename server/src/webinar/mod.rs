//! Webinar Mode (SPEC "Webinar Mode") — the broadcast (1-to-many) control plane.
//!
//! Phase 0-1 lives here: short join-code generation with collision-safe retry
//! (F0-2), and (in later tasks) the CRUD handlers, media-URL provisioning, and
//! lifecycle. The media path itself (WHIP ingest → LL-HLS) is off-box; this
//! module is only the Rust control plane on top of the `webinars` table (037).

use std::future::Future;

use chrono::{DateTime, Utc};
use rand::RngExt;
use sqlx::FromRow;
use uuid::Uuid;

use crate::config::WebinarConfig;
use crate::db::Pool;

/// The authenticated caller, when the request carries a valid session JWT.
///
/// `{code}`-addressed webinar endpoints are reachable by guests, so auth here is
/// optional and advisory rather than an extractor that rejects. Centralised
/// because three call sites had grown byte-identical copies of this chain.
pub fn caller_id(state: &crate::AppState, headers: &axum::http::HeaderMap) -> Option<Uuid> {
    let billing = state.config.billing.as_ref()?;
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .and_then(|tok| crate::auth::verify_jwt(&billing.jwt_secret, tok).ok())
        .and_then(|c| Uuid::parse_str(&c.sub).ok())
}

/// Enforce a webinar's `members_only` flag on an endpoint that serves its
/// CONTENT, accepts PARTICIPATION, or spends its budget.
///
/// What the flag means, precisely (migration 044): an unauthenticated guest may
/// still see that the webinar EXISTS — title, schedule, host — so the client can
/// render a sign-in gate. They may not read what was said in it, take part in
/// it, or cause it to spend money. `public_get` is therefore deliberately NOT a
/// caller of this; everything else keyed by `{code}` is.
///
/// Being logged in is the whole bar, matching the presence WebSocket's gate:
/// this is "members only" in the sign-up sense, not org membership.
// `Response` as the Err variant is intentionally large — the same convention the
// handler modules that call this declare crate-wide.
#[allow(clippy::result_large_err)]
pub fn require_member_access(
    w: &Webinar,
    state: &crate::AppState,
    headers: &axum::http::HeaderMap,
) -> Result<(), axum::response::Response> {
    use axum::response::IntoResponse as _;
    if w.members_only && caller_id(state, headers).is_none() {
        return Err((
            axum::http::StatusCode::UNAUTHORIZED,
            "this webinar is open to signed-in members only",
        )
            .into_response());
    }
    Ok(())
}

pub mod ai;
pub mod chat;
pub mod files;
pub mod guest;
pub mod media;
pub mod metrics;
pub mod presence;
pub mod routes;
pub mod stt;

/// Base58 alphabet: digits + letters MINUS the visually ambiguous `0 O I l`, so a
/// code read off a screen or scanned from a QR is unambiguous.
pub const CODE_ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

/// Default join-code length; overridable via `WEBINAR_CODE_LEN`. At 58 symbols,
/// len=10 is ~58 bits of entropy — not guessable.
pub const DEFAULT_CODE_LEN: usize = 10;

/// A random, non-guessable join code of `len` chars drawn uniformly from
/// [`CODE_ALPHABET`].
pub fn generate_webinar_code(len: usize) -> String {
    let mut rng = rand::rng();
    (0..len)
        .map(|_| CODE_ALPHABET[rng.random_range(0..CODE_ALPHABET.len())] as char)
        .collect()
}

/// Whether a sqlx error is a Postgres UNIQUE violation (SQLSTATE 23505) — the
/// signal to regenerate the code and retry the insert.
pub fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.code().as_deref() == Some("23505"))
}

/// Run `attempt` with a freshly generated code, retrying up to `max` times while
/// `retry_if` classifies the error as a code collision. Generic over the error so
/// the retry logic is unit-testable without a live database.
pub async fn with_code_retry<T, E, F, Fut>(
    code_len: usize,
    max: usize,
    retry_if: impl Fn(&E) -> bool,
    mut attempt: F,
) -> Result<T, E>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut tries = 0;
    loop {
        tries += 1;
        let code = generate_webinar_code(code_len);
        match attempt(code).await {
            Ok(v) => return Ok(v),
            Err(e) if retry_if(&e) && tries < max => continue,
            Err(e) => return Err(e),
        }
    }
}

// ---- DB layer: the `webinars` row + create (with code retry) ---------------

/// A row from `webinars` (migration 037).
#[derive(Debug, Clone, FromRow)]
pub struct Webinar {
    pub id: Uuid,
    pub org_id: Uuid,
    pub host_user_id: Option<Uuid>,
    pub code: String,
    pub title: String,
    pub description: Option<String>,
    pub source_language: String,
    pub tier: String,
    pub status: String,
    pub scheduled_start: Option<DateTime<Utc>>,
    pub scheduled_end: Option<DateTime<Utc>>,
    pub actual_start: Option<DateTime<Utc>>,
    pub actual_end: Option<DateTime<Utc>>,
    pub record_video: bool,
    pub record_transcript: bool,
    pub voice_clone: bool,
    /// When set, the per-webinar auto-translated chat panel is on; every message
    /// is persisted (recorded) — the gate is independent of `record_transcript`.
    pub chat_enabled: bool,
    /// `'private'` (default) — reachable only via the direct `/w/{code}` link — or
    /// `'public'` — also discoverable via the public list endpoint (042).
    pub visibility: String,
    pub project_id: Option<Uuid>,
    pub google_event_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Soft-archive timestamp (039). NULL = active; set = hidden from the active list.
    pub archived_at: Option<DateTime<Utc>>,
    /// Snapshot of the host's avatar URL at webinar creation time (043). NULL for
    /// webinars created before the migration or when the host has no avatar.
    pub host_avatar_url: Option<String>,
    /// When true, only authenticated (logged-in) users may reach this webinar's
    /// content, take part in it, or cause it to spend money: the presence WebSocket,
    /// the transcript, chat (read and write), file upload, and TTS-token minting.
    /// Unauthenticated guests can still read its METADATA — that is deliberate, so
    /// the client can render a sign-in gate rather than a dead end (migration 044).
    /// Enforced through [`require_member_access`]; see the `coverage` tests for why
    /// the enforcement lives in one helper instead of at each call site.
    pub members_only: bool,
    /// Lead time, in minutes, for the "starting soon" reminder sent to the host's
    /// accepted friends when this is a PUBLIC scheduled webinar (default 10, clamped
    /// 0..=1440). Mirrors `scheduled_meetings.reminder_minutes_before` (migration 047).
    pub reminder_minutes_before: i32,
    /// Dedup marker for the friend reminder: NULL = not yet notified, set = fired once
    /// (either the time-based "soon" reminder or the go-live "live now" hook). Migration 047.
    pub reminder_sent_at: Option<DateTime<Utc>>,
    /// When false, no friend-reminder notifications are sent for this webinar — neither
    /// from the timed scheduler nor from the go-live hook (migration 051). Default true
    /// preserves the prior always-notify behavior.
    pub notify_friends: bool,
}

/// Fields for creating a webinar (F0-3); the `code` is generated server-side.
pub struct NewWebinar<'a> {
    pub org_id: Uuid,
    pub host_user_id: Uuid,
    pub title: &'a str,
    pub description: Option<&'a str>,
    pub source_language: &'a str,
    pub tier: &'a str,
    pub record_video: bool,
    pub record_transcript: bool,
    pub voice_clone: bool,
    pub chat_enabled: bool,
    pub visibility: &'a str,
    pub scheduled_start: Option<DateTime<Utc>>,
    pub scheduled_end: Option<DateTime<Utc>>,
    pub project_id: Option<Uuid>,
    pub members_only: bool,
    /// Reminder lead time in minutes (already clamped 0..=1440 by the caller).
    pub reminder_minutes_before: i32,
    /// Whether to notify the host's accepted friends about this webinar (migration 051).
    pub notify_friends: bool,
}

/// Insert a webinar, generating a fresh code and retrying the rare UNIQUE(code)
/// collision (F0-2).
pub async fn create_webinar(
    pool: &Pool,
    new: &NewWebinar<'_>,
    code_len: usize,
) -> Result<Webinar, sqlx::Error> {
    const MAX_ATTEMPTS: usize = 8;
    with_code_retry(code_len, MAX_ATTEMPTS, is_unique_violation, |code| {
        insert_webinar(pool, new, code)
    })
    .await
}

async fn insert_webinar(
    pool: &Pool,
    new: &NewWebinar<'_>,
    code: String,
) -> Result<Webinar, sqlx::Error> {
    sqlx::query_as(
        "INSERT INTO webinars
            (org_id, host_user_id, code, title, description, source_language, tier,
             record_video, record_transcript, voice_clone, chat_enabled, visibility,
             scheduled_start, scheduled_end, project_id, host_avatar_url, members_only,
             reminder_minutes_before, notify_friends)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
             (SELECT avatar_url FROM users WHERE id = $2), $16, $17, $18)
         RETURNING *",
    )
    .bind(new.org_id)
    .bind(new.host_user_id)
    .bind(code)
    .bind(new.title)
    .bind(new.description)
    .bind(new.source_language)
    .bind(new.tier)
    .bind(new.record_video)
    .bind(new.record_transcript)
    .bind(new.voice_clone)
    .bind(new.chat_enabled)
    .bind(new.visibility)
    .bind(new.scheduled_start)
    .bind(new.scheduled_end)
    .bind(new.project_id)
    .bind(new.members_only)
    .bind(new.reminder_minutes_before)
    .bind(new.notify_friends)
    .fetch_one(pool)
    .await
}

/// Look up a webinar by its public join code.
pub async fn find_by_code(pool: &Pool, code: &str) -> Result<Option<Webinar>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM webinars WHERE code = $1")
        .bind(code)
        .fetch_optional(pool)
        .await
}

/// Look up a webinar by id.
pub async fn find_by_id(pool: &Pool, id: Uuid) -> Result<Option<Webinar>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM webinars WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

// ---- URL helpers -----------------------------------------------------------

/// Public participant page for a code: `{app_base_url}/w/{code}`.
pub fn join_url(app_base_url: &str, code: &str) -> String {
    format!("{}/w/{}", app_base_url.trim_end_matches('/'), code)
}

/// Public LL-HLS playback URL for a code (served from the HLS host).
pub fn playback_url(cfg: &WebinarConfig, code: &str) -> String {
    format!("https://{}/webinar/{}/index.m3u8", cfg.hls_host, code)
}

#[cfg(test)]
mod coverage {
    //! Coverage guards for `members_only`.
    //!
    //! The audit finding these exist for: `members_only` was enforced in exactly
    //! ONE place — the presence WebSocket — while five other `{code}`-addressed
    //! endpoints ignored it, so a guest with the link could still read the
    //! transcript, read and post chat, upload files, and mint a TTS token. The
    //! flag did not mean what its name promised.
    //!
    //! That was a coverage defect, not a logic defect: each handler was
    //! individually reasonable and collectively they left the door open. A unit
    //! test of the policy function would have passed the whole time. So these
    //! tests assert the *call sites* instead, and they fail when the next
    //! `{code}` endpoint is added without the guard.

    /// Extract a top-level `fn` body from source text. Relies on rustfmt putting
    /// the closing brace of a top-level item in column 0, which is true for every
    /// handler here and is checked by `cargo fmt` in CI.
    fn body_of<'a>(src: &'a str, signature: &str) -> &'a str {
        let start = src
            .find(signature)
            .unwrap_or_else(|| panic!("handler not found — did it get renamed? {signature}"));
        let rest = &src[start..];
        let end = rest.find("\n}").map(|i| i + 2).unwrap_or(rest.len());
        &rest[..end]
    }

    /// Every `{code}`-addressed endpoint that serves webinar CONTENT, accepts
    /// PARTICIPATION, or spends the host's budget.
    ///
    /// Deliberately absent: `public_get`, which serves only metadata. Migration
    /// 044's documented intent is that a guest may still see a members-only
    /// webinar EXISTS so the client can render a sign-in gate — gating it would
    /// break the very flow the flag is for. `presence_ws` is also absent: it
    /// carries its own inline gate because it must close the socket with a
    /// policy-violation frame rather than return a status code.
    const GATED: &[(&str, &str, &str)] = &[
        (
            "routes.rs",
            include_str!("routes.rs"),
            "pub async fn list_public_transcript(",
        ),
        (
            "routes.rs",
            include_str!("routes.rs"),
            "pub async fn tts_session(",
        ),
        (
            "chat.rs",
            include_str!("chat.rs"),
            "pub async fn post_chat(",
        ),
        (
            "chat.rs",
            include_str!("chat.rs"),
            "pub async fn list_chat(",
        ),
        (
            "files.rs",
            include_str!("files.rs"),
            "pub async fn upload_webinar_file(",
        ),
    ];

    #[test]
    fn every_code_addressed_content_endpoint_enforces_members_only() {
        for (file, src, signature) in GATED {
            let body = body_of(src, signature);
            assert!(
                body.contains("require_member_access"),
                "{file} :: {signature} does not call require_member_access — a guest \
                 with the link can reach a members-only webinar's content through it"
            );
        }
    }

    #[test]
    fn public_get_stays_open_so_the_sign_in_gate_can_render() {
        // The inverse guard: gating metadata would break migration 044's intent.
        // If someone "fixes" this by adding the guard here, that is a regression.
        let body = body_of(include_str!("routes.rs"), "pub async fn public_get(");
        assert!(
            !body.contains("require_member_access"),
            "public_get must stay open — a guest needs the metadata to be shown the \
             sign-in gate for a members-only webinar"
        );
    }

    #[test]
    fn anonymous_money_spending_endpoints_rate_limit_per_ip() {
        // The second audit finding in this module: `tts_session` mints a real,
        // hour-long Cartesia token to anonymous callers but keyed its rate limit
        // on the WEBINAR CODE, not the caller. That let one attacker both farm
        // tokens and exhaust the shared bucket, denying TTS to the webinar's
        // legitimate viewers. Every other anonymous endpoint here keys on the IP.
        let body = body_of(include_str!("routes.rs"), "pub async fn tts_session(");
        assert!(
            body.contains("client_ip"),
            "tts_session must rate-limit per caller IP, not only per webinar code"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::HashSet;

    #[test]
    fn code_has_requested_length() {
        for len in [6, 10, 16] {
            assert_eq!(generate_webinar_code(len).chars().count(), len);
        }
    }

    #[test]
    fn code_uses_only_unambiguous_alphabet() {
        let allowed: HashSet<char> = CODE_ALPHABET.iter().map(|&b| b as char).collect();
        assert_eq!(CODE_ALPHABET.len(), 58, "base58 minus 0 O I l");
        for _ in 0..200 {
            for c in generate_webinar_code(12).chars() {
                assert!(allowed.contains(&c), "char {c} not in alphabet");
                assert!(!"0OIl".contains(c), "ambiguous char {c} leaked in");
            }
        }
    }

    #[test]
    fn codes_are_practically_unique() {
        let mut seen = HashSet::new();
        for _ in 0..5_000 {
            assert!(
                seen.insert(generate_webinar_code(10)),
                "unexpected collision in 5k len-10 codes"
            );
        }
    }

    #[tokio::test]
    async fn retry_regenerates_until_success() {
        #[derive(Debug)]
        struct Collision;
        let calls = Cell::new(0usize);
        let codes = std::cell::RefCell::new(Vec::new());
        let out: Result<String, Collision> = with_code_retry(
            10,
            8,
            |_| true,
            |code| {
                let n = calls.get();
                calls.set(n + 1);
                codes.borrow_mut().push(code.clone());
                async move {
                    if n < 2 {
                        Err(Collision)
                    } else {
                        Ok(code)
                    }
                }
            },
        )
        .await;
        assert!(out.is_ok(), "succeeds after retries");
        assert_eq!(calls.get(), 3, "two collisions then success");
        let c = codes.borrow();
        assert_ne!(c[0], c[1], "each attempt uses a fresh code");
    }

    #[tokio::test]
    async fn retry_gives_up_after_max() {
        #[derive(Debug, PartialEq)]
        struct Collision;
        let calls = Cell::new(0usize);
        let out: Result<(), Collision> = with_code_retry(
            10,
            3,
            |_| true,
            |_| {
                calls.set(calls.get() + 1);
                async { Err(Collision) }
            },
        )
        .await;
        assert_eq!(out, Err(Collision));
        assert_eq!(calls.get(), 3, "stops after max attempts");
    }

    #[tokio::test]
    async fn retry_does_not_retry_non_collision() {
        #[derive(Debug, PartialEq)]
        struct Fatal;
        let calls = Cell::new(0usize);
        let out: Result<(), Fatal> = with_code_retry(
            10,
            8,
            |_| false,
            |_| {
                calls.set(calls.get() + 1);
                async { Err(Fatal) }
            },
        )
        .await;
        assert_eq!(out, Err(Fatal));
        assert_eq!(calls.get(), 1, "a non-collision error is not retried");
    }

    #[test]
    fn join_url_uses_app_base_and_code() {
        assert_eq!(
            join_url("https://voxtranslate.app", "AbC123"),
            "https://voxtranslate.app/w/AbC123"
        );
        // A trailing slash on the base is trimmed, not doubled.
        assert_eq!(
            join_url("https://voxtranslate.app/", "x"),
            "https://voxtranslate.app/w/x"
        );
    }

    #[test]
    fn playback_url_points_at_hls_host() {
        let cfg = WebinarConfig::test_default();
        assert_eq!(
            playback_url(&cfg, "AbC123"),
            "https://hls.test/webinar/AbC123/index.m3u8"
        );
    }
}
