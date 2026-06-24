//! Transcript persistence: call sessions, their participants, and the
//! speech/chat events captured during a call (spec 0009).
//!
//! Events are fire-and-forget queued onto an unbounded channel and batch
//! inserted by a background recorder task (every [`BATCH_INTERVAL`] or
//! [`BATCH_MAX`] events, whichever first), so the hot subtitle/chat paths never
//! wait on Postgres. [`TranscriptService::flush`] is a barrier used by the
//! export endpoints to kill the leave-then-download race.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, oneshot};
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

use crate::db::Pool;

/// Flush the recorder buffer at least this often.
const BATCH_INTERVAL: Duration = Duration::from_secs(3);
/// ...or as soon as this many events are buffered.
const BATCH_MAX: usize = 64;

/// What kind of utterance a transcript event records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Speech,
    Chat,
}

impl EventKind {
    fn as_str(self) -> &'static str {
        match self {
            EventKind::Speech => "speech",
            EventKind::Chat => "chat",
        }
    }
}

/// One captured utterance (finalized speech transcript or chat message),
/// with the full translation fan-out map.
#[derive(Debug)]
pub struct TranscriptEvent {
    pub session_id: Uuid,
    pub kind: EventKind,
    pub speaker_peer_id: String,
    /// `None` for guests. Cascades on account deletion (GDPR).
    pub speaker_user_id: Option<Uuid>,
    pub speaker_name: String,
    pub original_text: String,
    pub original_lang: String,
    /// `{ lang: text }` for every target language in the room at capture time.
    pub translations: HashMap<String, String>,
    /// When the words were spoken/sent (captured *before* translation).
    pub ts: DateTime<Utc>,
}

enum RecorderMsg {
    Event(Box<TranscriptEvent>),
    /// Barrier: everything queued before this is in the DB when it acks.
    Flush(oneshot::Sender<()>),
}

/// Whether a user may download a session's transcript (spec 0009 R4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAccess {
    /// No such session (or it was purged).
    NotFound,
    /// The session exists but the user wasn't a participant.
    Forbidden,
    /// The user participated — export away.
    Ok,
}

/// One row of `GET /api/sessions`: a call the user took part in.
#[derive(Debug, serde::Serialize)]
pub struct SessionSummary {
    pub id: Uuid,
    pub room: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub event_count: i64,
}

/// A pinned moment in a call (spec 0013). `mine` lets the UI show the delete
/// button only to the owner; creators are exposed by display name only.
#[derive(Debug, serde::Serialize)]
pub struct Bookmark {
    pub id: Uuid,
    pub ts: DateTime<Utc>,
    pub label: Option<String>,
    pub by: String,
    pub mine: bool,
}

/// Outcome of an owner-gated bookmark mutation (update / delete).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BookmarkMutation {
    NotFound,
    /// The bookmark exists but belongs to another participant.
    Forbidden,
    Ok,
}

/// Bookmark shape embedded in [`TranscriptExport`] documents.
#[derive(Debug, serde::Serialize)]
pub struct ExportBookmark {
    pub ts: DateTime<Utc>,
    pub label: Option<String>,
    pub by: String,
}

/// The downloadable transcript document (JSON body / PDF input). Participants
/// are identified by their *peer* ids only — user UUIDs never leave the server.
#[derive(Debug, serde::Serialize)]
pub struct TranscriptExport {
    pub session: ExportSession,
    pub events: Vec<ExportEvent>,
    pub bookmarks: Vec<ExportBookmark>,
    pub exported_at: DateTime<Utc>,
}

#[derive(Debug, serde::Serialize)]
pub struct ExportSession {
    pub id: Uuid,
    pub room_name: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    /// Until `ended_at` for finished calls, until `exported_at` for live ones.
    pub duration_seconds: i64,
    pub participants: Vec<ExportParticipant>,
}

#[derive(Debug, serde::Serialize)]
pub struct ExportParticipant {
    /// The peer id (never the user's UUID).
    pub id: String,
    pub name: String,
    pub language: String,
}

