//! Schema guarantees for Live Connect (spec 0121), migration `065_live_connect.sql`.
//!
//! The properties that matter are the ones the application leans on and would otherwise
//! have to re-check in Rust: a session always has an owner, a session holds at most two
//! participants (one per side), a billing tick can be written once, and purging the
//! conversation never erases the usage record.
//!
//! Every test is **DB-gated**: it no-ops when `DATABASE_URL` is unset. Run against the
//! local pgvector Postgres (`docker compose up -d --wait postgres`):
//! `DATABASE_URL=postgres://vox:vox_local_dev@127.0.0.1:55432/voxtranslate_voip_test \
//!   cargo test --test live_schema`.
//! A run that finishes in a few milliseconds skipped everything.

use chrono::Utc;
use serde_json::json;
use uuid::Uuid;
use voxtranslate_server::db::{self, Pool};

const LIVE_TABLES: &[&str] = &[
    "live_desks",
    "live_sessions",
    "live_participants",
    "live_usage_ticks",
    "live_session_events",
    "live_session_summaries",
    "live_desk_usage_daily",
];

/// Re-running a migration's DDL takes ACCESS EXCLUSIVE locks table by table; a query that reads
/// two of those tables in the other order deadlocks with it. Tests that re-run DDL, and the one
/// that reads the tables the desk-usage refresh touches, take turns.
static DDL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn setup() -> Option<Pool> {
    let url = db::test_database_url()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    Some(pool)
}

async fn make_user(pool: &Pool) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO users (google_id, email, name) VALUES ($1, $2, 'Live Tester') RETURNING id",
    )
    .bind(format!("g-{}", Uuid::new_v4()))
    .bind(format!("{}@x.com", Uuid::new_v4()))
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn make_org(pool: &Pool, owner: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id) VALUES ('Live Org', $1, $2) RETURNING id",
    )
    .bind(format!("slug-{}", Uuid::new_v4().simple()))
    .bind(owner)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A B2C session owned by `owner`, `waiting`, expiring in an hour.
async fn make_session(pool: &Pool, owner: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO live_sessions (owner_user_id, host_lang, expires_at)
         VALUES ($1, 'it', now() + interval '1 hour') RETURNING id",
    )
    .bind(owner)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn add_participant(pool: &Pool, session: Uuid, role: &str) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO live_participants (session_id, role, display_name, lang)
         VALUES ($1, $2, 'P', 'es') RETURNING peer_id",
    )
    .bind(session)
    .bind(role)
    .fetch_one(pool)
    .await
}

fn is_violation(err: &sqlx::Error, code: &str) -> bool {
    err.as_database_error()
        .and_then(|e| e.code())
        .is_some_and(|c| c == code)
}

#[tokio::test]
async fn live_tables_exist_with_rls_enabled() {
    let Some(pool) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    for t in LIVE_TABLES {
        let rls: Option<bool> = sqlx::query_scalar(
            "SELECT relrowsecurity FROM pg_class
             WHERE relname = $1 AND relnamespace = 'public'::regnamespace",
        )
        .bind(t)
        .fetch_optional(&pool)
        .await
        .unwrap();
        assert_eq!(rls, Some(true), "{t} must exist with RLS enabled");
    }
}

#[tokio::test]
async fn migration_is_idempotent() {
    let Some(pool) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let _turn = DDL.lock().await;
    // `sqlx::migrate!` already applied it once; applying the raw SQL again twice more must
    // be a no-op — the property CLAUDE.md demands of every migration.
    let sql = include_str!("../migrations/065_live_connect.sql");
    for _ in 0..2 {
        sqlx::raw_sql(sql)
            .execute(&pool)
            .await
            .expect("065 must be re-runnable");
    }
}

#[tokio::test]
async fn a_session_must_have_an_owner() {
    let Some(pool) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let err = sqlx::query(
        "INSERT INTO live_sessions (host_lang, expires_at) VALUES ('it', now() + interval '1 hour')",
    )
    .execute(&pool)
    .await
    .expect_err("neither owner_user_id nor org_id");
    assert!(is_violation(&err, "23514"), "check violation, got {err:?}");
}

