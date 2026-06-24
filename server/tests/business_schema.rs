//! Phase 1 schema verification for "VoxTranslate for Business" (spec 0106).
//!
//! These assert the shape and guarantees of migration `016_business_workspace.sql`
//! against a real Postgres: tables/columns exist with the right types, RLS is
//! enabled as default-deny defense-in-depth, the consumer `call_sessions` table
//! gained its business columns without disturbing the existing ones, cross-tenant
//! reads stay isolated, FK cascades behave, and `updated_at` advances on UPDATE
//! (the app-code convention — there are no triggers).
//!
//! Every test is **DB-gated**: it no-ops when `DATABASE_URL` is unset (e.g. CI
//! without a Postgres service). Locally, run against the Docker Postgres:
//! `DATABASE_URL=postgres://postgres:postgres@localhost:5433/voxtest \
//!   cargo test --test business_schema`.

use std::time::Duration;

use uuid::Uuid;
use voxtranslate_server::db::{self, Pool};

/// Connect + migrate, or `None` when there's no `DATABASE_URL`.
async fn setup() -> Option<Pool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    Some(pool)
}

/// Insert a throwaway user (unique google_id/email) for the many `users(id)` FKs.
async fn make_user(pool: &Pool) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO users (google_id, email, name)
         VALUES ($1, $2, 'Biz Tester') RETURNING id",
    )
    .bind(format!("g-{}", Uuid::new_v4()))
    .bind(format!("{}@x.com", Uuid::new_v4()))
    .fetch_one(pool)
    .await
    .unwrap()
}

/// `information_schema` data type of a column, or `None` if the column is absent.
async fn col_type(pool: &Pool, table: &str, col: &str) -> Option<String> {
    sqlx::query_scalar(
        "SELECT data_type FROM information_schema.columns
         WHERE table_schema = 'public' AND table_name = $1 AND column_name = $2",
    )
    .bind(table)
    .bind(col)
    .fetch_optional(pool)
    .await
    .unwrap()
}

/// Assert a column exists with the expected `information_schema` data type.
async fn assert_col(pool: &Pool, table: &str, col: &str, ty: &str) {
    let got = col_type(pool, table, col).await;
    assert_eq!(got.as_deref(), Some(ty), "{table}.{col} type");
}

/// Whether RLS is enabled on a public table.
async fn rls_enabled(pool: &Pool, table: &str) -> bool {
    sqlx::query_scalar(
        "SELECT relrowsecurity FROM pg_class
         WHERE relname = $1 AND relnamespace = 'public'::regnamespace",
    )
    .bind(table)
    .fetch_one(pool)
    .await
    .unwrap()
}

const NEW_TABLES: &[&str] = &[
    "organizations",
    "organization_members",
    "organization_invites",
    "projects",
    "room_business_bindings",
    "transcripts",
    "audit_logs",
    "organization_credits_transactions",
];

