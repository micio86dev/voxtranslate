//! Regression test for project voice messages (spec: B2B project voice notes).
//!
//! A voice note is modelled as a lightweight `call_sessions` row tagged
//! `kind = 'voice_message'` + a transcript + an embedding, so the EXISTING
//! semantic-search / insights retrieval consume it unchanged, while the COLD
//! analytics/history aggregations exclude it via `kind = 'call'`. This test
//! asserts both halves of that contract:
//!   1. the analytics KPI count (which now filters `kind = 'call'`) excludes the
//!      voice note but counts a real call;
//!   2. the UNCHANGED scoped search KNN still returns the voice note's session for
//!      the uploader (via their participant row) and for an admin (scope = None);
//!   3. the uploader's visible-project set includes the note's project, granted by
//!      the participant row the create handler inserts.
//!
//! Skipped when `DATABASE_URL` is unset (same convention as the other DB tests);
//! the DB must have the `vector` extension (migration 030).

use sqlx::PgPool;
use uuid::Uuid;
use voxtranslate_server::db;

/// The scoped KNN predicate from `business::search::list` — UNCHANGED by this
/// feature. Admins pass `None`; members pass their visible-project set.
async fn scoped_session_ids(pool: &PgPool, org_id: Uuid, scope: Option<&[Uuid]>) -> Vec<Uuid> {
    let qvec = pgvector::Vector::from(vec![0.1f32; 1536]);
    let rows: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT te.session_id
         FROM transcript_embeddings te
         JOIN call_sessions cs ON cs.id = te.session_id
         LEFT JOIN projects p  ON p.id = te.project_id
         WHERE te.org_id = $1
           AND ($3::uuid[] IS NULL OR te.project_id = ANY($3))
         ORDER BY te.embedding <=> $2
         LIMIT 50",
    )
    .bind(org_id)
    .bind(qvec)
    .bind(scope)
    .fetch_all(pool)
    .await
    .expect("scoped knn query");
    rows.into_iter().map(|r| r.0).collect()
}

/// The visible-project resolution query from the handler (created OR participated)
/// — UNCHANGED. The voice-note uploader becomes "participated" via their row.
async fn visible_projects(pool: &PgPool, org_id: Uuid, user_id: Uuid) -> Vec<Uuid> {
    sqlx::query_scalar(
        "SELECT id FROM projects
         WHERE org_id = $1 AND archived_at IS NULL
           AND (created_by = $2
                OR id IN (
                    SELECT DISTINCT cs.project_id FROM call_sessions cs
                    JOIN session_participants sp ON sp.session_id = cs.id
                    WHERE cs.org_id = $1 AND sp.user_id = $2 AND cs.project_id IS NOT NULL
                ))",
    )
    .bind(org_id)
    .bind(user_id)
    .fetch_all(pool)
    .await
    .expect("visible projects query")
}

/// The analytics KPI call count, mirroring `business::analytics::summary` — note
/// the `kind = 'call'` filter this feature added.
async fn analytics_call_count(pool: &PgPool, org_id: Uuid, with_kind_filter: bool) -> i64 {
    let sql = if with_kind_filter {
        "SELECT count(*)::bigint FROM call_sessions WHERE org_id = $1 AND kind = 'call'"
    } else {
        "SELECT count(*)::bigint FROM call_sessions WHERE org_id = $1"
    };
    sqlx::query_scalar(sql)
        .bind(org_id)
        .fetch_one(pool)
        .await
        .expect("kpi count")
}

async fn mk_user(pool: &PgPool, tag: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, google_id, email, name) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(format!("g-{id}"))
        .bind(format!("{tag}-{id}@example.test"))
        .bind(tag)
        .execute(pool)
        .await
        .expect("insert user");
    id
}

/// Seed a transcript + one embedding row for a session (dummy vector → no OpenAI
/// key needed; we exercise scope, not ranking).
async fn add_transcript_embedding(pool: &PgPool, session_id: Uuid, org_id: Uuid, project_id: Uuid) {
    let transcript_id: Uuid = sqlx::query_scalar(
        "INSERT INTO transcripts (session_id, org_id, source_language) VALUES ($1, $2, 'en')
         RETURNING id",
    )
    .bind(session_id)
    .bind(org_id)
    .fetch_one(pool)
    .await
    .expect("insert transcript");
    sqlx::query(
        "INSERT INTO transcript_embeddings
            (transcript_id, session_id, org_id, project_id, chunk_index, content, embedding)
         VALUES ($1, $2, $3, $4, 0, 'dummy', $5)",
    )
    .bind(transcript_id)
    .bind(session_id)
    .bind(org_id)
    .bind(project_id)
    .bind(pgvector::Vector::from(vec![0.1f32; 1536]))
    .execute(pool)
    .await
    .expect("insert embedding");
}

