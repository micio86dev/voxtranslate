//! Turning a finished Live Connect conversation into a searchable record (spec 0121, S12/S13).
//!
//! While a conversation runs, what is said is stored by the existing meeting machinery as
//! `transcript_events`, one row per utterance, original and translations in separate columns.
//! That is the right shape for reading a conversation back, and the wrong one for finding it:
//! search, insights and the dashboards all work off a `transcripts` row and its embeddings, which
//! only post-call recordings and voice messages produced. Finalisation writes the same record
//! for a conversation, so everything that already finds a meeting finds this too — no second
//! search engine, no second set of filters.
//!
//! What is deliberately NOT done here:
//! * A **consumer** conversation gets no `transcripts` row. That table is organisation-owned
//!   (`org_id NOT NULL`), and an opted-in personal transcript has no organisation to belong to.
//!   It stays in `transcript_events`, readable by its owner through the live-transcript reader.
//! * **Translations** are not copied. They stay beside each utterance in `transcript_events`,
//!   where the live reader resolves them per viewer; the original text is what is indexed.
//!
//! Idempotent and safe to run twice or concurrently: the latest room session's row is locked and
//! `transcript_status` says whether the work has been done.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::business::transcripts::{chunk_segments, Chunk, Segment};
use crate::db::Pool;
use crate::AppState;

/// Milliseconds one word is assumed to take (about 150 words a minute). Events carry when an
/// utterance was captured, not how long it lasted, so an end time is an estimate and is the
/// only thing here that is.
const MS_PER_WORD: i64 = 400;
const MIN_SEGMENT_MS: i64 = 800;
const MAX_SEGMENT_MS: i64 = 30_000;

/// What finalising a conversation did.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The transcript was written (and `embedded` chunks indexed; 0 without an embedding
    /// provider or when indexing failed, which never fails the transcript).
    Finalized {
        transcript_id: Uuid,
        segments: usize,
        embedded: usize,
    },
    /// An earlier run (or retention) already settled it.
    AlreadyDone,
    /// Nothing was said, or the meeting-side rows are gone.
    NoContent,
    /// Not a business conversation, not found, or still open.
    NotApplicable,
}

// ---- the room sessions of a conversation -------------------------------------------------

/// SQL that selects the meeting-side rows of one conversation, with `live` standing for however
/// the caller names the live session id (a bind or a correlated column).
///
/// A conversation that outlived a flap of both phones lives in more than one `call_sessions`
/// row: `live_sessions.call_session_id` points at the latest, earlier ones exist only as
/// `room_session` events. The `kind` and `room` conditions make this safe to hang a DELETE on:
/// an event can only ever name a row that really is this conversation's room, so a corrupted or
/// forged pointer cannot reach another conversation or an ordinary meeting.
pub(crate) fn room_sessions_sql(live: &str) -> String {
    format!(
        "SELECT cs.id FROM call_sessions cs
         WHERE cs.kind = 'live_connect'
           AND cs.room = 'live-' || ({live})::uuid::text
           AND cs.id IN (
               SELECT call_session_id FROM live_sessions
               WHERE id = ({live})::uuid AND call_session_id IS NOT NULL
               UNION
               SELECT CASE WHEN detail->>'call_session_id'
                                ~ '^[0-9a-f]{{8}}-[0-9a-f]{{4}}-[0-9a-f]{{4}}-[0-9a-f]{{4}}-[0-9a-f]{{12}}$'
                           THEN (detail->>'call_session_id')::uuid END
               FROM live_session_events
               WHERE session_id = ({live})::uuid AND kind = 'room_session'
           )"
    )
}

/// Every existing meeting-side row of the conversation, oldest first.
pub async fn room_session_ids(
    pool: &Pool,
    live_session_id: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    let sql = format!("{} ORDER BY cs.started_at, cs.id", room_sessions_sql("$1"));
    sqlx::query_scalar(&sql)
        .bind(live_session_id)
        .fetch_all(pool)
        .await
}

// ---- building the transcript (pure) ------------------------------------------------------

/// One spoken utterance, as `transcript_events` holds it.
pub(crate) struct SpeechEvent {
    pub speaker_id: String,
    pub speaker_name: String,
    pub text: String,
    pub lang: String,
    pub at: DateTime<Utc>,
}