#[derive(Debug, serde::Serialize)]
pub struct ExportEvent {
    #[serde(rename = "type")]
    pub kind: String,
    pub ts: DateTime<Utc>,
    pub speaker_id: String,
    pub speaker_name: String,
    pub lang: String,
    pub original: String,
    pub translations: HashMap<String, String>,
}

/// Row shape of the `list_sessions` query.
type SessionRow = (Uuid, String, DateTime<Utc>, Option<DateTime<Utc>>, i64);
/// Row shape of the `list_bookmarks` query: `(id, ts, label, owner_name, mine)`.
type BookmarkRow = (Uuid, DateTime<Utc>, Option<String>, String, bool);
/// Row shape of the `export` events query: `(event_type, speaker_peer_id,
/// speaker_name, original_text, original_lang, translations, ts)`.
type EventRow = (
    String,
    String,
    String,
    String,
    String,
    serde_json::Value,
    DateTime<Utc>,
);

/// Transcript operations against the database. Cheap to clone (pool is an
/// `Arc`, events go through a shared channel).
#[derive(Clone)]
pub struct TranscriptService {
    pool: Pool,
    tx: mpsc::UnboundedSender<RecorderMsg>,
}

impl TranscriptService {
    /// Create the service and spawn its background recorder task.
    pub fn new(pool: Pool) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(run_recorder(pool.clone(), rx));
        Self { pool, tx }
    }

    /// Ensure the session row exists. Idempotent — every joiner calls it, the
    /// first one wins (`ON CONFLICT DO NOTHING`).
    ///
    /// The call inherits any business binding for its room code (spec 0106): if a
    /// `room_business_bindings` row exists, the new `call_sessions` row is created
    /// with that org/project and recording intent. Consumer rooms have no binding,
    /// so the sub-selects return NULL/false and the behaviour is unchanged.
    pub async fn session_started(&self, session_id: Uuid, room: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO call_sessions (id, room, org_id, project_id, cloud_recording_enabled)
             VALUES (
                 $1, $2,
                 (SELECT org_id FROM room_business_bindings WHERE room = $2),
                 (SELECT project_id FROM room_business_bindings WHERE room = $2),
                 COALESCE(
                     (SELECT cloud_recording_enabled FROM room_business_bindings WHERE room = $2),
                     FALSE
                 )
             )
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(session_id)
        .bind(room)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record a participant joining; returns their row id for `participant_left`.
    pub async fn participant_joined(
        &self,
        session_id: Uuid,
        peer_id: &str,
        user_id: Option<Uuid>,
        name: &str,
        lang: &str,
    ) -> Result<Uuid, sqlx::Error> {
        sqlx::query_scalar(
            "INSERT INTO session_participants (session_id, peer_id, user_id, name, lang)
             VALUES ($1, $2, $3, $4, $5) RETURNING id",
        )
        .bind(session_id)
        .bind(peer_id)
        .bind(user_id)
        .bind(name)
        .bind(lang)
        .fetch_one(&self.pool)
        .await
    }

    /// Update a participant's language once auto-detect resolves it (or the user
    /// corrects it mid-call) so exports/PDF defaults reflect what was spoken.
    pub async fn update_participant_lang(
        &self,
        participant_id: Uuid,
        lang: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE session_participants SET lang = $2 WHERE id = $1")
            .bind(participant_id)
            .bind(lang)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Stamp a participant's departure.
    pub async fn participant_left(&self, participant_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE session_participants SET left_at = now() WHERE id = $1 AND left_at IS NULL",
        )
        .bind(participant_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Queue an event for batched insertion. Fire-and-forget: never blocks the
    /// subtitle/chat hot path.
    pub fn record(&self, event: TranscriptEvent) {
        let _ = self.tx.send(RecorderMsg::Event(Box::new(event)));
    }

    /// Barrier: resolves once everything queued so far is in the database.
    pub async fn flush(&self) {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.tx.send(RecorderMsg::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.await;
        }
    }

    /// End of call: flush pending events, stamp `ended_at`, and purge the
    /// session entirely when no signed-in user took part — guests can never
    /// download, so keeping their words would be pure liability (spec 0009 R5).
    pub async fn finalize_session(&self, session_id: Uuid) -> Result<(), sqlx::Error> {
        self.flush().await;
        sqlx::query("UPDATE call_sessions SET ended_at = now() WHERE id = $1 AND ended_at IS NULL")
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "DELETE FROM call_sessions cs
             WHERE cs.id = $1
               AND NOT EXISTS (
                   SELECT 1 FROM session_participants sp
                   WHERE sp.session_id = cs.id AND sp.user_id IS NOT NULL
               )",
        )
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The user's recent call sessions (most recent first), with how many
    /// transcript events each holds — for the Transcripts tab.
    pub async fn list_sessions(
        &self,
        user_id: Uuid,
        limit: i64,
    ) -> Result<Vec<SessionSummary>, sqlx::Error> {
        let rows: Vec<SessionRow> = sqlx::query_as(
            "SELECT cs.id, cs.room, cs.started_at, cs.ended_at,
                    (SELECT count(*) FROM transcript_events te
                     WHERE te.session_id = cs.id) AS event_count
             FROM call_sessions cs
             WHERE EXISTS (SELECT 1 FROM session_participants sp
                           WHERE sp.session_id = cs.id AND sp.user_id = $1)
             ORDER BY cs.started_at DESC
             LIMIT $2",
        )
        .bind(user_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(id, room, started_at, ended_at, event_count)| SessionSummary {
                    id,
                    room,
                    started_at,
                    ended_at,
                    event_count,
                },
            )
            .collect())
    }

    /// May `user_id` download this session's transcript? Only participants can.
    pub async fn access(
        &self,
        session_id: Uuid,
        user_id: Uuid,
    ) -> Result<SessionAccess, sqlx::Error> {
        let participated: Option<bool> = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM session_participants sp
                            WHERE sp.session_id = cs.id AND sp.user_id = $2)
             FROM call_sessions cs WHERE cs.id = $1",
        )
        .bind(session_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(match participated {
            None => SessionAccess::NotFound,
            Some(false) => SessionAccess::Forbidden,
            Some(true) => SessionAccess::Ok,
        })
    }

    /// The language `user_id` joined this session with (first join wins) —
    /// used as the PDF's default translation language.
    pub async fn participant_lang(
        &self,
        session_id: Uuid,
        user_id: Uuid,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT lang FROM session_participants
             WHERE session_id = $1 AND user_id = $2
             ORDER BY joined_at LIMIT 1",
        )
        .bind(session_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
    }

    /// Pin a moment (spec 0013). `ts` defaults to "now" — the in-call 🔖 button
    /// posts instantly and labels afterwards. Caller has already passed
    /// [`Self::access`].
    pub async fn add_bookmark(
        &self,
        session_id: Uuid,
        user_id: Uuid,
        ts: Option<DateTime<Utc>>,
        label: Option<&str>,
    ) -> Result<Bookmark, sqlx::Error> {
        let (id, ts, label): (Uuid, DateTime<Utc>, Option<String>) = sqlx::query_as(
            "INSERT INTO transcript_bookmarks (session_id, user_id, ts, label)
             VALUES ($1, $2, COALESCE($3, now()), $4)
             RETURNING id, ts, label",
        )
        .bind(session_id)
        .bind(user_id)
        .bind(ts)
        .bind(label)
        .fetch_one(&self.pool)
        .await?;
        let by: String = sqlx::query_scalar("SELECT name FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(Bookmark {
            id,
            ts,
            label,
            by,
            mine: true,
        })
    }

    /// Every participant's bookmarks for a session, chronological.
    pub async fn list_bookmarks(
        &self,
        session_id: Uuid,
        viewer: Uuid,
    ) -> Result<Vec<Bookmark>, sqlx::Error> {
        let rows: Vec<BookmarkRow> = sqlx::query_as(
            "SELECT b.id, b.ts, b.label, u.name, b.user_id = $2
             FROM transcript_bookmarks b JOIN users u ON u.id = b.user_id
             WHERE b.session_id = $1 ORDER BY b.ts",
        )
        .bind(session_id)
        .bind(viewer)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, ts, label, by, mine)| Bookmark {
                id,
                ts,
                label,
                by,
                mine,
            })
            .collect())
    }

    /// Relabel a bookmark — owner only.
    pub async fn update_bookmark_label(
        &self,
        session_id: Uuid,
        bookmark_id: Uuid,
        user_id: Uuid,
        label: Option<&str>,
    ) -> Result<BookmarkMutation, sqlx::Error> {
        let done = sqlx::query(
            "UPDATE transcript_bookmarks SET label = $4
             WHERE id = $1 AND session_id = $2 AND user_id = $3",
        )
        .bind(bookmark_id)
        .bind(session_id)
        .bind(user_id)
        .bind(label)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if done == 1 {
            return Ok(BookmarkMutation::Ok);
        }
        self.bookmark_gate(session_id, bookmark_id).await
    }

    /// Remove a bookmark — owner only.
    pub async fn delete_bookmark(
        &self,
        session_id: Uuid,
        bookmark_id: Uuid,
        user_id: Uuid,
    ) -> Result<BookmarkMutation, sqlx::Error> {
        let done = sqlx::query(
            "DELETE FROM transcript_bookmarks
             WHERE id = $1 AND session_id = $2 AND user_id = $3",
        )
        .bind(bookmark_id)
        .bind(session_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if done == 1 {
            return Ok(BookmarkMutation::Ok);
        }
        self.bookmark_gate(session_id, bookmark_id).await
    }

    /// 404-vs-403 for a failed owner-gated mutation.
    async fn bookmark_gate(
        &self,
        session_id: Uuid,
        bookmark_id: Uuid,
    ) -> Result<BookmarkMutation, sqlx::Error> {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM transcript_bookmarks
                            WHERE id = $1 AND session_id = $2)",
        )
        .bind(bookmark_id)
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(if exists {
            BookmarkMutation::Forbidden
        } else {
            BookmarkMutation::NotFound
        })
    }

    /// Assemble the full transcript document for a session, or `None` if the
    /// session doesn't exist (check [`Self::access`] first for 403 vs 404).
    pub async fn export(&self, session_id: Uuid) -> Result<Option<TranscriptExport>, sqlx::Error> {
        let session: Option<(String, DateTime<Utc>, Option<DateTime<Utc>>)> =
            sqlx::query_as("SELECT room, started_at, ended_at FROM call_sessions WHERE id = $1")
                .bind(session_id)
                .fetch_optional(&self.pool)
                .await?;
        let Some((room_name, started_at, ended_at)) = session else {
            return Ok(None);
        };

        let participant_rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT peer_id, name, lang FROM session_participants
             WHERE session_id = $1 ORDER BY joined_at",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        // Rejoining with the same peer id creates one row per join; keep the first.
        let mut seen = std::collections::HashSet::new();
        let participants = participant_rows
            .into_iter()
            .filter(|(peer_id, _, _)| seen.insert(peer_id.clone()))
            .map(|(id, name, language)| ExportParticipant { id, name, language })
            .collect();

        let event_rows: Vec<EventRow> = sqlx::query_as(
            "SELECT event_type, speaker_peer_id, speaker_name, original_text,
                    original_lang, translations, ts
             FROM transcript_events WHERE session_id = $1 ORDER BY ts",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        let events = event_rows
            .into_iter()
            .map(
                |(kind, speaker_id, speaker_name, original, lang, translations, ts)| ExportEvent {
                    kind,
                    ts,
                    speaker_id,
                    speaker_name,
                    lang,
                    original,
                    translations: serde_json::from_value(translations).unwrap_or_default(),
                },
            )
            .collect();

        let bookmark_rows: Vec<(DateTime<Utc>, Option<String>, String)> = sqlx::query_as(
            "SELECT b.ts, b.label, u.name
             FROM transcript_bookmarks b JOIN users u ON u.id = b.user_id
             WHERE b.session_id = $1 ORDER BY b.ts",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        let bookmarks = bookmark_rows
            .into_iter()
            .map(|(ts, label, by)| ExportBookmark { ts, label, by })
            .collect();

        let exported_at = Utc::now();
        let duration_seconds = (ended_at.unwrap_or(exported_at) - started_at)
            .num_seconds()
            .max(0);
        Ok(Some(TranscriptExport {
            session: ExportSession {
                id: session_id,
                room_name,
                started_at,
                ended_at,
                duration_seconds,
                participants,
            },
            events,
            bookmarks,
            exported_at,
        }))
    }
}

/// Background recorder: buffers events and batch-inserts on a 3s tick, when the
/// buffer reaches [`BATCH_MAX`], on a flush barrier, or on channel close.
async fn run_recorder(pool: Pool, mut rx: mpsc::UnboundedReceiver<RecorderMsg>) {
    let mut buf: Vec<TranscriptEvent> = Vec::new();
    let mut tick = tokio::time::interval(BATCH_INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(RecorderMsg::Event(ev)) => {
                    buf.push(*ev);
                    if buf.len() >= BATCH_MAX {
                        insert_batch(&pool, &mut buf).await;
                    }
                }
                Some(RecorderMsg::Flush(ack)) => {
                    insert_batch(&pool, &mut buf).await;
                    let _ = ack.send(());
                }
                None => {
                    insert_batch(&pool, &mut buf).await;
                    break;
                }
            },
            _ = tick.tick() => insert_batch(&pool, &mut buf).await,
        }
    }
}