#[tokio::test]
async fn status_and_role_vocabularies_are_enforced() {
    let Some(pool) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let owner = make_user(&pool).await;
    let session = make_session(&pool, owner).await;

    let err = sqlx::query("UPDATE live_sessions SET status = 'weird' WHERE id = $1")
        .bind(session)
        .execute(&pool)
        .await
        .expect_err("unknown status");
    assert!(is_violation(&err, "23514"));

    let err = add_participant(&pool, session, "admin")
        .await
        .expect_err("unknown role");
    assert!(is_violation(&err, "23514"));
}

#[tokio::test]
async fn a_session_holds_at_most_two_participants_one_per_side() {
    let Some(pool) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let owner = make_user(&pool).await;
    let session = make_session(&pool, owner).await;

    add_participant(&pool, session, "host").await.unwrap();
    // Owner side is taken: neither a second host nor a staff member may join it.
    for role in ["host", "staff"] {
        let err = add_participant(&pool, session, role)
            .await
            .expect_err("owner side is full");
        assert!(is_violation(&err, "23505"), "{role}: {err:?}");
    }
    add_participant(&pool, session, "guest").await.unwrap();
    let err = add_participant(&pool, session, "guest")
        .await
        .expect_err("guest side is full");
    assert!(is_violation(&err, "23505"));

    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM live_participants WHERE session_id = $1")
        .bind(session)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 2);
}

#[tokio::test]
async fn a_usage_tick_can_be_written_once() {
    let Some(pool) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let owner = make_user(&pool).await;
    let session = make_session(&pool, owner).await;
    let peer = add_participant(&pool, session, "host").await.unwrap();

    let insert = || {
        sqlx::query(
            "INSERT INTO live_usage_ticks (session_id, peer_id, tick_seq, payer_kind, amount)
             VALUES ($1, $2, 7, 'user', 0.001)
             ON CONFLICT (session_id, peer_id, tick_seq) DO NOTHING",
        )
        .bind(session)
        .bind(peer)
        .execute(&pool)
    };
    assert_eq!(
        insert().await.unwrap().rows_affected(),
        1,
        "first write wins"
    );
    assert_eq!(
        insert().await.unwrap().rows_affected(),
        0,
        "replay charges nothing"
    );
}

#[tokio::test]
async fn purging_the_conversation_keeps_the_usage_record() {
    let Some(pool) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let owner = make_user(&pool).await;
    let org = make_org(&pool, owner).await;
    let call: Uuid = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO call_sessions (id, room, kind, org_id) VALUES ($1, $2, 'live_connect', $3)",
    )
    .bind(call)
    .bind(format!("live-{call}"))
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    let session: Uuid = sqlx::query_scalar(
        "INSERT INTO live_sessions (call_session_id, org_id, host_lang, expires_at)
         VALUES ($1, $2, 'it', now() + interval '1 hour') RETURNING id",
    )
    .bind(call)
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();

    // Retention / finalize deletes the meeting-side row...
    sqlx::query("DELETE FROM call_sessions WHERE id = $1")
        .bind(call)
        .execute(&pool)
        .await
        .unwrap();
    // ...the live row (billing + desk attribution) survives with the link cleared.
    let linked: Option<Uuid> =
        sqlx::query_scalar("SELECT call_session_id FROM live_sessions WHERE id = $1")
            .bind(session)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(linked, None);

    // Deleting the org is the only thing that removes it.
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM live_sessions WHERE id = $1")
        .bind(session)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

#[tokio::test]
async fn desk_public_ids_are_unique_and_status_is_checked() {
    let Some(pool) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let owner = make_user(&pool).await;
    let org = make_org(&pool, owner).await;
    let pid = format!("d{}", Uuid::new_v4().simple());

    let insert = |public_id: String, status: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO live_desks (org_id, name, public_id, status) VALUES ($1, 'Reception', $2, $3)",
            )
            .bind(org)
            .bind(public_id)
            .bind(status)
            .execute(&pool)
            .await
        }
    };
    insert(pid.clone(), "active").await.unwrap();
    let err = insert(pid, "active")
        .await
        .expect_err("duplicate public_id");
    assert!(is_violation(&err, "23505"));
    let err = insert(format!("d{}", Uuid::new_v4().simple()), "paused")
        .await
        .expect_err("unknown status");
    assert!(is_violation(&err, "23514"));
}