#[tokio::test]
async fn voice_note_is_searchable_but_excluded_from_call_kpis() {
    let Ok(db_url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let pool = db::connect(&db_url).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");

    let owner = mk_user(&pool, "owner").await;
    let member = mk_user(&pool, "member").await;

    let org_id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug, owner_id) VALUES ($1, $2, $3, $4)")
        .bind(org_id)
        .bind("Acme")
        .bind(format!("acme-{org_id}"))
        .bind(owner)
        .execute(&pool)
        .await
        .expect("insert org");
    for (uid, role) in [(owner, "owner"), (member, "member")] {
        sqlx::query("INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, $3)")
            .bind(org_id)
            .bind(uid)
            .bind(role)
            .execute(&pool)
            .await
            .expect("insert member");
    }

    // A project owned by the org owner — the member has NOT been in any call here.
    let project_id = Uuid::new_v4();
    sqlx::query("INSERT INTO projects (id, org_id, name, created_by) VALUES ($1, $2, $3, $4)")
        .bind(project_id)
        .bind(org_id)
        .bind(format!("proj-{project_id}"))
        .bind(owner)
        .execute(&pool)
        .await
        .expect("insert project");

    // A REAL call in the project (kind defaults to 'call').
    let call_session = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO call_sessions (id, room, org_id, project_id, transcript_status)
         VALUES ($1, $2, $3, $4, 'ready')",
    )
    .bind(call_session)
    .bind(format!("room-{call_session}"))
    .bind(org_id)
    .bind(project_id)
    .execute(&pool)
    .await
    .expect("insert call session");
    add_transcript_embedding(&pool, call_session, org_id, project_id).await;

    // A VOICE NOTE the member dropped onto the project: synthetic session tagged
    // 'voice_message' + the uploader as a participant (the visibility hook) +
    // transcript + embedding + the artifact row — exactly what the create handler does.
    let vm_session = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO call_sessions (id, room, org_id, project_id, kind, transcript_status, ended_at)
         VALUES ($1, $2, $3, $4, 'voice_message', 'ready', now())",
    )
    .bind(vm_session)
    .bind(format!("vm-{vm_session}"))
    .bind(org_id)
    .bind(project_id)
    .execute(&pool)
    .await
    .expect("insert voice-message session");
    sqlx::query(
        "INSERT INTO session_participants (session_id, peer_id, user_id, name, lang)
         VALUES ($1, $2, $3, 'M', 'en')",
    )
    .bind(vm_session)
    .bind(format!("vm-{member}"))
    .bind(member)
    .execute(&pool)
    .await
    .expect("insert uploader participant");
    add_transcript_embedding(&pool, vm_session, org_id, project_id).await;
    sqlx::query(
        "INSERT INTO project_voice_messages
            (org_id, project_id, session_id, created_by, created_by_name, object_path,
             file_name, content_type, size_bytes, source_language)
         VALUES ($1, $2, $3, $4, 'member', $5, 'voice-message.webm', 'audio/webm', 1024, 'en')",
    )
    .bind(org_id)
    .bind(project_id)
    .bind(vm_session)
    .bind(member)
    .bind(format!("{project_id}/{}.webm", Uuid::new_v4()))
    .execute(&pool)
    .await
    .expect("insert project_voice_messages");

    // (1) Analytics call KPI: the kind='call' filter counts the real call only,
    // while the unfiltered count would have wrongly included the voice note.
    assert_eq!(
        analytics_call_count(&pool, org_id, true).await,
        1,
        "voice note must NOT inflate the call KPI"
    );
    assert_eq!(
        analytics_call_count(&pool, org_id, false).await,
        2,
        "sanity: without the kind filter both sessions are counted"
    );

    // (2) The unchanged scoped search returns BOTH sessions for an admin (scope=None)…
    let admin_hits = scoped_session_ids(&pool, org_id, None).await;
    assert!(admin_hits.contains(&call_session));
    assert!(
        admin_hits.contains(&vm_session),
        "admin search must surface the voice note"
    );

    // …and (3) the member — who only contributed the voice note — sees the project
    // (via the participant row) and the voice-note session in their scoped search.
    let visible = visible_projects(&pool, org_id, member).await;
    assert!(
        visible.contains(&project_id),
        "uploader must see the project via their voice-note participant row"
    );
    let member_hits = scoped_session_ids(&pool, org_id, Some(&visible)).await;
    assert!(
        member_hits.contains(&vm_session),
        "uploader must be able to retrieve their own voice note"
    );

    // Cleanup (org cascade removes members/projects/sessions/pvm/etc).
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org_id)
        .execute(&pool)
        .await
        .expect("cleanup org");
    for uid in [owner, member] {
        let _ = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(uid)
            .execute(&pool)
            .await;
    }
}
