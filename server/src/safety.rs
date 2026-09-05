//! Trust & safety + GDPR data operations: abuse reports, consent (age + ToS),
//! bans, and the user's right to export / delete their data.

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use crate::db::Pool;
use crate::storage::SupabaseStorage;

/// One JSON document with everything we hold on a user (GDPR data portability).
/// Built entirely in Postgres (`json_build_object`) and returned as text so we
/// don't need sqlx's json feature.
const EXPORT_SQL: &str = "
SELECT json_build_object(
  'profile', (SELECT row_to_json(u) FROM (
      SELECT id, email, name, avatar_url, balance, created_at,
             age_confirmed, consent_tos_at, tos_version
      FROM users WHERE id = $1) u),
  'credit_transactions', (SELECT coalesce(json_agg(t), '[]') FROM (
      SELECT amount, kind, balance_after, description, created_at
      FROM credit_transactions WHERE user_id = $1 ORDER BY created_at) t),
  'usage_sessions', (SELECT coalesce(json_agg(s), '[]') FROM (
      SELECT room, speaking_seconds, cost, started_at, ended_at
      FROM usage_sessions WHERE user_id = $1 ORDER BY started_at) s),
  'reports_filed', (SELECT coalesce(json_agg(r), '[]') FROM (
      SELECT room, reason, created_at
      FROM reports WHERE reporter_user_id = $1 ORDER BY created_at) r),
  'call_sessions', (SELECT coalesce(json_agg(c), '[]') FROM (
      SELECT cs.room, cs.started_at, cs.ended_at
      FROM call_sessions cs
      WHERE EXISTS (SELECT 1 FROM session_participants sp
                    WHERE sp.session_id = cs.id AND sp.user_id = $1)
      ORDER BY cs.started_at) c),
  'transcript_events', (SELECT coalesce(json_agg(e), '[]') FROM (
      SELECT te.event_type, te.original_text, te.original_lang, te.ts
      FROM transcript_events te
      WHERE te.speaker_user_id = $1 ORDER BY te.ts) e)
)::text";

/// Why an erasure could not be completed. Erasure spans two systems (Postgres and
/// Supabase Storage), so it needs an error that can name either — the previous
/// `sqlx::Error` return could only describe half of the operation.
#[derive(Debug)]
pub enum EraseError {
    /// A database statement failed. Nothing was committed.
    Db(sqlx::Error),
    /// An object could not be deleted from storage. The account is untouched.
    Storage(String),
    /// The user has storage objects but no storage client is configured, so the
    /// bytes cannot be reached. Refusing beats deleting the rows that point at
    /// them — see [`SafetyService::delete_user`].
    StorageUnavailable,
}

impl std::fmt::Display for EraseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(e) => write!(f, "erasure database step failed: {e}"),
            Self::Storage(e) => write!(f, "erasure storage step failed: {e}"),
            Self::StorageUnavailable => write!(
                f,
                "erasure aborted: user has storage objects but storage is not configured"
            ),
        }
    }
}

impl std::error::Error for EraseError {}

impl From<sqlx::Error> for EraseError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

/// Database operations for moderation + GDPR. Cheap to clone.
#[derive(Clone)]
pub struct SafetyService {
    pool: Pool,
    /// The `chat-files` bucket client, when Supabase Storage is configured.
    /// `None` on a deploy without `SUPABASE_*`, where uploads are disabled and a
    /// user therefore cannot have objects.
    files_storage: Option<SupabaseStorage>,
}

impl SafetyService {
    pub fn new(pool: Pool) -> Self {
        Self {
            pool,
            files_storage: None,
        }
    }

    /// Attach the chat-files bucket so [`Self::delete_user`] can erase the bytes a
    /// user uploaded, not just the rows pointing at them.
    pub fn with_files_storage(mut self, storage: Option<SupabaseStorage>) -> Self {
        self.files_storage = storage;
        self
    }