#[tokio::test]
async fn deleting_a_staff_account_keeps_the_conversations_billing_ticks_and_erases_the_name() {
    let Some(pool) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    // Migration 066 is re-runnable and leaves the foreign key on SET NULL.
    let sql = include_str!("../migrations/066_live_participant_user_fk.sql");
    for _ in 0..2 {
        sqlx::raw_sql(sql)
            .execute(&pool)
            .await
            .expect("066 must be re-runnable");
    }
    let rule: String = sqlx::query_scalar(
        "SELECT confdeltype::text FROM pg_constraint
         WHERE conrelid = 'public.live_participants'::regclass AND conname = 'live_participants_user_id_fkey'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rule, "n", "ON DELETE SET NULL");

    let owner = make_user(&pool).await;
    let org = make_org(&pool, owner).await;
    let staff = make_user(&pool).await;
    let session: Uuid = sqlx::query_scalar(
        "INSERT INTO live_sessions (org_id, host_lang, status, expires_at) VALUES ($1, 'it', 'ended', now() + interval '1 hour') RETURNING id",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    let peer: Uuid = sqlx::query_scalar(
        "INSERT INTO live_participants (session_id, role, user_id, display_name, lang) VALUES ($1, 'staff', $2, 'Maria Rossi', 'it') RETURNING peer_id",
    )
    .bind(session)
    .bind(staff)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO live_usage_ticks (session_id, peer_id, tick_seq, payer_kind, amount) VALUES ($1, $2, 1, 'org', 5), ($1, $2, 2, 'org', 3)")
        .bind(session)
        .bind(peer)
        .execute(&pool)
        .await
        .unwrap();

    voxtranslate_server::safety::SafetyService::new(pool.clone())
        .delete_user(staff)
        .await
        .expect("account deletion");

    let ticks: i64 =
        sqlx::query_scalar("SELECT count(*) FROM live_usage_ticks WHERE session_id = $1")
            .bind(session)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        ticks, 2,
        "the billing ledger of the conversation survives the account"
    );
    let (user_id, name): (Option<Uuid>, String) =
        sqlx::query_as("SELECT user_id, display_name FROM live_participants WHERE peer_id = $1")
            .bind(peer)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (user_id, name.as_str()),
        (None, "Erased"),
        "the person is gone, the row is not"
    );
}

#[tokio::test]
async fn the_events_erasure_depends_on_are_never_dropped_by_the_diagnostics_cap() {
    let Some(pool) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let owner = make_user(&pool).await;
    let session = make_session(&pool, owner).await;
    // A flapping connection fills the bounded diagnostics...
    for _ in 0..230 {
        voxtranslate_server::live::sessions::record_event(&pool, session, "connected", json!({}))
            .await;
    }
    let capped: i64 =
        sqlx::query_scalar("SELECT count(*) FROM live_session_events WHERE session_id = $1")
            .bind(session)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(capped, 200, "diagnostics stay bounded");
    // ...but the pointer to a room's content must still be recorded: erasure and finalization
    // find a conversation's rooms through it.
    let room = Uuid::new_v4();
    voxtranslate_server::live::sessions::record_event(
        &pool,
        session,
        "room_session",
        json!({ "call_session_id": room }),
    )
    .await;
    let kept: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM live_session_events WHERE session_id = $1 AND kind = 'room_session'",
    )
    .bind(session)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kept, 1);
}

// ---- 067: AI summaries -------------------------------------------------------------------

