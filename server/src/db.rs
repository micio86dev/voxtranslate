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
}