#[tokio::test]
async fn tables_and_columns_have_expected_types() {
    let Some(pool) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };

    // Every new table exists.
    for t in NEW_TABLES {
        let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(format!("public.{t}"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(exists, "table {t} should exist");
    }

    // Spot-check the columns whose types matter most for the app contract.
    assert_col(&pool, "organizations", "id", "uuid").await;
    assert_col(&pool, "organizations", "slug", "text").await;
    // Org credits are INTEGER — a separate currency from the consumer DECIMAL ledger.
    assert_col(&pool, "organizations", "credits_balance", "integer").await;
    assert_col(&pool, "organizations", "settings", "jsonb").await;
    assert_col(&pool, "organizations", "owner_id", "uuid").await;

    assert_col(&pool, "organization_members", "role", "text").await;
    assert_col(&pool, "organization_invites", "token", "text").await;
    assert_col(
        &pool,
        "organization_invites",
        "expires_at",
        "timestamp with time zone",
    )
    .await;

    assert_col(&pool, "projects", "default_languages", "ARRAY").await;

    assert_col(&pool, "transcripts", "segments", "jsonb").await;
    assert_col(&pool, "transcripts", "translations", "jsonb").await;

    assert_col(&pool, "audit_logs", "ip_address", "inet").await;

    // Org ledger amount is a signed INTEGER.
    assert_col(
        &pool,
        "organization_credits_transactions",
        "amount",
        "integer",
    )
    .await;
}

#[tokio::test]
async fn call_sessions_gained_business_columns_only() {
    let Some(pool) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };

    // The five additive columns (spec's "rooms" additions land on call_sessions).
    assert_col(&pool, "call_sessions", "org_id", "uuid").await;
    assert_col(&pool, "call_sessions", "project_id", "uuid").await;
    assert_col(&pool, "call_sessions", "cloud_recording_enabled", "boolean").await;
    assert_col(&pool, "call_sessions", "recording_storage_path", "text").await;
    assert_col(&pool, "call_sessions", "transcript_status", "text").await;

    // The pre-existing columns are untouched (no regression to the consumer flow).
    assert_col(&pool, "call_sessions", "id", "uuid").await;
    assert_col(&pool, "call_sessions", "room", "text").await;
    assert_col(
        &pool,
        "call_sessions",
        "started_at",
        "timestamp with time zone",
    )
    .await;

    // A plain consumer call_session inserts with the original two-column form and
    // gets the safe defaults — proving existing INSERTs keep working.
    let sid = Uuid::new_v4();
    sqlx::query("INSERT INTO call_sessions (id, room) VALUES ($1, 'consumer-room')")
        .bind(sid)
        .execute(&pool)
        .await
        .unwrap();
    let (org_id, recording, status): (Option<Uuid>, bool, String) = sqlx::query_as(
        "SELECT org_id, cloud_recording_enabled, transcript_status
         FROM call_sessions WHERE id = $1",
    )
    .bind(sid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(org_id.is_none(), "consumer call has no org");
    assert!(!recording, "recording defaults off");
    assert_eq!(status, "none", "transcript_status defaults to 'none'");

    sqlx::query("DELETE FROM call_sessions WHERE id = $1")
        .bind(sid)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn rls_enabled_on_every_new_table() {
    let Some(pool) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    for t in NEW_TABLES {
        assert!(rls_enabled(&pool, t).await, "RLS should be enabled on {t}");
    }
}

#[tokio::test]
async fn cross_org_reads_are_isolated() {
    let Some(pool) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };

    // Two orgs, two owners, plus Carol who belongs only to org A as an admin.
    let alice = make_user(&pool).await;
    let bob = make_user(&pool).await;
    let carol = make_user(&pool).await;

    let org_a = make_org(&pool, "Org A", alice).await;
    let org_b = make_org(&pool, "Org B", bob).await;

    sqlx::query(
        "INSERT INTO organization_members (org_id, user_id, role) VALUES
            ($1, $2, 'owner'),
            ($3, $4, 'owner'),
            ($1, $5, 'admin')",
    )
    .bind(org_a)
    .bind(alice)
    .bind(org_b)
    .bind(bob)
    .bind(carol)
    .execute(&pool)
    .await
    .unwrap();

    // The membership-scoped "list my orgs" query (the pattern the Rust guards use)
    // returns only the caller's orgs.
    assert_eq!(my_orgs(&pool, alice).await, vec![org_a]);
    assert_eq!(my_orgs(&pool, bob).await, vec![org_b]);
    assert_eq!(my_orgs(&pool, carol).await, vec![org_a]);

    // get_user_org_role() resolves roles for members and NULL for non-members —
    // this is what backs the API authorization checks.
    assert_eq!(role(&pool, org_a, alice).await.as_deref(), Some("owner"));
    assert_eq!(role(&pool, org_a, carol).await.as_deref(), Some("admin"));
    // Carol is NOT in org B → no role → the API would 403.
    assert_eq!(role(&pool, org_b, carol).await, None);
    // Bob can't see org A.
    assert_eq!(role(&pool, org_a, bob).await, None);

    // Cleanup cascades members away with the org.
    sqlx::query("DELETE FROM organizations WHERE id = ANY($1)")
        .bind(vec![org_a, org_b])
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn updated_at_advances_on_update() {
    let Some(pool) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let owner = make_user(&pool).await;
    let org = make_org(&pool, "Stamp Co", owner).await;

    let before = org_updated_at(&pool, org).await;

    // now() is the transaction timestamp; a later autocommitted UPDATE advances it.
    tokio::time::sleep(Duration::from_millis(10)).await;
    sqlx::query("UPDATE organizations SET name = 'Renamed', updated_at = now() WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();

    let after = org_updated_at(&pool, org).await;
    assert!(after > before, "updated_at must advance on UPDATE");

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn business_rows_round_trip_and_cascade() {
    let Some(pool) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let owner = make_user(&pool).await;
    let org = make_org(&pool, "Acme", owner).await;

    // The settings default is applied.
    let settings: serde_json::Value =
        sqlx::query_scalar("SELECT settings FROM organizations WHERE id = $1")
            .bind(org)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(settings["retention_days"], 90);
    assert_eq!(settings["compliance_mode"], false);

    let project: Uuid = sqlx::query_scalar(
        "INSERT INTO projects (org_id, name, default_languages, created_by)
         VALUES ($1, 'Q3 Sync', ARRAY['it','de'], $2) RETURNING id",
    )
    .bind(org)
    .bind(owner)
    .fetch_one(&pool)
    .await
    .unwrap();

    let room = format!("room-{}", Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO room_business_bindings
            (room, org_id, project_id, cloud_recording_enabled, created_by)
         VALUES ($1, $2, $3, true, $4)",
    )
    .bind(&room)
    .bind(org)
    .bind(project)
    .bind(owner)
    .execute(&pool)
    .await
    .unwrap();

    // A business call_session carrying the org/project + recording state.
    let sid = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO call_sessions
            (id, room, org_id, project_id, cloud_recording_enabled,
             recording_storage_path, transcript_status)
         VALUES ($1, $2, $3, $4, true, $5, 'processing')",
    )
    .bind(sid)
    .bind(&room)
    .bind(org)
    .bind(project)
    .bind(format!("{org}/{sid}/rec.webm"))
    .execute(&pool)
    .await
    .unwrap();

    let segments = serde_json::json!([
        {"speaker_id": "0", "speaker_name": "Alice", "text": "hi", "start_ms": 0, "end_ms": 900}
    ]);
    let transcript: Uuid = sqlx::query_scalar(
        "INSERT INTO transcripts (session_id, org_id, source_language, segments, word_count)
         VALUES ($1, $2, 'en', $3, 12) RETURNING id",
    )
    .bind(sid)
    .bind(org)
    .bind(segments)
    .fetch_one(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO audit_logs (org_id, actor_id, action, resource_type, resource_id)
         VALUES ($1, $2, 'transcript.view', 'transcript', $3)",
    )
    .bind(org)
    .bind(owner)
    .bind(transcript)
    .execute(&pool)
    .await
    .unwrap();

    // Org ledger: a recording charge (negative) tied to the call.
    sqlx::query(
        "INSERT INTO organization_credits_transactions
            (org_id, amount, type, session_id, description)
         VALUES ($1, -3, 'recording', $2, '3 min recording')",
    )
    .bind(org)
    .bind(sid)
    .execute(&pool)
    .await
    .unwrap();

    // A second call stays affiliated to the org until the org itself is deleted.
    let sid2 = Uuid::new_v4();
    sqlx::query("INSERT INTO call_sessions (id, room, org_id) VALUES ($1, 'biz-room-2', $2)")
        .bind(sid2)
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();

    // Deleting the call cascades its transcript (retention lifecycle) but PRESERVES
    // the financial ledger row, nulling its session_id (immutable accounting).
    sqlx::query("DELETE FROM call_sessions WHERE id = $1")
        .bind(sid)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        count_rows(&pool, "transcripts", "id", transcript).await,
        0,
        "transcript cascades with its call_session"
    );
    let ledger_session: Option<Uuid> = sqlx::query_scalar(
        "SELECT session_id FROM organization_credits_transactions WHERE org_id = $1",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        ledger_session.is_none(),
        "ledger row survives the call delete with a NULL session_id"
    );

    // Deleting the org cascades its members/projects/invites/bindings/audit/ledger,
    // but reverts shared call_sessions to unaffiliated (consumer) records.
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        count_rows(&pool, "projects", "id", project).await,
        0,
        "projects cascade"
    );
    assert_eq!(
        count_org(&pool, "room_business_bindings", org).await,
        0,
        "room bindings cascade"
    );
    assert_eq!(
        count_org(&pool, "audit_logs", org).await,
        0,
        "audit logs cascade"
    );
    assert_eq!(
        count_org(&pool, "organization_credits_transactions", org).await,
        0,
        "org ledger cascades"
    );
    let org_after: Option<Uuid> =
        sqlx::query_scalar("SELECT org_id FROM call_sessions WHERE id = $1")
            .bind(sid2)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        org_after.is_none(),
        "call_session survives org delete, reverted to consumer (org_id NULL)"
    );

    sqlx::query("DELETE FROM call_sessions WHERE id = $1")
        .bind(sid2)
        .execute(&pool)
        .await
        .unwrap();
}

