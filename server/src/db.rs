//! Database layer: Postgres connection pool, migrations, and row types.
//!
//! We use **runtime** SQLx (`sqlx::query`/`query_as`) rather than the
//! compile-time `query!` macros, so the crate builds with no live database and
//! CI stays simple. Migrations in `migrations/` are embedded at compile time and
//! run on startup via [`migrate`].

use std::time::Duration;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::postgres::PgPoolOptions;
use sqlx::FromRow;
use uuid::Uuid;

pub type Pool = sqlx::PgPool;

/// A row from `users`. `balance` is in USD credits (DECIMAL(10,6)).
#[derive(Debug, Clone, FromRow)]
pub struct User {
    pub id: Uuid,
    pub google_id: String,
    pub email: String,
    pub name: String,
    pub avatar_url: Option<String>,
    pub balance: Decimal,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    // Trust & safety / GDPR consent (added in migration 002).
    pub age_confirmed: bool,
    pub consent_tos_at: Option<DateTime<Utc>>,
    pub tos_version: Option<String>,
    pub banned_until: Option<DateTime<Utc>>,
    pub banned_reason: Option<String>,
    // Acquisition source (added in migration 007): the `?source`/`utm_source` the
    // user arrived with on first login. NULL = organic / pre-attribution.
    pub source: Option<String>,
    // UI locale captured at login (migration 026): the language the user uses the
    // product in, so outbound notifications can be localized to them. NULL = unknown.
    pub locale: Option<String>,
    // Cartesia Instant Voice Cloning id (spec 0108): the account's cloned voice, reused
    // across devices so we never re-prompt a user who already cloned. NULL until they
    // complete the pre-join voice-prep step. Every `users` query here is `SELECT *` /
    // `RETURNING *`, so adding this column-backed field maps cleanly.
    pub cartesia_voice_id: Option<String>,
    // Vox Voices preferences (migration 033): the chosen speech engine
    // ('auto'|'browser'|'vox') and Vox voice id, synced across the user's devices. The
    // browser-voice choice is device-local (not portable) and lives only in the client.
    pub tts_engine_pref: Option<String>,
    pub tts_voice_id: Option<String>,
}

/// A row from `chat_files` (spec 0018): metadata for a file attached to chat.
/// The bytes themselves live in Supabase Storage; `file_url` is the public URL.
#[derive(Debug, Clone, FromRow)]
pub struct ChatFile {
    pub id: Uuid,
    pub session_id: Uuid,
    pub room: String,
    pub sender_peer_id: String,
    pub sender_name: String,
    pub file_url: String,
    pub file_name: String,
    pub file_type: String,
    pub size_bytes: i64,
    pub created_at: DateTime<Utc>,
}

/// Persist a chat-file upload's metadata, returning the inserted row.
#[allow(clippy::too_many_arguments)]
pub async fn insert_chat_file(
    pool: &Pool,
    session_id: Uuid,
    room: &str,
    sender_peer_id: &str,
    sender_name: &str,
    file_url: &str,
    file_name: &str,
    file_type: &str,
    size_bytes: i64,
) -> Result<ChatFile, sqlx::Error> {
    sqlx::query_as(
        "INSERT INTO chat_files
            (session_id, room, sender_peer_id, sender_name,
             file_url, file_name, file_type, size_bytes)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         RETURNING *",
    )
    .bind(session_id)
    .bind(room)
    .bind(sender_peer_id)
    .bind(sender_name)
    .bind(file_url)
    .bind(file_name)
    .bind(file_type)
    .bind(size_bytes)
    .fetch_one(pool)
    .await
}

/// Open a connection pool to the given Postgres URL.
pub async fn connect(url: &str) -> Result<Pool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(10))
        .connect(url)
        .await
}

/// Run all pending migrations (idempotent — already-applied ones are skipped).
pub async fn migrate(pool: &Pool) -> Result<(), sqlx::Error> {
    sqlx::migrate!("./migrations").run(pool).await?;
    Ok(())
}