pub(crate) struct Built {
    pub segments: Vec<Segment>,
    pub source_language: String,
    pub word_count: i64,
    /// First to last utterance, for a conversation with no recorded duration.
    pub span_ms: i64,
}

fn estimate_ms(text: &str) -> i64 {
    (text.split_whitespace().count() as i64 * MS_PER_WORD).clamp(MIN_SEGMENT_MS, MAX_SEGMENT_MS)
}

/// Build the transcript from utterances already in time order. Blank utterances are dropped.
/// Every segment keeps the ORIGINAL text, the speaker's id and name exactly as the room used
/// them, and a start relative to the first utterance. `fallback_lang` is used only when no
/// utterance recorded a language.
pub(crate) fn build_transcript(events: &[SpeechEvent], fallback_lang: &str) -> Option<Built> {
    let spoken: Vec<&SpeechEvent> = events
        .iter()
        .filter(|e| !e.text.trim().is_empty())
        .collect();
    let first = spoken.first()?.at;
    let mut segments = Vec::with_capacity(spoken.len());
    let mut by_lang: HashMap<&str, usize> = HashMap::new();
    let mut words = 0i64;
    let mut last_end = 0i64;
    for e in &spoken {
        let text = e.text.trim();
        let start_ms = (e.at - first).num_milliseconds().max(0);
        let end_ms = start_ms + estimate_ms(text);
        last_end = last_end.max(end_ms);
        words += text.split_whitespace().count() as i64;
        if !e.lang.trim().is_empty() {
            *by_lang.entry(e.lang.trim()).or_default() += 1;
        }
        segments.push(Segment {
            speaker_id: e.speaker_id.clone(),
            speaker_name: e.speaker_name.clone(),
            text: text.to_string(),
            start_ms,
            end_ms,
        });
    }
    // The language spoken most; on a tie the alphabetically first, so the same events always
    // give the same answer.
    let source_language = by_lang
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(l, _)| l.to_string())
        .unwrap_or_else(|| fallback_lang.to_string());
    Some(Built {
        segments,
        source_language,
        word_count: words,
        span_ms: last_end,
    })
}

/// Chunks for indexing, one speaker at a time. The meeting chunker joins consecutive segments
/// up to ~250 words and labels the chunk with whoever spoke first; for diarised audio that is
/// fine, but a conversation is short alternating turns, so a chunk would be both people's words
/// under one name, and "what did the customer ask" could not be answered. Each run of one
/// speaker is chunked on its own with the shared chunker.
pub fn turn_chunks(segments: &[Segment]) -> Vec<Chunk> {
    let mut out = Vec::new();
    let mut run_start = 0;
    for i in 1..=segments.len() {
        if i == segments.len() || segments[i].speaker_id != segments[run_start].speaker_id {
            out.extend(chunk_segments(&segments[run_start..i]));
            run_start = i;
        }
    }
    out
}

