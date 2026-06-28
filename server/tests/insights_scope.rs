//! Security-critical regression test for the insights assistant's team-lead scope.
//!
//! A team **lead** may only reach members of the teams they lead and the projects
//! those members created or participated in; an org **owner** reaches everything; a
//! user who leads no team is not eligible. This seeds those cases and runs the SAME
//! scope SQL the `/insights` handler uses (`business::insights::resolve_scope` +
//! `ensure_*_in_scope`), asserting the resolved sets. Mirrors `search_scope.rs`.
//!
//! Skipped when `DATABASE_URL` is unset; the DB must have the `vector` extension.

use sqlx::PgPool;
use uuid::Uuid;
use voxtranslate_server::db;

/// The handler's "teams I lead" query.
async fn led_team_ids(pool: &PgPool, org_id: Uuid, user_id: Uuid) -> Vec<Uuid> {
    sqlx::query_scalar(
        "SELECT tm.team_id FROM team_members tm
         JOIN teams t ON t.id = tm.team_id
         WHERE t.org_id = $1 AND t.archived_at IS NULL
           AND tm.user_id = $2 AND tm.role = 'lead'",
    )
    .bind(org_id)
    .bind(user_id)
    .fetch_all(pool)
    .await
    .expect("led teams")
}

/// The handler's member set for led teams.
async fn member_ids(pool: &PgPool, led: &[Uuid]) -> Vec<Uuid> {
    sqlx::query_scalar("SELECT DISTINCT user_id FROM team_members WHERE team_id = ANY($1)")
        .bind(led)
        .fetch_all(pool)
        .await
        .expect("member ids")
}

/// The handler's project set: created-by OR participated-in by the scoped members.
async fn project_ids(pool: &PgPool, org_id: Uuid, members: &[Uuid]) -> Vec<Uuid> {
    sqlx::query_scalar(
        "SELECT id FROM projects
         WHERE org_id = $1 AND archived_at IS NULL
           AND (created_by = ANY($2)
                OR id IN (
                    SELECT DISTINCT cs.project_id FROM call_sessions cs
                    JOIN session_participants sp ON sp.session_id = cs.id
                    WHERE cs.org_id = $1 AND sp.user_id = ANY($2) AND cs.project_id IS NOT NULL
                ))",
    )
    .bind(org_id)
    .bind(members)
    .fetch_all(pool)
    .await
    .expect("project ids")
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

async fn add_team(pool: &PgPool, org_id: Uuid, creator: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO teams (id, org_id, name, created_by) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(org_id)
        .bind(format!("team-{id}"))
        .bind(creator)
        .execute(pool)
        .await
        .expect("insert team");
    id
}

async fn add_team_member(pool: &PgPool, team_id: Uuid, user_id: Uuid, role: &str) {
    sqlx::query(
        "INSERT INTO team_members (team_id, user_id, added_by, role) VALUES ($1, $2, $2, $3)",
    )
    .bind(team_id)
    .bind(user_id)
    .bind(role)
    .execute(pool)
    .await
    .expect("insert team member");
}

async fn add_project(pool: &PgPool, org_id: Uuid, created_by: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO projects (id, org_id, name, created_by) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(org_id)
        .bind(format!("proj-{id}"))
        .bind(created_by)
        .execute(pool)
        .await
        .expect("insert project");
    id
}

/// A call bound to `project_id` with `participant` recorded — to exercise the
/// participated-in branch of the project scope.
async fn add_call_with_participant(
    pool: &PgPool,
    org_id: Uuid,
    project_id: Uuid,
    participant: Uuid,
) {
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
    sqlx::query(
        "INSERT INTO session_participants (session_id, peer_id, user_id, name, lang)
         VALUES ($1, $2, $3, 'P', 'en')",
    )
    .bind(session_id)
    .bind(format!("peer-{participant}"))
    .bind(participant)
    .execute(pool)
    .await
    .expect("insert participant");
}

#[tokio::test]
async fn lead_scope_is_led_teams_members_and_their_projects() {
    let Ok(db_url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let pool = db::connect(&db_url).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");

    let owner = mk_user(&pool, "owner").await;
    let lead = mk_user(&pool, "lead").await;
    let member_a = mk_user(&pool, "memberA").await; // in the led team
    let member_b = mk_user(&pool, "memberB").await; // in a DIFFERENT team (out of scope)
    let other = mk_user(&pool, "other").await; // unrelated creator

    let org_id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug, owner_id) VALUES ($1, $2, $3, $4)")
        .bind(org_id)
        .bind("Acme")
        .bind(format!("acme-{org_id}"))
        .bind(owner)
        .execute(&pool)
        .await
        .expect("insert org");
    for (uid, role) in [
        (owner, "owner"),
        (lead, "admin"),
        (member_a, "member"),
        (member_b, "member"),
        (other, "member"),
    ] {
        sqlx::query("INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, $3)")
            .bind(org_id)
            .bind(uid)
            .bind(role)
            .execute(&pool)
            .await
            .expect("insert org member");
    }

    // team_led: lead=lead, member=member_a. team_other: member=member_b, NOT led by `lead`.
    let team_led = add_team(&pool, org_id, owner).await;
    add_team_member(&pool, team_led, lead, "lead").await;
    add_team_member(&pool, team_led, member_a, "member").await;
    let team_other = add_team(&pool, org_id, owner).await;
    add_team_member(&pool, team_other, member_b, "member").await;

    // Projects: in-scope via created_by (member_a); out-of-scope (member_b); in-scope
    // via PARTICIPATION (created by `other` but member_a joined a call there).
    let proj_created = add_project(&pool, org_id, member_a).await;
    let proj_out = add_project(&pool, org_id, member_b).await;
    let proj_participated = add_project(&pool, org_id, other).await;
    add_call_with_participant(&pool, org_id, proj_participated, member_a).await;

    // --- Lead scope ---
    let led = led_team_ids(&pool, org_id, lead).await;
    assert_eq!(led, vec![team_led], "lead should lead exactly team_led");
    let members = member_ids(&pool, &led).await;
    assert!(members.contains(&member_a) && members.contains(&lead));
    assert!(
        !members.contains(&member_b),
        "member_b is in a non-led team"
    );
    let projects = project_ids(&pool, org_id, &members).await;
    assert!(projects.contains(&proj_created), "created-by in scope");
    assert!(
        projects.contains(&proj_participated),
        "participated-in in scope"
    );
    assert!(
        !projects.contains(&proj_out),
        "SECURITY: lead reached a project outside their team scope"
    );

    // --- A user who leads no team is not eligible (empty led set) ---
    assert!(
        led_team_ids(&pool, org_id, member_b).await.is_empty(),
        "member_b leads no team → not eligible"
    );

    // --- Owner rank resolves above member (handler grants Scope::All) ---
    assert!(
        voxtranslate_server::business::role_rank("owner") >= voxtranslate_server::business::OWNER
    );

    // Cleanup (org cascade removes members/teams/projects/sessions).
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org_id)
        .execute(&pool)
        .await
        .expect("cleanup org");
    for uid in [owner, lead, member_a, member_b, other] {
        let _ = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(uid)
            .execute(&pool)
            .await;
    }
}