#[tokio::test]
async fn the_summary_and_desk_usage_migrations_are_idempotent() {
    let Some(pool) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let _turn = DDL.lock().await;
    for sql in [
        include_str!("../migrations/067_live_session_summaries.sql"),
        include_str!("../migrations/068_live_usage_by_desk_view.sql"),
    ] {
        for _ in 0..2 {
            sqlx::raw_sql(sql)
                .execute(&pool)
                .await
                .expect("must be re-runnable");
        }
    }
}

#[tokio::test]
async fn summaries_are_one_per_language_checked_and_follow_the_conversation_and_its_asker() {
    let Some(pool) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let owner = make_user(&pool).await;
    let org = make_org(&pool, owner).await;
    let asker = make_user(&pool).await;
    let session: Uuid = sqlx::query_scalar(
        "INSERT INTO live_sessions (org_id, host_lang, status, expires_at) VALUES ($1, 'it', 'ended', now() + interval '1 hour') RETURNING id",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    let insert = |lang: &'static str, status: &'static str, summary: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query("INSERT INTO live_session_summaries (session_id, lang, status, summary, created_by) VALUES ($1, $2, $3, $4, $5)")
                .bind(session)
                .bind(lang)
                .bind(status)
                .bind(summary)
                .bind(asker)
                .execute(&pool)
                .await
        }
    };

    insert("en", "ready", "text")
        .await
        .expect("a ready summary");
    let dup = insert("en", "pending", "")
        .await
        .expect_err("one per (session, lang)");
    assert!(is_violation(&dup, "23505"), "{dup:?}");
    for (lang, status, summary, why) in [
        ("EN", "ready", "x", "language codes are stored lower case"),
        ("e", "ready", "x", "too short"),
        ("it", "done", "x", "unknown status"),
        ("fr", "ready", "", "a ready summary is never empty"),
    ] {
        let err = insert(lang, status, summary).await.expect_err(why);
        assert!(is_violation(&err, "23514"), "{why}: {err:?}");
    }
    insert("it", "pending", "")
        .await
        .expect("a pending claim may be empty");

    // The asker's account going away keeps the org's summary; the conversation going away
    // takes it.
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(asker)
        .execute(&pool)
        .await
        .unwrap();
    let by: Vec<Option<Uuid>> =
        sqlx::query_scalar("SELECT created_by FROM live_session_summaries WHERE session_id = $1")
            .bind(session)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(by, vec![None, None]);
    sqlx::query("DELETE FROM live_sessions WHERE id = $1")
        .bind(session)
        .execute(&pool)
        .await
        .unwrap();
    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM live_session_summaries WHERE session_id = $1")
            .bind(session)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(left, 0);
}

// ---- 068: credits per desk per day -------------------------------------------------------