    /// File an abuse report against a peer in a room.
    pub async fn record_report(
        &self,
        reporter: Uuid,
        room: &str,
        reported_peer_id: Option<&str>,
        reported_name: Option<&str>,
        reason: &str,
        transcript_excerpt: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO reports
                 (reporter_user_id, room, reported_peer_id, reported_name, reason, transcript_excerpt)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(reporter)
        .bind(room)
        .bind(reported_peer_id)
        .bind(reported_name)
        .bind(reason)
        .bind(transcript_excerpt)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record that the user confirmed they're of age and accepted the given
    /// ToS/Privacy version.
    pub async fn set_consent(&self, user_id: Uuid, tos_version: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE users
             SET age_confirmed = TRUE, consent_tos_at = now(), tos_version = $2, updated_at = now()
             WHERE id = $1",
        )
        .bind(user_id)
        .bind(tos_version)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// `Some(reason)` if the user is currently banned, else `None`.
    pub async fn is_banned(&self, user_id: Uuid) -> Result<Option<String>, sqlx::Error> {
        let row: Option<(Option<DateTime<Utc>>, Option<String>)> =
            sqlx::query_as("SELECT banned_until, banned_reason FROM users WHERE id = $1")
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(match row {
            Some((Some(until), reason)) if until > Utc::now() => {
                Some(reason.unwrap_or_else(|| "banned".to_string()))
            }
            _ => None,
        })
    }

    /// `true` once the user confirmed they're 18+ and accepted the ToS/Privacy
    /// (mirrors `consent_given` in [`crate::auth`]). Intentionally does NOT require the
    /// *current* ToS version, so users who consented under an earlier version still pass.
    pub async fn has_consented(&self, user_id: Uuid) -> Result<bool, sqlx::Error> {
        let row: Option<(bool, Option<DateTime<Utc>>)> =
            sqlx::query_as("SELECT age_confirmed, consent_tos_at FROM users WHERE id = $1")
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(matches!(row, Some((true, Some(_)))))
    }

    /// Ban a user. `days = None` is effectively permanent.
    pub async fn ban_user(
        &self,
        user_id: Uuid,
        reason: &str,
        days: Option<i64>,
    ) -> Result<(), sqlx::Error> {
        let until = Utc::now() + Duration::days(days.unwrap_or(365_000));
        sqlx::query("UPDATE users SET banned_until = $2, banned_reason = $3 WHERE id = $1")
            .bind(user_id)
            .bind(until)
            .bind(reason)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Lift a ban (clear `banned_until`/`banned_reason`).
    pub async fn unban_user(&self, user_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE users SET banned_until = NULL, banned_reason = NULL WHERE id = $1")
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// All data held on the user, as one JSON document (GDPR portability).
    pub async fn export_user_data(&self, user_id: Uuid) -> Result<serde_json::Value, sqlx::Error> {
        let json: String = sqlx::query_scalar(EXPORT_SQL)
            .bind(user_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(serde_json::from_str(&json).unwrap_or(serde_json::Value::Null))
    }

    /// Erase the user: their Supabase Storage objects first, then the account and
    /// every linked row (FKs cascade). GDPR right to erasure.
    ///
    /// **Ordering — storage before database, deliberately.** The row is the only
    /// durable handle on the object (`file_url` is an expiring signed URL, not a
    /// path), so deleting the row first would strand bytes nothing could ever find
    /// again. Clearing storage first means a failure anywhere leaves the account
    /// fully intact and the caller can simply retry: `SupabaseStorage::delete`
    /// treats a 404 as success, so repeating a partially-completed erasure is
    /// safe. This is the same rationale as the retention sweep
    /// (`business::retention::sweep_once`), which deletes the object before
    /// clearing its pointer for exactly this reason.
    ///
    /// **Scope.** Only artifacts authored by this user and sharing the lifecycle of
    /// their utterances are erased — today that is `chat_files`, whose rows cascade
    /// like `transcript_events.speaker_user_id` (migration 004). Org-owned
    /// artifacts are deliberately excluded: cloud recordings are multi-party, and
    /// `project_voice_messages` follows the rule migration 016 states for
    /// projects — "keep the project if the creator's personal account is deleted".
    /// For those the organisation is the controller, and erasure belongs to a
    /// tenant-admin path, not to an individual's account deletion.
    pub async fn delete_user(&self, user_id: Uuid) -> Result<(), EraseError> {
        // (1) Collect the object paths while the rows still link them to the user.
        //     Pre-053 uploads carry a NULL path: they are unattributable, so they
        //     are skipped rather than guessed at.
        let object_paths: Vec<String> = sqlx::query_scalar(
            "SELECT object_path FROM chat_files
             WHERE user_id = $1 AND object_path IS NOT NULL AND object_path <> ''",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;

        // (2) Bytes exist but nothing can reach them — refuse. Deleting the rows
        //     here would destroy the only pointer and guarantee a permanent leak.
        if !object_paths.is_empty() {
            let Some(storage) = self.files_storage.as_ref() else {
                tracing::error!(
                    %user_id,
                    objects = object_paths.len(),
                    "erasure aborted: storage objects present but no storage client configured"
                );
                return Err(EraseError::StorageUnavailable);
            };

            // (3) Clear storage. The first failure aborts with the database
            //     untouched, so the operation stays retryable as a whole.
            for path in &object_paths {
                storage.delete(path).await.map_err(|e| {
                    tracing::error!(%user_id, path, "erasure storage delete failed: {e}");
                    EraseError::Storage(e)
                })?;
            }
        }

        // (4) Only now drop the account. The cascade takes chat_files with it.
        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(user_id)
            .execute(&self.pool)
            .await?;

        tracing::info!(
            %user_id,
            objects_deleted = object_paths.len(),
            "erased account and its storage objects"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn service() -> Option<SafetyService> {
        let url = std::env::var("DATABASE_URL").ok()?;
        let pool = crate::db::connect(&url).await.ok()?;
        crate::db::migrate(&pool).await.ok()?;
        Some(SafetyService::new(pool))
    }

    async fn make_user(svc: &SafetyService) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO users (google_id, email, name, balance)
             VALUES ($1, $2, 'T', 1.0) RETURNING id",
        )
        .bind(format!("g-{}", Uuid::new_v4()))
        .bind(format!("{}@x.com", Uuid::new_v4()))
        .fetch_one(&svc.pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn consent_ban_report_export_delete() {
        let Some(svc) = service().await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        let uid = make_user(&svc).await;

        // Consent.
        svc.set_consent(uid, "v1").await.unwrap();
        let (age, at): (bool, Option<DateTime<Utc>>) =
            sqlx::query_as("SELECT age_confirmed, consent_tos_at FROM users WHERE id = $1")
                .bind(uid)
                .fetch_one(&svc.pool)
                .await
                .unwrap();
        assert!(age && at.is_some());

        // Not banned, then banned, then expired ban.
        assert!(svc.is_banned(uid).await.unwrap().is_none());
        svc.ban_user(uid, "abuse", Some(7)).await.unwrap();
        assert_eq!(svc.is_banned(uid).await.unwrap().as_deref(), Some("abuse"));
        svc.ban_user(uid, "old", Some(-1)).await.unwrap(); // already expired
        assert!(svc.is_banned(uid).await.unwrap().is_none());

        // Report + export sees it.
        svc.record_report(
            uid,
            "room1",
            Some("peer9"),
            Some("Bob"),
            "harassment",
            Some("bad text"),
        )
        .await
        .unwrap();

        // Seed a call session the user took part in, with one spoken event,
        // so the export's transcript sections have something to show.
        let sid = Uuid::new_v4();
        sqlx::query("INSERT INTO call_sessions (id, room) VALUES ($1, 'room-gdpr')")
            .bind(sid)
            .execute(&svc.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO session_participants (session_id, peer_id, user_id, name, lang)
             VALUES ($1, 'peer-1', $2, 'T', 'it')",
        )
        .bind(sid)
        .bind(uid)
        .execute(&svc.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO transcript_events
                 (session_id, event_type, speaker_peer_id, speaker_user_id, speaker_name,
                  original_text, original_lang, ts)
             VALUES ($1, 'speech', 'peer-1', $2, 'T', 'ciao', 'it', now())",
        )
        .bind(sid)
        .bind(uid)
        .execute(&svc.pool)
        .await
        .unwrap();

        let export = svc.export_user_data(uid).await.unwrap();
        assert!(export["profile"]["email"].is_string());
        assert_eq!(export["reports_filed"][0]["reason"], "harassment");
        assert_eq!(export["call_sessions"][0]["room"], "room-gdpr");
        assert_eq!(export["transcript_events"][0]["original_text"], "ciao");

        // Delete cascades.
        svc.delete_user(uid).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE id = $1")
            .bind(uid)
            .fetch_one(&svc.pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        let reports: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM reports WHERE reporter_user_id = $1")
                .bind(uid)
                .fetch_one(&svc.pool)
                .await
                .unwrap();
        assert_eq!(reports, 0, "reports cascade-deleted");
        let participants: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM session_participants WHERE session_id = $1 AND user_id = $2",
        )
        .bind(sid)
        .bind(uid)
        .fetch_one(&svc.pool)
        .await
        .unwrap();
        assert_eq!(participants, 0, "participant rows cascade-deleted");
        let spoken: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM transcript_events WHERE speaker_user_id = $1")
                .bind(uid)
                .fetch_one(&svc.pool)
                .await
                .unwrap();
        assert_eq!(spoken, 0, "spoken transcript events cascade-deleted");
    }
}