/// Store embedded chunks for a transcript, replacing any earlier ones (so re-indexing is safe).
/// Separate from the embedding call so the storage and what reads it can be exercised without
/// an embedding provider.
pub async fn store_chunks(
    pool: &Pool,
    transcript_id: Uuid,
    session_id: Uuid,
    org_id: Uuid,
    project_id: Option<Uuid>,
    chunks: &[Chunk],
    vectors: Vec<Vec<f32>>,
) -> Result<usize, String> {
    if vectors.len() != chunks.len() {
        return Err("embedding count mismatch".to_string());
    }
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    sqlx::query("DELETE FROM transcript_embeddings WHERE transcript_id = $1")
        .bind(transcript_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    for (i, (chunk, vector)) in chunks.iter().zip(vectors).enumerate() {
        sqlx::query(
            "INSERT INTO transcript_embeddings
                (transcript_id, session_id, org_id, project_id, chunk_index,
                 speaker_name, start_ms, end_ms, content, embedding)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(transcript_id)
        .bind(session_id)
        .bind(org_id)
        .bind(project_id)
        .bind(i as i32)
        .bind(&chunk.speaker_name)
        .bind(chunk.start_ms)
        .bind(chunk.end_ms)
        .bind(&chunk.content)
        .bind(pgvector::Vector::from(vector))
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    }
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(chunks.len())
}

/// Embed the transcript's turns and store them. No-op without an embedding provider.
async fn embed_turns(
    state: &AppState,
    transcript_id: Uuid,
    session_id: Uuid,
    org_id: Uuid,
    project_id: Option<Uuid>,
    segments: &[Segment],
) -> Result<usize, String> {
    let Some(embedder) = state.embeddings.as_ref() else {
        return Ok(0);
    };
    let pool = state.pool.as_ref().ok_or("no database")?;
    let chunks = turn_chunks(segments);
    if chunks.is_empty() {
        return Ok(0);
    }
    let texts: Vec<String> = chunks.iter().map(|c| c.content.clone()).collect();
    let vectors = embedder.embed_batch(&texts).await?;
    store_chunks(
        pool,
        transcript_id,
        session_id,
        org_id,
        project_id,
        &chunks,
        vectors,
    )
    .await
}

// ---- finalising --------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct LiveRow {
    org_id: Option<Uuid>,
    project_id: Option<Uuid>,
    call_session_id: Option<Uuid>,
    status: String,
    host_lang: String,
    save_transcript: bool,
    /// Start to end, when both are known.
    secs: Option<f64>,
}

/// Write the transcript of an ended business conversation, and index it for search.
///
/// Safe to call any number of times, from anywhere, at the same time: the latest room session's
/// row is locked for the write and `transcript_status` records that it happened. A conversation
/// that is still open, was never staffed (no speech), or whose text retention already removed is
/// left alone.
pub async fn finalize(state: &AppState, live_session_id: Uuid) -> Result<Outcome, sqlx::Error> {
    let Some(pool) = state.pool.as_ref() else {
        return Ok(Outcome::NotApplicable);
    };
    let row: Option<LiveRow> = sqlx::query_as(
        "SELECT org_id, project_id, call_session_id, status, host_lang, save_transcript,
                EXTRACT(EPOCH FROM (ended_at - COALESCE(started_at, created_at)))::float8 AS secs
         FROM live_sessions WHERE id = $1",
    )
    .bind(live_session_id)
    .fetch_optional(pool)
    .await?;
    let Some(live) = row else {
        return Ok(Outcome::NotApplicable);
    };
    // No organisation: nothing to own a `transcripts` row. Still open: it is not finished.
    let Some(org_id) = live.org_id else {
        // A personal conversation the owner did not ask to keep leaves nothing behind: nothing
        // was recorded, so the names it held have no purpose once it is over either.
        if !live.save_transcript && matches!(live.status.as_str(), "ended" | "expired") {
            super::erase::scrub_participants(pool, live_session_id).await?;
        }
        return Ok(Outcome::NotApplicable);
    };
    if !matches!(live.status.as_str(), "ended" | "expired") {
        return Ok(Outcome::NotApplicable);
    }
    let Some(latest) = live.call_session_id else {
        return Ok(Outcome::NoContent);
    };

    // The recorder writes events in the background; wait for what is queued to land.
    if let Some(t) = state.transcripts.as_ref() {
        t.flush().await;
    }
    let rooms = room_session_ids(pool, live_session_id).await?;

    let mut tx = pool.begin().await?;
    let status: Option<String> =
        sqlx::query_scalar("SELECT transcript_status FROM call_sessions WHERE id = $1 FOR UPDATE")
            .bind(latest)
            .fetch_optional(&mut *tx)
            .await?;
    match status.as_deref() {
        None => return Ok(Outcome::NoContent),
        Some("none" | "failed") => {}
        // 'ready': done. 'expired': retention removed the text on purpose and the surviving
        // events must not bring it back. 'processing': somebody else is on it.
        Some(_) => return Ok(Outcome::AlreadyDone),
    }

    let rows: Vec<(String, String, String, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT speaker_peer_id, speaker_name, original_text, original_lang, ts
         FROM transcript_events
         WHERE session_id = ANY($1) AND event_type = 'speech'
         ORDER BY ts, created_at, id",
    )
    .bind(&rooms)
    .fetch_all(&mut *tx)
    .await?;
    let events: Vec<SpeechEvent> = rows
        .into_iter()
        .map(|(speaker_id, speaker_name, text, lang, at)| SpeechEvent {
            speaker_id,
            speaker_name,
            text,
            lang,
            at,
        })
        .collect();
    let Some(built) = build_transcript(&events, &live.host_lang) else {
        return Ok(Outcome::NoContent);
    };

    let duration = live
        .secs
        .map(|s| s.round() as i64)
        .unwrap_or(built.span_ms / 1000)
        .clamp(0, i32::MAX as i64) as i32;
    let segments_json = serde_json::to_value(&built.segments)
        .map_err(|e| sqlx::Error::Protocol(format!("segments serialisation: {e}")))?;

    // Exactly one transcript per session, like the post-call job.
    sqlx::query("DELETE FROM transcripts WHERE session_id = $1")
        .bind(latest)
        .execute(&mut *tx)
        .await?;
    let transcript_id: Uuid = sqlx::query_scalar(
        "INSERT INTO transcripts
            (session_id, org_id, source_language, segments, translations,
             duration_seconds, word_count, processed_at)
         VALUES ($1, $2, $3, $4, '{}'::jsonb, $5, $6, now())
         RETURNING id",
    )
    .bind(latest)
    .bind(org_id)
    .bind(&built.source_language)
    .bind(&segments_json)
    .bind(duration)
    .bind(built.word_count.clamp(0, i32::MAX as i64) as i32)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("UPDATE call_sessions SET transcript_status = 'ready' WHERE id = $1")
        .bind(latest)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    // Indexing is best-effort, like for voice messages: the transcript is saved, and a
    // conversation search misses until `/internal/embeddings/backfill` picks it up.
    let embedded = match embed_turns(
        state,
        transcript_id,
        latest,
        org_id,
        live.project_id,
        &built.segments,
    )
    .await
    {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(session = %live_session_id, error = %e, "live: could not index the transcript");
            0
        }
    };
    Ok(Outcome::Finalized {
        transcript_id,
        segments: built.segments.len(),
        embedded,
    })
}