/// Insert a user bug report (spec 0071); returns the new row id. `user_id`/`email`
/// are set only for signed-in reporters; `page_url`/`user_agent` are triage context.
pub async fn insert_bug_report(
    pool: &Pool,
    message: &str,
    user_id: Option<Uuid>,
    email: Option<&str>,
    page_url: Option<&str>,
    user_agent: Option<&str>,
) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO bug_reports (message, user_id, email, page_url, user_agent)
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(message)
    .bind(user_id)
    .bind(email)
    .bind(page_url)
    .bind(user_agent)
    .fetch_one(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Migrate a real test DB and round-trip a user. Skipped when `DATABASE_URL`
    /// is unset (e.g. CI without a Postgres service); run locally against the
    /// Docker Postgres: `DATABASE_URL=postgres://postgres:postgres@localhost:5433/voxtest`.
    #[tokio::test]
    async fn migrate_and_round_trip_user() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skipping db test — no DATABASE_URL");
            return;
        };

        let pool = connect(&url).await.expect("connect");
        migrate(&pool).await.expect("migrate");

        // Unique identifiers so repeated runs don't collide on the UNIQUE cols.
        let gid = format!("g-{}", Uuid::new_v4());
        let email = format!("{}@example.com", Uuid::new_v4());

        let inserted: User = sqlx::query_as(
            "INSERT INTO users (google_id, email, name, avatar_url)
             VALUES ($1, $2, $3, $4) RETURNING *",
        )
        .bind(&gid)
        .bind(&email)
        .bind("Tester")
        .bind(Option::<String>::None)
        .fetch_one(&pool)
        .await
        .expect("insert user");

        assert_eq!(inserted.google_id, gid);
        assert_eq!(inserted.email, email);
        // New users start at exactly zero credits.
        assert_eq!(inserted.balance, Decimal::ZERO);

        let fetched: User = sqlx::query_as("SELECT * FROM users WHERE id = $1")
            .bind(inserted.id)
            .fetch_one(&pool)
            .await
            .expect("fetch user");
        assert_eq!(fetched.id, inserted.id);
        assert_eq!(fetched.email, email);
    }

    /// Round-trip a `bug_reports` row (spec 0071): a guest report inserts with a NULL
    /// user and defaults to status `received`. Skipped without `DATABASE_URL`.
    #[tokio::test]
    async fn insert_bug_report_guest_defaults_received() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skipping db test — no DATABASE_URL");
            return;
        };
        let pool = connect(&url).await.expect("connect");
        migrate(&pool).await.expect("migrate");

        let id = insert_bug_report(
            &pool,
            "the call dropped when I shared my screen",
            None,
            None,
            Some("/call/ABCD"),
            Some("Mozilla/5.0"),
        )
        .await
        .expect("insert bug report");

        let (status, user_id, msg): (String, Option<Uuid>, String) =
            sqlx::query_as("SELECT status, user_id, message FROM bug_reports WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .expect("fetch bug report");
        assert_eq!(status, "received");
        assert!(user_id.is_none());
        assert!(msg.contains("screen"));
    }

    /// Round-trip a `chat_files` row (spec 0018) against the real schema, proving
    /// the migration + `insert_chat_file` query agree. Needs a `call_sessions`
    /// parent row for the FK. Skipped without `DATABASE_URL`.
    #[tokio::test]
    async fn insert_and_read_chat_file() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skipping db test — no DATABASE_URL");
            return;
        };
        let pool = connect(&url).await.expect("connect");
        migrate(&pool).await.expect("migrate");

        // The chat_files FK references call_sessions(id), so create one first.
        let session_id = Uuid::new_v4();
        sqlx::query("INSERT INTO call_sessions (id, room) VALUES ($1, $2)")
            .bind(session_id)
            .bind("round-trip-room")
            .execute(&pool)
            .await
            .expect("insert call_session");

        let row = insert_chat_file(
            &pool,
            session_id,
            "round-trip-room",
            "peer-1",
            "Tester",
            "https://ref.supabase.co/storage/v1/object/public/chat-files/s/f.mp3",
            "memo.mp3",
            "audio/mpeg",
            12_345,
        )
        .await
        .expect("insert chat_file");

        assert_eq!(row.session_id, session_id);
        assert_eq!(row.file_name, "memo.mp3");
        assert_eq!(row.file_type, "audio/mpeg");
        assert_eq!(row.size_bytes, 12_345);
        assert_eq!(row.sender_peer_id, "peer-1");

        // Deleting the parent session cascades the file row away (GDPR lifecycle).
        sqlx::query("DELETE FROM call_sessions WHERE id = $1")
            .bind(session_id)
            .execute(&pool)
            .await
            .expect("delete session");
        let still: Option<ChatFile> = sqlx::query_as("SELECT * FROM chat_files WHERE id = $1")
            .bind(row.id)
            .fetch_optional(&pool)
            .await
            .expect("query chat_file");
        assert!(
            still.is_none(),
            "FK cascade removes the file with its session"
        );
    }

    /// F0-1: the `webinars` table (SPEC §7 subset) round-trips and enforces its
    /// constraints — UNIQUE `code`, CHECK on `status`/`tier`, FK to
    /// `organizations`, and ON DELETE CASCADE from the org. Skipped without
    /// `DATABASE_URL`; local: `DATABASE_URL=postgres://…@localhost:5432/voxtest`.
    #[tokio::test]
    async fn webinars_schema_constraints() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skipping db test — no DATABASE_URL");
            return;
        };
        let pool = connect(&url).await.expect("connect");
        migrate(&pool).await.expect("migrate");

        // Parents for the FKs: a host user and an org.
        let host_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (google_id, email, name) VALUES ($1,$2,$3) RETURNING id",
        )
        .bind(format!("g-{}", Uuid::new_v4()))
        .bind(format!("{}@x.com", Uuid::new_v4()))
        .bind("Host")
        .fetch_one(&pool)
        .await
        .expect("insert host");
        let org_id: Uuid = sqlx::query_scalar(
            "INSERT INTO organizations (name, slug, owner_id) VALUES ($1,$2,$3) RETURNING id",
        )
        .bind("Acme")
        .bind(format!("acme-{}", Uuid::new_v4().simple()))
        .bind(host_id)
        .fetch_one(&pool)
        .await
        .expect("insert org");

        let code = format!("w-{}", Uuid::new_v4().simple());

        // Happy path: a valid webinar inserts and defaults status/tier.
        let (_wid, status, tier): (Uuid, String, String) = sqlx::query_as(
            "INSERT INTO webinars (org_id, host_user_id, code, title, source_language)
             VALUES ($1,$2,$3,$4,$5) RETURNING id, status, tier",
        )
        .bind(org_id)
        .bind(host_id)
        .bind(&code)
        .bind("Launch webinar")
        .bind("en")
        .fetch_one(&pool)
        .await
        .expect("insert webinar");
        assert_eq!(status, "scheduled", "status defaults to scheduled");
        assert_eq!(tier, "enhanced", "tier defaults to enhanced");

        // Helper: attempt an insert with an overridable code/status/tier/org.
        async fn try_insert(
            pool: &Pool,
            org: Uuid,
            host: Uuid,
            code: &str,
            status: &str,
            tier: &str,
        ) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
            sqlx::query(
                "INSERT INTO webinars
                    (org_id, host_user_id, code, title, source_language, status, tier)
                 VALUES ($1,$2,$3,$4,'en',$5,$6)",
            )
            .bind(org)
            .bind(host)
            .bind(code)
            .bind(code) // reuse code as title, irrelevant here
            .bind(status)
            .bind(tier)
            .execute(pool)
            .await
        }

        let uniq = || format!("w-{}", Uuid::new_v4().simple());

        // UNIQUE(code): a second webinar with the same code is rejected.
        assert!(
            try_insert(&pool, org_id, host_id, &code, "scheduled", "enhanced")
                .await
                .is_err(),
            "duplicate code must violate UNIQUE"
        );
        // CHECK(status): an unknown status is rejected.
        assert!(
            try_insert(&pool, org_id, host_id, &uniq(), "bogus", "enhanced")
                .await
                .is_err(),
            "unknown status must violate CHECK"
        );
        // CHECK(tier): an unknown tier is rejected.
        assert!(
            try_insert(&pool, org_id, host_id, &uniq(), "scheduled", "gold")
                .await
                .is_err(),
            "unknown tier must violate CHECK"
        );
        // FK(org_id): a non-existent org is rejected.
        assert!(
            try_insert(
                &pool,
                Uuid::new_v4(),
                host_id,
                &uniq(),
                "scheduled",
                "enhanced"
            )
            .await
            .is_err(),
            "unknown org_id must violate FK"
        );

        // ON DELETE CASCADE: dropping the org removes its webinars.
        sqlx::query("DELETE FROM organizations WHERE id = $1")
            .bind(org_id)
            .execute(&pool)
            .await
            .expect("delete org");
        let survivors: i64 = sqlx::query_scalar("SELECT count(*) FROM webinars WHERE org_id = $1")
            .bind(org_id)
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(survivors, 0, "org delete cascades its webinars");
        let _ = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(host_id)
            .execute(&pool)
            .await;
    }
}