async fn make_desk(pool: &Pool, org: Uuid, name: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO live_desks (org_id, name, public_id) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(org)
    .bind(name)
    .bind(format!("d{}", &Uuid::new_v4().simple().to_string()[..11]))
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A business conversation at `desk` with one guest seat, and a tick per `(seq, payer, amount,
/// ts)`.
async fn desk_session(
    pool: &Pool,
    org: Uuid,
    desk: Option<Uuid>,
    ticks: &[(i64, &str, i32, &str)],
) -> Uuid {
    let session: Uuid = sqlx::query_scalar(
        "INSERT INTO live_sessions (org_id, desk_id, host_lang, status, expires_at) VALUES ($1, $2, 'it', 'ended', now() + interval '1 hour') RETURNING id",
    )
    .bind(org)
    .bind(desk)
    .fetch_one(pool)
    .await
    .unwrap();
    let peer = add_participant(pool, session, "guest").await.unwrap();
    for (seq, payer, amount, ts) in ticks {
        sqlx::query("INSERT INTO live_usage_ticks (session_id, peer_id, tick_seq, payer_kind, amount, ts) VALUES ($1, $2, $3, $4, $5, $6::timestamptz)")
            .bind(session)
            .bind(peer)
            .bind(seq)
            .bind(payer)
            .bind(rust_decimal::Decimal::from(*amount))
            .bind(ts)
            .execute(pool)
            .await
            .unwrap();
    }
    session
}

#[tokio::test]
async fn the_desk_usage_view_sums_org_credits_per_desk_and_day_and_exposes_nothing_personal() {
    let Some(pool) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let owner = make_user(&pool).await;
    let org = make_org(&pool, owner).await;
    let other_org = make_org(&pool, owner).await;
    let front = make_desk(&pool, org, "Front desk").await;
    let spa = make_desk(&pool, org, "Spa").await;
    let foreign = make_desk(&pool, other_org, "Elsewhere").await;

    // Front desk: two conversations on the 3rd (5+3 and 4), one on the 4th (7); a consumer-paid
    // tick, which is not the org's credits, is left out.
    desk_session(
        &pool,
        org,
        Some(front),
        &[
            (1, "org", 5, "2026-03-03 10:00+00"),
            (2, "org", 3, "2026-03-03 23:59:30+00"),
            (3, "user", 9, "2026-03-03 11:00+00"),
        ],
    )
    .await;
    desk_session(
        &pool,
        org,
        Some(front),
        &[(1, "org", 4, "2026-03-03 12:00+00")],
    )
    .await;
    desk_session(
        &pool,
        org,
        Some(front),
        &[(1, "org", 7, "2026-03-04 00:00:30+00")],
    )
    .await;
    desk_session(
        &pool,
        org,
        Some(spa),
        &[(1, "org", 2, "2026-03-03 09:00+00")],
    )
    .await;
    // Never part of the view: a conversation whose desk is gone, another organisation's desk.
    desk_session(&pool, org, None, &[(1, "org", 100, "2026-03-03 09:00+00")]).await;
    desk_session(
        &pool,
        other_org,
        Some(foreign),
        &[(1, "org", 100, "2026-03-03 09:00+00")],
    )
    .await;

    let rows: Vec<(String, Uuid, Uuid, chrono::NaiveDate, i64, i64)> = sqlx::query_as(
        "SELECT id, org_id, desk_id, day, sessions, credits FROM live_desk_usage WHERE org_id = $1 ORDER BY day, desk_id = $2 DESC",
    )
    .bind(org)
    .bind(front)
    .fetch_all(&pool)
    .await
    .unwrap();
    let day = |d: &str| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").unwrap();
    assert_eq!(
        rows,
        vec![
            (
                format!("{front}:2026-03-03"),
                org,
                front,
                day("2026-03-03"),
                2,
                12
            ),
            (
                format!("{spa}:2026-03-03"),
                org,
                spa,
                day("2026-03-03"),
                1,
                2
            ),
            (
                format!("{front}:2026-03-04"),
                org,
                front,
                day("2026-03-04"),
                1,
                7
            ),
        ]
    );
    let ids: std::collections::HashSet<&String> = rows.iter().map(|r| &r.0).collect();
    assert_eq!(ids.len(), rows.len(), "the surrogate id is unique");

    // Metadata only: no name, no text, no language, no token.
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns WHERE table_name = 'live_desk_usage' ORDER BY ordinal_position",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        columns,
        ["id", "org_id", "desk_id", "day", "sessions", "credits"]
    );
}

#[tokio::test]
async fn the_desk_usage_view_does_not_widen_access_beyond_the_tables_under_it() {
    let Some(pool) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    // On Postgres 15+ the view must run with the CALLER's rights (so RLS on the live tables
    // still applies to anyone but the owner), not its owner's.
    let version: i32 = sqlx::query_scalar("SELECT current_setting('server_version_num')::int")
        .fetch_one(&pool)
        .await
        .unwrap();
    if version >= 150_000 {
        let options: Option<Vec<String>> = sqlx::query_scalar(
            "SELECT reloptions FROM pg_class WHERE relname = 'live_desk_usage' AND relnamespace = 'public'::regnamespace",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            options
                .unwrap_or_default()
                .iter()
                .any(|o| o == "security_invoker=true"),
            "security_invoker must be on"
        );
    }
    // Where the Supabase roles exist they hold no privilege on it.
    for role in ["anon", "authenticated"] {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
                .bind(role)
                .fetch_one(&pool)
                .await
                .unwrap();
        if exists {
            let can: bool = sqlx::query_scalar(
                "SELECT has_table_privilege($1, 'public.live_desk_usage', 'SELECT')",
            )
            .bind(role)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(!can, "{role} must not read live_desk_usage");
        }
    }
}