/// Finalise a conversation that has just ended, a moment from now.
///
/// Called from every place that ends one. The pause lets the engines' last finals reach the
/// recorder first; `finalize` flushes it too, so this is a margin, not the mechanism. Fire and
/// forget: ending a conversation never waits on, or fails because of, its transcript.
pub fn on_session_ended(state: &AppState, live_session_id: Uuid) {
    if state.pool.is_none() {
        return;
    }
    let state = state.clone();
    let grace = Duration::from_secs(state.live.config.finalize_grace_secs.max(0) as u64);
    tokio::spawn(async move {
        if !grace.is_zero() {
            tokio::time::sleep(grace).await;
        }
        if let Err(e) = finalize(&state, live_session_id).await {
            tracing::warn!(session = %live_session_id, error = %e, "live: finalising failed");
        }
    });
}

/// Finalise ended business conversations nobody finalised: the timer above lives in memory, so
/// a restart between "ended" and "finalised" would otherwise lose the transcript for good.
/// Returns how many transcripts were written. `only` scopes a pass to given sessions (tests
/// share one database); `limit` bounds a pass.
pub async fn finalize_pending(
    state: &AppState,
    only: Option<&[Uuid]>,
    limit: i64,
) -> Result<u64, sqlx::Error> {
    let Some(pool) = state.pool.as_ref() else {
        return Ok(0);
    };
    let rooms = room_sessions_sql("ls.id");
    let sql = format!(
        "SELECT ls.id FROM live_sessions ls
         JOIN call_sessions latest ON latest.id = ls.call_session_id
         WHERE ls.org_id IS NOT NULL
           AND ls.status IN ('ended', 'expired')
           AND ls.ended_at <= now() - make_interval(secs => $3::int)
           AND latest.transcript_status IN ('none', 'failed')
           AND ($1::uuid[] IS NULL OR ls.id = ANY($1))
           AND EXISTS (
               SELECT 1 FROM transcript_events te
               WHERE te.event_type = 'speech' AND te.session_id IN ({rooms}))
         ORDER BY ls.ended_at
         LIMIT $2"
    );
    let ids: Vec<Uuid> = sqlx::query_scalar(&sql)
        .bind(only)
        .bind(limit)
        .bind(state.live.config.finalize_grace_secs as i32)
        .fetch_all(pool)
        .await?;
    let mut written = 0;
    for id in ids {
        match finalize(state, id).await {
            Ok(Outcome::Finalized { .. }) => written += 1,
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(session = %id, error = %e, "live: finalising a straggler failed")
            }
        }
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ev(speaker: &str, text: &str, lang: &str, secs: i64) -> SpeechEvent {
        SpeechEvent {
            speaker_id: speaker.to_string(),
            speaker_name: speaker.to_uppercase(),
            text: text.to_string(),
            lang: lang.to_string(),
            at: Utc.timestamp_opt(1_800_000_000 + secs, 0).unwrap(),
        }
    }

    #[test]
    fn segments_keep_the_original_the_speaker_and_a_start_relative_to_the_first_utterance() {
        let events = [
            ev("a", "  Buongiorno a tutti  ", "it", 100),
            ev("b", "Hola", "es", 107),
        ];
        let built = build_transcript(&events, "en").unwrap();
        assert_eq!(built.segments.len(), 2);
        assert_eq!(built.segments[0].text, "Buongiorno a tutti");
        assert_eq!(
            (built.segments[0].start_ms, built.segments[1].start_ms),
            (0, 7_000)
        );
        assert_eq!(built.segments[1].speaker_id, "b");
        assert_eq!(built.segments[1].speaker_name, "B");
        // 3 words * 400 ms; a single word is floored at 800 ms.
        assert_eq!(built.segments[0].end_ms - built.segments[0].start_ms, 1_200);
        assert_eq!(built.segments[1].end_ms - built.segments[1].start_ms, 800);
        assert_eq!(built.word_count, 4);
        assert_eq!(built.span_ms, 7_800);
    }

    #[test]
    fn a_very_long_utterance_is_capped_and_blank_ones_are_dropped() {
        let long = "word ".repeat(500);
        let events = [
            ev("a", "   ", "it", 0),
            ev("a", &long, "it", 1),
            ev("b", "", "es", 2),
        ];
        let built = build_transcript(&events, "en").unwrap();
        assert_eq!(built.segments.len(), 1);
        assert_eq!(
            built.segments[0].end_ms - built.segments[0].start_ms,
            MAX_SEGMENT_MS
        );
        assert_eq!(
            built.segments[0].start_ms, 0,
            "relative to the first NON-blank utterance"
        );
        assert!(build_transcript(&[ev("a", " ", "it", 0)], "en").is_none());
        assert!(build_transcript(&[], "en").is_none());
    }

    #[test]
    fn the_source_language_is_the_most_spoken_with_a_stable_tie_break() {
        let majority = [
            ev("a", "x", "es", 0),
            ev("b", "y", "es", 1),
            ev("a", "z", "it", 2),
        ];
        assert_eq!(
            build_transcript(&majority, "en").unwrap().source_language,
            "es"
        );
        let tie = [ev("a", "x", "it", 0), ev("b", "y", "es", 1)];
        assert_eq!(build_transcript(&tie, "en").unwrap().source_language, "es");
        let none = [ev("a", "x", "", 0)];
        assert_eq!(build_transcript(&none, "pt").unwrap().source_language, "pt");
    }

    fn seg(speaker: &str, text: &str, start: i64) -> Segment {
        Segment {
            speaker_id: speaker.to_string(),
            speaker_name: speaker.to_uppercase(),
            text: text.to_string(),
            start_ms: start,
            end_ms: start + 500,
        }
    }

    #[test]
    fn chunks_never_mix_speakers_and_keep_each_speakers_name() {
        let segments = [
            seg("a", "one", 0),
            seg("a", "two", 1_000),
            seg("b", "three", 2_000),
            seg("a", "four", 3_000),
        ];
        let chunks = turn_chunks(&segments);
        let got: Vec<(&str, &str)> = chunks
            .iter()
            .map(|c| (c.speaker_name.as_str(), c.content.as_str()))
            .collect();
        assert_eq!(got, [("A", "one two"), ("B", "three"), ("A", "four")]);
        assert_eq!((chunks[0].start_ms, chunks[0].end_ms), (0, 1_500));
        assert!(turn_chunks(&[]).is_empty());
    }

    #[test]
    fn a_long_monologue_is_still_split_by_the_shared_chunker() {
        let text = "word ".repeat(600);
        let chunks = turn_chunks(&[seg("a", &text, 0)]);
        assert_eq!(
            chunks.len(),
            1,
            "one segment is never cut mid-way, as in meetings"
        );
        let two = turn_chunks(&[
            seg("a", &"w ".repeat(300), 0),
            seg("a", &"w ".repeat(300), 9_000),
        ]);
        assert_eq!(
            two.len(),
            2,
            "consecutive turns of one speaker are grouped by size"
        );
    }
}