/// One multi-row `INSERT … SELECT FROM UNNEST` for the whole buffer. The inner
/// join to `call_sessions` silently drops late events whose session was already
/// purged (guest-only finalize racing the last batch) instead of FK-erroring.
async fn insert_batch(pool: &Pool, buf: &mut Vec<TranscriptEvent>) {
    if buf.is_empty() {
        return;
    }
    let events = std::mem::take(buf);
    let n = events.len();

    let mut session_ids = Vec::with_capacity(n);
    let mut kinds = Vec::with_capacity(n);
    let mut peer_ids = Vec::with_capacity(n);
    let mut user_ids = Vec::with_capacity(n);
    let mut names = Vec::with_capacity(n);
    let mut originals = Vec::with_capacity(n);
    let mut langs = Vec::with_capacity(n);
    let mut translations = Vec::with_capacity(n);
    let mut timestamps = Vec::with_capacity(n);
    for ev in events {
        session_ids.push(ev.session_id);
        kinds.push(ev.kind.as_str());
        peer_ids.push(ev.speaker_peer_id);
        user_ids.push(ev.speaker_user_id);
        names.push(ev.speaker_name);
        originals.push(ev.original_text);
        langs.push(ev.original_lang);
        translations.push(
            serde_json::to_value(ev.translations)
                .unwrap_or_else(|_| serde_json::Value::Object(Default::default())),
        );
        timestamps.push(ev.ts);
    }

    let res = sqlx::query(
        "INSERT INTO transcript_events
             (session_id, event_type, speaker_peer_id, speaker_user_id, speaker_name,
              original_text, original_lang, translations, ts)
         SELECT u.session_id, u.event_type, u.speaker_peer_id, u.speaker_user_id,
                u.speaker_name, u.original_text, u.original_lang, u.translations, u.ts
         FROM UNNEST($1::uuid[], $2::text[], $3::text[], $4::uuid[], $5::text[],
                     $6::text[], $7::text[], $8::jsonb[], $9::timestamptz[])
              AS u(session_id, event_type, speaker_peer_id, speaker_user_id, speaker_name,
                   original_text, original_lang, translations, ts)
         JOIN call_sessions cs ON cs.id = u.session_id",
    )
    .bind(&session_ids)
    .bind(&kinds)
    .bind(&peer_ids)
    .bind(&user_ids)
    .bind(&names)
    .bind(&originals)
    .bind(&langs)
    .bind(&translations)
    .bind(&timestamps)
    .execute(pool)
    .await;

    if let Err(e) = res {
        tracing::error!("transcript batch insert failed ({n} events): {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Connect + migrate the test DB, or `None` when `DATABASE_URL` is unset.
    async fn test_service() -> Option<TranscriptService> {
        let url = std::env::var("DATABASE_URL").ok()?;
        let pool = crate::db::connect(&url).await.ok()?;
        crate::db::migrate(&pool).await.ok()?;
        Some(TranscriptService::new(pool))
    }

    /// Insert a bare user, returning its id.
    async fn make_user(svc: &TranscriptService) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO users (google_id, email, name, balance)
             VALUES ($1, $2, 'T', 0) RETURNING id",
        )
        .bind(format!("g-{}", Uuid::new_v4()))
        .bind(format!("{}@x.com", Uuid::new_v4()))
        .fetch_one(&svc.pool)
        .await
        .unwrap()
    }

    fn speech_event(session_id: Uuid, user_id: Option<Uuid>, text: &str) -> TranscriptEvent {
        TranscriptEvent {
            session_id,
            kind: EventKind::Speech,
            speaker_peer_id: "peer-1".into(),
            speaker_user_id: user_id,
            speaker_name: "Alice".into(),
            original_text: text.into(),
            original_lang: "it".into(),
            translations: HashMap::from([("en".to_string(), format!("{text} (en)"))]),
            ts: Utc::now(),
        }
    }

    #[tokio::test]
    async fn record_flush_persists_events_with_translations() {
        let Some(svc) = test_service().await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        let uid = make_user(&svc).await;
        let sid = Uuid::new_v4();
        svc.session_started(sid, "room-t").await.unwrap();
        svc.participant_joined(sid, "peer-1", Some(uid), "Alice", "it")
            .await
            .unwrap();

        svc.record(speech_event(sid, Some(uid), "ciao"));
        let mut chat = speech_event(sid, Some(uid), "come va?");
        chat.kind = EventKind::Chat;
        svc.record(chat);
        svc.flush().await;

        let rows: Vec<(String, String, serde_json::Value)> = sqlx::query_as(
            "SELECT event_type, original_text, translations
             FROM transcript_events WHERE session_id = $1 ORDER BY ts",
        )
        .bind(sid)
        .fetch_all(&svc.pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "speech");
        assert_eq!(rows[0].1, "ciao");
        assert_eq!(rows[0].2["en"], "ciao (en)");
        assert_eq!(rows[1].0, "chat");
    }

    #[tokio::test]
    async fn finalize_stamps_ended_and_keeps_user_sessions() {
        let Some(svc) = test_service().await else {
            return;
        };
        let uid = make_user(&svc).await;
        let sid = Uuid::new_v4();
        svc.session_started(sid, "room-t").await.unwrap();
        let pid = svc
            .participant_joined(sid, "peer-1", Some(uid), "Alice", "it")
            .await
            .unwrap();
        svc.participant_left(pid).await.unwrap();
        svc.finalize_session(sid).await.unwrap();

        let (ended, left): (Option<DateTime<Utc>>, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT cs.ended_at, sp.left_at FROM call_sessions cs
             JOIN session_participants sp ON sp.session_id = cs.id
             WHERE cs.id = $1",
        )
        .bind(sid)
        .fetch_one(&svc.pool)
        .await
        .unwrap();
        assert!(ended.is_some(), "session kept (a user participated)");
        assert!(left.is_some());
    }

    #[tokio::test]
    async fn finalize_purges_guest_only_sessions() {
        let Some(svc) = test_service().await else {
            return;
        };
        let sid = Uuid::new_v4();
        svc.session_started(sid, "room-g").await.unwrap();
        svc.participant_joined(sid, "peer-1", None, "Guest", "it")
            .await
            .unwrap();
        svc.record(speech_event(sid, None, "ciao"));
        svc.finalize_session(sid).await.unwrap();

        let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM call_sessions WHERE id = $1")
            .bind(sid)
            .fetch_one(&svc.pool)
            .await
            .unwrap();
        assert_eq!(sessions, 0, "guest-only session purged");
        let events: i64 =
            sqlx::query_scalar("SELECT count(*) FROM transcript_events WHERE session_id = $1")
                .bind(sid)
                .fetch_one(&svc.pool)
                .await
                .unwrap();
        assert_eq!(events, 0, "events cascade with the session");
    }

    #[tokio::test]
    async fn list_access_export_roundtrip() {
        let Some(svc) = test_service().await else {
            return;
        };
        let alice = make_user(&svc).await;
        let eve = make_user(&svc).await;
        let sid = Uuid::new_v4();
        svc.session_started(sid, "room-x").await.unwrap();
        let pid = svc
            .participant_joined(sid, "peer-1", Some(alice), "Alice", "it")
            .await
            .unwrap();
        // A rejoin with the same peer id must not duplicate the participant.
        svc.participant_joined(sid, "peer-1", Some(alice), "Alice", "it")
            .await
            .unwrap();
        svc.record(speech_event(sid, Some(alice), "ciao"));
        svc.flush().await;
        svc.participant_left(pid).await.unwrap();
        svc.finalize_session(sid).await.unwrap();

        // Listing: Alice sees the session with its event count, Eve doesn't.
        let mine = svc.list_sessions(alice, 50).await.unwrap();
        let row = mine.iter().find(|s| s.id == sid).expect("session listed");
        assert_eq!(row.room, "room-x");
        assert_eq!(row.event_count, 1);
        assert!(row.ended_at.is_some());
        assert!(svc.list_sessions(eve, 50).await.unwrap().is_empty());

        // Access: participant Ok, stranger Forbidden, unknown NotFound.
        assert_eq!(svc.access(sid, alice).await.unwrap(), SessionAccess::Ok);
        assert_eq!(
            svc.access(sid, eve).await.unwrap(),
            SessionAccess::Forbidden
        );
        assert_eq!(
            svc.access(Uuid::new_v4(), alice).await.unwrap(),
            SessionAccess::NotFound
        );

        // Export: full document, peer ids only, translations map intact.
        let doc = svc.export(sid).await.unwrap().expect("exists");
        assert_eq!(doc.session.room_name, "room-x");
        assert!(doc.session.duration_seconds >= 0);
        assert_eq!(doc.session.participants.len(), 1, "rejoin deduped");
        assert_eq!(doc.session.participants[0].id, "peer-1");
        assert_eq!(doc.events.len(), 1);
        assert_eq!(doc.events[0].kind, "speech");
        assert_eq!(doc.events[0].original, "ciao");
        assert_eq!(doc.events[0].translations["en"], "ciao (en)");
        assert!(svc.export(Uuid::new_v4()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn late_event_after_purge_is_dropped_silently() {
        let Some(svc) = test_service().await else {
            return;
        };
        let sid = Uuid::new_v4();
        svc.session_started(sid, "room-g").await.unwrap();
        svc.participant_joined(sid, "peer-1", None, "Guest", "it")
            .await
            .unwrap();
        svc.finalize_session(sid).await.unwrap(); // purged (guest-only)

        // A straggler event lands after the purge: the UNNEST inner join drops
        // it without erroring, and nothing is resurrected.
        svc.record(speech_event(sid, None, "too late"));
        svc.flush().await;

        let events: i64 =
            sqlx::query_scalar("SELECT count(*) FROM transcript_events WHERE session_id = $1")
                .bind(sid)
                .fetch_one(&svc.pool)
                .await
                .unwrap();
        assert_eq!(events, 0);
    }
}