// ---- 068: the table Directus charts from -------------------------------------------------

async fn daily(pool: &Pool, org: Uuid) -> Vec<(String, i64, i64)> {
    sqlx::query_as(
        "SELECT id, sessions::bigint, credits::bigint FROM live_desk_usage_daily WHERE org_id = $1 ORDER BY id",
    )
    .bind(org)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn the_rollup_table_follows_the_view_and_a_refresh_changes_only_what_differs() {
    let Some(pool) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let _turn = DDL.lock().await;
    let owner = make_user(&pool).await;
    let org = make_org(&pool, owner).await;
    let front = make_desk(&pool, org, "Front desk").await;
    let spa = make_desk(&pool, org, "Spa").await;
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let now = format!("{today} 10:00+00");
    let front_session = desk_session(&pool, org, Some(front), &[(1, "org", 5, &now)]).await;
    let spa_session = desk_session(
        &pool,
        org,
        Some(spa),
        &[(1, "org", 2, &now), (2, "user", 9, &now)],
    )
    .await;

    voxtranslate_server::live::dashboard::refresh_desk_usage(&pool)
        .await
        .unwrap();
    let rows = daily(&pool, org).await;
    assert!(rows.contains(&(format!("{front}:{today}"), 1, 5)));
    assert!(
        rows.contains(&(format!("{spa}:{today}"), 1, 2)),
        "the consumer-paid tick is not counted: {rows:?}"
    );

    // A second refresh with nothing new changes nothing for this organisation.
    let before: Vec<(String, chrono::DateTime<Utc>)> = sqlx::query_as(
        "SELECT id, refreshed_at FROM live_desk_usage_daily WHERE org_id = $1 ORDER BY id",
    )
    .bind(org)
    .fetch_all(&pool)
    .await
    .unwrap();
    voxtranslate_server::live::dashboard::refresh_desk_usage(&pool)
        .await
        .unwrap();
    let after: Vec<(String, chrono::DateTime<Utc>)> = sqlx::query_as(
        "SELECT id, refreshed_at FROM live_desk_usage_daily WHERE org_id = $1 ORDER BY id",
    )
    .bind(org)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(before, after, "an unchanged row is left alone");

    // New usage corrects the row; a conversation that loses its desk drops out of the table.
    let peer: Uuid =
        sqlx::query_scalar("SELECT peer_id FROM live_participants WHERE session_id = $1")
            .bind(front_session)
            .fetch_one(&pool)
            .await
            .unwrap();
    sqlx::query("INSERT INTO live_usage_ticks (session_id, peer_id, tick_seq, payer_kind, amount, ts) VALUES ($1, $2, 2, 'org', 4, $3::timestamptz)")
        .bind(front_session)
        .bind(peer)
        .bind(&now)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE live_sessions SET desk_id = NULL WHERE id = $1")
        .bind(spa_session)
        .execute(&pool)
        .await
        .unwrap();
    voxtranslate_server::live::dashboard::refresh_desk_usage(&pool)
        .await
        .unwrap();
    assert_eq!(
        daily(&pool, org).await,
        vec![(format!("{front}:{today}"), 1, 9)]
    );

    // History outside the window is never touched by a refresh.
    sqlx::query("INSERT INTO live_desk_usage_daily (id, org_id, desk_id, day, sessions, credits) VALUES ($1, $2, $3, current_date - 400, 1, 1)")
        .bind(format!("{spa}:old"))
        .bind(org)
        .bind(spa)
        .execute(&pool)
        .await
        .unwrap();
    voxtranslate_server::live::dashboard::refresh_desk_usage(&pool)
        .await
        .unwrap();
    assert_eq!(
        daily(&pool, org).await.len(),
        2,
        "old history survives a refresh"
    );
}