// ---- shared helpers ----------------------------------------------------------

async fn make_org(pool: &Pool, name: &str, owner: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id)
         VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(name)
    .bind(format!("slug-{}", Uuid::new_v4().simple()))
    .bind(owner)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn my_orgs(pool: &Pool, user: Uuid) -> Vec<Uuid> {
    sqlx::query_scalar(
        "SELECT o.id FROM organizations o
         JOIN organization_members m ON m.org_id = o.id
         WHERE m.user_id = $1 ORDER BY o.created_at",
    )
    .bind(user)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn role(pool: &Pool, org: Uuid, user: Uuid) -> Option<String> {
    sqlx::query_scalar("SELECT get_user_org_role($1, $2)")
        .bind(org)
        .bind(user)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn org_updated_at(pool: &Pool, org: Uuid) -> chrono::DateTime<chrono::Utc> {
    sqlx::query_scalar("SELECT updated_at FROM organizations WHERE id = $1")
        .bind(org)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn count_rows(pool: &Pool, table: &str, col: &str, id: Uuid) -> i64 {
    let sql = format!("SELECT count(*) FROM {table} WHERE {col} = $1");
    sqlx::query_scalar(&sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Count rows in `table` belonging to `org` (via its `org_id` column).
async fn count_org(pool: &Pool, table: &str, org: Uuid) -> i64 {
    count_rows(pool, table, "org_id", org).await
}
