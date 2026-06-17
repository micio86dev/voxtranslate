//! Quiz history persistence (spec 0098 / issue #221).
//!
//! Quizzes (specs 0046/0067) run as ephemeral relayed game state and are lost when
//! the call ends. The host POSTs the finished quiz + each participant's score here
//! so the session-detail page can show them afterwards. Participants are keyed by
//! `peer_id` + `display_name` (the transcript model), so a guest's score is stored
//! without an account; `created_by` is the authed host.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::Pool;

fn empty_json_array() -> serde_json::Value {
    serde_json::json!([])
}

/// A finished quiz to persist, as POSTed by the host.
#[derive(Debug, Deserialize)]
pub struct QuizInput {
    pub title: Option<String>,
    /// `[{ prompt, options: [text...], correct_index }]`
    #[serde(default = "empty_json_array")]
    pub questions: serde_json::Value,
    pub results: Vec<QuizResultInput>,
}

#[derive(Debug, Deserialize)]
pub struct QuizResultInput {
    pub peer_id: String,
    pub display_name: String,
    pub score: i32,
    pub total: i32,
    #[serde(default = "empty_json_array")]
    pub answers: serde_json::Value,
}

/// A persisted quiz with its results, for the detail page.
#[derive(Debug, Serialize)]
pub struct QuizRow {
    pub id: Uuid,
    pub title: Option<String>,
    pub questions: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub results: Vec<QuizResultRow>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct QuizResultRow {
    pub peer_id: String,
    pub display_name: String,
    pub score: i32,
    pub total: i32,
}

/// Insert a quiz + its results in one transaction. Returns the new quiz id.
pub async fn save_quiz(
    pool: &Pool,
    session_id: Uuid,
    created_by: Option<Uuid>,
    quiz: &QuizInput,
) -> Result<Uuid, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let quiz_id: Uuid = sqlx::query_scalar(
        "INSERT INTO session_quizzes (session_id, title, questions, created_by, status)
         VALUES ($1, $2, $3, $4, 'completed') RETURNING id",
    )
    .bind(session_id)
    .bind(&quiz.title)
    .bind(&quiz.questions)
    .bind(created_by)
    .fetch_one(&mut *tx)
    .await?;
    for r in &quiz.results {
        sqlx::query(
            "INSERT INTO quiz_results
                (quiz_id, user_id, peer_id, display_name, score, total, answers)
             VALUES ($1, NULL, $2, $3, $4, $5, $6)",
        )
        .bind(quiz_id)
        .bind(&r.peer_id)
        .bind(&r.display_name)
        .bind(r.score)
        .bind(r.total)
        .bind(&r.answers)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(quiz_id)
}

/// Every quiz for a session, newest first, each with its results (best score first).
pub async fn session_quizzes(pool: &Pool, session_id: Uuid) -> Result<Vec<QuizRow>, sqlx::Error> {
    let quizzes: Vec<(Uuid, Option<String>, serde_json::Value, DateTime<Utc>)> = sqlx::query_as(
        "SELECT id, title, questions, created_at FROM session_quizzes
         WHERE session_id = $1 ORDER BY created_at DESC",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;

    let mut out = Vec::with_capacity(quizzes.len());
    for (id, title, questions, created_at) in quizzes {
        let results: Vec<QuizResultRow> = sqlx::query_as(
            "SELECT peer_id, display_name, score, total FROM quiz_results
             WHERE quiz_id = $1 ORDER BY score DESC, display_name ASC",
        )
        .bind(id)
        .fetch_all(pool)
        .await?;
        out.push(QuizRow {
            id,
            title,
            questions,
            created_at,
            results,
        });
    }
    Ok(out)
}
