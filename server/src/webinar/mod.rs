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

pub mod guest;
pub mod routes;

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
    pub project_id: Option<Uuid>,
    pub google_event_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
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
    pub scheduled_start: Option<DateTime<Utc>>,
    pub scheduled_end: Option<DateTime<Utc>>,
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
             record_video, record_transcript, voice_clone, scheduled_start, scheduled_end)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
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
    .bind(new.scheduled_start)
    .bind(new.scheduled_end)
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
