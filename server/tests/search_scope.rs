//! Security-critical regression test for semantic-search visibility scoping.
//!
//! A B2B **member** must only ever see transcripts of projects they CREATED or
//! PARTICIPATED in; an **admin/owner** sees the whole org. This test seeds all
//! three project cases (created / participated / neither) and runs the SAME scoped
//! KNN SQL the `/search` handler uses (`business::search::list`), asserting which
//! sessions each caller can reach. Embeddings are inserted directly with a dummy
//! vector, so the test needs no OpenAI key — it exercises the scope filter, not
//! ranking quality.
//!
//! Skipped when `DATABASE_URL` is unset (same convention as the other DB tests);
//! the DB it points at must have the `vector` extension (migration 030).

use sqlx::PgPool;
use uuid::Uuid;
use voxtranslate_server::db;

/// The exact scope predicate from `business::search::list`: admins pass `None`
/// (no project restriction); members pass their visible-project set.
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

/// The visible-project resolution query from the handler (created OR participated).
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

/// Create a project + a call session bound to it + a ready transcript + one
/// embedding row. Returns `(project_id, session_id)`. `participant` (if any) is
/// recorded as a session participant.
async fn mk_project_with_call(
    pool: &PgPool,
    org_id: Uuid,
    created_by: Uuid,
    participant: Option<Uuid>,
) -> (Uuid, Uuid) {
    let project_id = Uuid::new_v4();
    sqlx::query("INSERT INTO projects (id, org_id, name, created_by) VALUES ($1, $2, $3, $4)")
        .bind(project_id)
        .bind(org_id)
        .bind(format!("proj-{project_id}"))
        .bind(created_by)
        .execute(pool)
        .await
        .expect("insert project");

    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO call_sessions (id, room, org_id, project_id, transcript_status)
         VALUES ($1, $2, $3, $4, 'ready')",
    )
    .bind(session_id)
    .bind(format!("room-{session_id}"))
    .bind(org_id)
    .bind(project_id)
    .execute(pool)
    .await
    .expect("insert call_session");

    if let Some(uid) = participant {
        sqlx::query(
            "INSERT INTO session_participants (session_id, peer_id, user_id, name, lang)
             VALUES ($1, $2, $3, 'P', 'en')",
        )
        .bind(session_id)
        .bind(format!("peer-{uid}"))
        .bind(uid)
        .execute(pool)
        .await
        .expect("insert participant");
    }

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

    (project_id, session_id)
}

#[tokio::test]
async fn member_scope_is_created_or_participated_admin_sees_all() {
    let Ok(db_url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let pool = db::connect(&db_url).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");

    // Users: org owner, the member under test, an admin, and an unrelated creator.
    let owner = mk_user(&pool, "owner").await;
    let member = mk_user(&pool, "member").await;
    let admin = mk_user(&pool, "admin").await;
    let other = mk_user(&pool, "other").await;

    let org_id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug, owner_id) VALUES ($1, $2, $3, $4)")
        .bind(org_id)
        .bind("Acme")
        .bind(format!("acme-{org_id}"))
        .bind(owner)
        .execute(&pool)
        .await
        .expect("insert org");
    for (uid, role) in [(owner, "owner"), (member, "member"), (admin, "admin")] {
        sqlx::query("INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, $3)")
            .bind(org_id)
            .bind(uid)
            .bind(role)
            .execute(&pool)
            .await
            .expect("insert member");
    }

    // Three projects: created by the member, participated in by the member, and
    // neither (owned by `other`, member absent).
    let (p_created, s_created) = mk_project_with_call(&pool, org_id, member, None).await;
    let (p_part, s_part) = mk_project_with_call(&pool, org_id, other, Some(member)).await;
    let (_p_other, s_other) = mk_project_with_call(&pool, org_id, other, None).await;

    // Member's visible set = created ∪ participated, never the third project.
    let visible = visible_projects(&pool, org_id, member).await;
    assert!(
        visible.contains(&p_created),
        "member should see created project"
    );
    assert!(
        visible.contains(&p_part),
        "member should see participated project"
    );
    assert_eq!(
        visible.len(),
        2,
        "member must NOT see the unrelated project"
    );

    // Member's scoped search returns only their two sessions.
    let member_hits = scoped_session_ids(&pool, org_id, Some(&visible)).await;
    assert!(member_hits.contains(&s_created));
    assert!(member_hits.contains(&s_part));
    assert!(
        !member_hits.contains(&s_other),
        "SECURITY: member reached a transcript outside their projects"
    );

    // Admin (scope = None) reaches every session in the org, including the third.
    let admin_hits = scoped_session_ids(&pool, org_id, None).await;
    for s in [s_created, s_part, s_other] {
        assert!(
            admin_hits.contains(&s),
            "admin should see every org transcript"
        );
    }
    // Sanity: the admin role does resolve above member rank.
    assert!(
        voxtranslate_server::business::role_rank("admin") >= voxtranslate_server::business::ADMIN
    );
    let _ = admin; // used via role seeding above

    // Clean up this run's rows (org cascade removes members/projects/sessions/etc).
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org_id)
        .execute(&pool)
        .await
        .expect("cleanup org");
    for uid in [owner, member, admin, other] {
        let _ = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(uid)
            .execute(&pool)
            .await;
    }
}
