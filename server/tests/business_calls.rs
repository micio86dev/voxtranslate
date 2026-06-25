//! Integration tests for the Business calls layer (spec 0106, Phase 2 PR-B):
//! room→org binding, org credit ledger, recording guards, transcript
//! read/translate(cache)/export, and call history. DB-gated.
//!
//! The recording happy-path and live transcription need Supabase Storage +
//! Deepgram, so those aren't exercised here; the local environment has no storage
//! configured, which lets us assert the guard paths instead. Run:
//! `DATABASE_URL=postgres://postgres:postgres@localhost:5433/voxtest \
//!   cargo test --test business_calls`.

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::Client;
use serde_json::{json, Value};
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::business::credits::{add_org_credits, deduct_org_credits, OrgCharge};
use voxtranslate_server::config::Config;
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::transcripts::TranscriptService;
use voxtranslate_server::{app, db, AppState};

const SECRET: &str = "biz-calls-secret";

struct Server {
    addr: SocketAddr,
    pool: db::Pool,
}

async fn setup() -> Option<Server> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let config = Config::test_with_billing(&url, SECRET, 0.0);
    let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);
    let mut state = AppState::new(config);
    state.billing = Some(BillingService::new(pool.clone(), min_join));
    state.safety = Some(SafetyService::new(pool.clone()));
    state.transcripts = Some(TranscriptService::new(pool.clone()));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    Some(Server { addr, pool })
}

fn base(srv: &Server) -> String {
    format!("http://{}", srv.addr)
}

async fn user(srv: &Server) -> (Uuid, String) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: "Caller".into(),
        avatar_url: None,
    };
    let u = upsert_google_user(&srv.pool, &identity, rust_decimal::Decimal::ZERO, None)
        .await
        .unwrap();
    let jwt = issue_jwt(SECRET, &u.id, &u.email, &u.name, 168).unwrap();
    (u.id, jwt)
}

/// Create an org (owner = `owner`) directly in the DB; returns its id.
async fn make_org(srv: &Server, owner: Uuid) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id) VALUES ('Calls Co', $1, $2) RETURNING id",
    )
    .bind(format!("calls-{}", Uuid::new_v4().simple()))
    .bind(owner)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(id)
    .bind(owner)
    .execute(&srv.pool)
    .await
    .unwrap();
    id
}

/// Insert a business call_session; returns its id.
async fn make_call(srv: &Server, org: Uuid, status: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO call_sessions (id, room, org_id, transcript_status) VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(format!("room-{}", Uuid::new_v4().simple()))
    .bind(org)
    .bind(status)
    .execute(&srv.pool)
    .await
    .unwrap();
    id
}

#[tokio::test]
async fn org_credit_ledger_add_deduct_insufficient() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let (owner, _) = user(&srv).await;
    let org = make_org(&srv, owner).await;

    assert_eq!(
        add_org_credits(&srv.pool, org, 10, "purchase", "topup", None)
            .await
            .unwrap(),
        10
    );

    match deduct_org_credits(&srv.pool, org, 3, "recording", None, None, "rec")
        .await
        .unwrap()
    {
        OrgCharge::Charged { balance_after } => assert_eq!(balance_after, 7),
        OrgCharge::Insufficient { .. } => panic!("should have charged"),
    }

    match deduct_org_credits(&srv.pool, org, 100, "recording", None, None, "rec")
        .await
        .unwrap()
    {
        OrgCharge::Insufficient { balance, required } => {
            assert_eq!(balance, 7);
            assert_eq!(required, 100);
        }
        OrgCharge::Charged { .. } => panic!("should be insufficient"),
    }

    // Balance is 7; exactly two ledger rows (the +10 and the -3) — the failed
    // deduction wrote nothing.
    let balance: i32 =
        sqlx::query_scalar("SELECT credits_balance FROM organizations WHERE id = $1")
            .bind(org)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(balance, 7);
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM organization_credits_transactions WHERE org_id = $1",
    )
    .bind(org)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(rows, 2);
}

#[tokio::test]
async fn room_binding_is_inherited_by_new_call() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner).await;
    let project: Uuid = sqlx::query_scalar(
        "INSERT INTO projects (org_id, name, created_by) VALUES ($1, 'P', $2) RETURNING id",
    )
    .bind(org)
    .bind(owner)
    .fetch_one(&srv.pool)
    .await
    .unwrap();

    // Binding a project from another org is rejected.
    let other_owner = user(&srv).await.0;
    let other_org = make_org(&srv, other_owner).await;
    let bad = http
        .patch(format!("{}/api/rooms/inherit-room/business", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "org_id": org, "project_id": Uuid::new_v4() }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    let _ = other_org;

    // Cloud recording requires an ACTIVE subscription — rejected without one.
    let no_sub = http
        .patch(format!("{}/api/rooms/inherit-room/business", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "org_id": org, "project_id": project, "cloud_recording_enabled": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        no_sub.status(),
        403,
        "recording needs an active subscription"
    );

    // Activate the org's subscription; now the recording binding is allowed.
    sqlx::query("UPDATE organizations SET subscription_status = 'active' WHERE id = $1")
        .bind(org)
        .execute(&srv.pool)
        .await
        .unwrap();

    // Bind the room to org + project with recording on.
    let bind = http
        .patch(format!("{}/api/rooms/inherit-room/business", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "org_id": org, "project_id": project, "cloud_recording_enabled": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(bind.status(), 200);

    // A new call on that room inherits org/project/recording from the binding.
    let svc = TranscriptService::new(srv.pool.clone());
    let sid = Uuid::new_v4();
    svc.session_started(sid, "inherit-room").await.unwrap();
    let (got_org, got_project, recording): (Option<Uuid>, Option<Uuid>, bool) = sqlx::query_as(
        "SELECT org_id, project_id, cloud_recording_enabled FROM call_sessions WHERE id = $1",
    )
    .bind(sid)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(got_org, Some(org));
    assert_eq!(got_project, Some(project));
    assert!(recording);
}

#[tokio::test]
async fn recording_guards() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner).await;
    let sid = make_call(&srv, org, "none").await;
    let upload = format!(
        "{}/api/business/rooms/{sid}/recording/upload-url",
        base(&srv)
    );
    let complete = format!("{}/api/business/rooms/{sid}/recording/complete", base(&srv));

    // No token → 401 on both endpoints.
    assert_eq!(http.post(&upload).send().await.unwrap().status(), 401);
    assert_eq!(
        http.post(&complete)
            .json(&json!({ "object_path": format!("{org}/{sid}/x.webm"), "duration_seconds": 30 }))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );

    // A non-member → 404 (call existence hidden).
    let (_outsider, jwt_out) = user(&srv).await;
    assert_eq!(
        http.post(&upload)
            .bearer_auth(&jwt_out)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );

    // A member, but recording storage isn't configured in this env → 503 (no charge).
    assert_eq!(
        http.post(&upload)
            .bearer_auth(&jwt)
            .send()
            .await
            .unwrap()
            .status(),
        503
    );

    // `complete` rejects a path outside this org+call's namespace (anti-tampering).
    let bad = http
        .post(&complete)
        .bearer_auth(&jwt)
        .json(&json!({ "object_path": "other-org/x/y.webm", "duration_seconds": 30 }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
}

#[tokio::test]
async fn transcript_read_translate_cache_and_export() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner).await;
    let sid = make_call(&srv, org, "ready").await;

    let segments = json!([
        {"speaker_id":"0","speaker_name":"Speaker 1","text":"hello there","start_ms":0,"end_ms":900},
        {"speaker_id":"1","speaker_name":"Speaker 2","text":"hi","start_ms":1000,"end_ms":1500}
    ]);
    sqlx::query(
        "INSERT INTO transcripts (session_id, org_id, source_language, segments, translations, word_count, duration_seconds, processed_at)
         VALUES ($1, $2, 'en', $3, $4, 3, 2, now())",
    )
    .bind(sid)
    .bind(org)
    .bind(&segments)
    .bind(json!({ "it": "Ciao a tutti" }))
    .execute(&srv.pool)
    .await
    .unwrap();

    // GET transcript (member) returns the segments + ready status + cached langs.
    let got: Value = http
        .get(format!(
            "{}/api/business/rooms/{sid}/transcript",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["status"], "ready");
    assert_eq!(got["source_language"], "en");
    assert_eq!(got["segments"][0]["text"], "hello there");
    assert_eq!(got["translated_languages"][0], "it");

    // Translate to the cached language → cached, 0 credits, no Groq call.
    let it: Value = http
        .post(format!(
            "{}/api/business/rooms/{sid}/transcript/translate",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .json(&json!({ "target_language": "it" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(it["cached"], true);
    assert_eq!(it["credits_deducted"], 0);
    assert_eq!(it["text"], "Ciao a tutti");

    // Translate to the source language → flattened original, 0 credits.
    let en: Value = http
        .post(format!(
            "{}/api/business/rooms/{sid}/transcript/translate",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .json(&json!({ "target_language": "en" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(en["cached"], true);
    assert!(en["text"]
        .as_str()
        .unwrap()
        .contains("Speaker 1: hello there"));

    // Export TXT (original).
    let txt = http
        .get(format!(
            "{}/api/business/rooms/{sid}/transcript/export?format=txt",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(txt.status(), 200);
    let txt_body = txt.text().await.unwrap();
    assert!(txt_body.contains("Speaker 1: hello there"));
    assert!(txt_body.contains("Speaker 2: hi"));

    // Export PDF renders via the embedded engine (no external calls).
    let pdf = http
        .get(format!(
            "{}/api/business/rooms/{sid}/transcript/export?format=pdf",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(pdf.status(), 200);
    assert_eq!(pdf.headers()["content-type"], "application/pdf");
    let pdf_bytes = pdf.bytes().await.unwrap();
    assert!(pdf_bytes.starts_with(b"%PDF"));
}

#[tokio::test]
async fn history_pagination_and_isolation() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner).await;
    for _ in 0..3 {
        make_call(&srv, org, "none").await;
    }

    // List org rooms — all 3 visible, paginated.
    let list: Value = http
        .get(format!(
            "{}/api/business/organizations/{org}/rooms?limit=2&page=1",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["limit"], 2);
    assert_eq!(list["rooms"].as_array().unwrap().len(), 2);

    let page2: Value = http
        .get(format!(
            "{}/api/business/organizations/{org}/rooms?limit=2&page=2",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page2["rooms"].as_array().unwrap().len(), 1);

    // A user from another org can't read this org's history → 404.
    let (_b, jwt_b) = user(&srv).await;
    let denied = http
        .get(format!(
            "{}/api/business/organizations/{org}/rooms",
            base(&srv)
        ))
        .bearer_auth(&jwt_b)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 404);
}

#[tokio::test]
async fn analytics_summary_aggregates_calls_and_spend() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner).await;

    // Two calls (one with a ready transcript), and some recording spend.
    make_call(&srv, org, "ready").await;
    make_call(&srv, org, "none").await;
    add_org_credits(&srv.pool, org, 100, "purchase", "topup", None)
        .await
        .unwrap();
    deduct_org_credits(&srv.pool, org, 7, "recording", None, None, "rec")
        .await
        .unwrap();

    let http = Client::new();
    let res = http
        .get(format!(
            "{}/api/business/organizations/{org}/analytics?days=30",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();

    assert!(body["kpis"]["calls"].as_i64().unwrap() >= 2);
    assert!(body["kpis"]["transcripts"].as_i64().unwrap() >= 1);
    assert_eq!(body["kpis"]["credits_spent"].as_i64().unwrap(), 7);
    // Spend is bucketed by ledger type.
    let by_type = body["credits_by_type"].as_array().unwrap();
    assert!(by_type
        .iter()
        .any(|t| t["type"] == "recording" && t["spent"].as_i64().unwrap() >= 7));
    // Per-day series is present.
    assert!(!body["calls_by_day"].as_array().unwrap().is_empty());

    // A non-member is denied (404 — org not visible to them).
    let (_b, jwt_b) = user(&srv).await;
    let denied = http
        .get(format!(
            "{}/api/business/organizations/{org}/analytics",
            base(&srv)
        ))
        .bearer_auth(&jwt_b)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 404);
}

#[tokio::test]
async fn member_analytics_tracks_calls_spend_and_collaborators() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner).await;
    let session = make_call(&srv, org, "ready").await;

    // Owner in the call for 30 min, alongside a guest collaborator "Bob".
    sqlx::query(
        "INSERT INTO session_participants (session_id, peer_id, user_id, name, lang, joined_at, left_at)
         VALUES ($1, 'p-owner', $2, 'Owner', 'en', now() - interval '40 min', now() - interval '10 min')",
    )
    .bind(session)
    .bind(owner)
    .execute(&srv.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO session_participants (session_id, peer_id, user_id, name, lang, joined_at, left_at)
         VALUES ($1, 'p-bob', NULL, 'Bob', 'it', now() - interval '40 min', now() - interval '10 min')",
    )
    .bind(session)
    .execute(&srv.pool)
    .await
    .unwrap();

    // Spend attributed to the owner (actor_id).
    add_org_credits(&srv.pool, org, 100, "purchase", "topup", None)
        .await
        .unwrap();
    deduct_org_credits(
        &srv.pool,
        org,
        5,
        "recording",
        Some(session),
        Some(owner),
        "rec",
    )
    .await
    .unwrap();

    let http = Client::new();
    let body: Value = http
        .get(format!(
            "{}/api/business/organizations/{org}/members/{owner}/analytics?days=30",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["calls"], 1);
    assert!(body["minutes_in_calls"].as_i64().unwrap() >= 29);
    assert_eq!(body["credits_spent"], 5);
    assert!(body["collaborators"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["name"] == "Bob"));

    // A user who isn't a member of this org → 404.
    let (stranger, _) = user(&srv).await;
    let nf = http
        .get(format!(
            "{}/api/business/organizations/{org}/members/{stranger}/analytics",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(nf.status(), 404);
}

#[tokio::test]
async fn storyboard_guards_and_preconditions() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org = make_org(&srv, owner).await;
    let project: Uuid = sqlx::query_scalar(
        "INSERT INTO projects (org_id, name, created_by) VALUES ($1, 'P', $2) RETURNING id",
    )
    .bind(org)
    .bind(owner)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    let sb_url = format!(
        "{}/api/business/organizations/{org}/projects/{project}/storyboard",
        base(&srv)
    );

    // No storyboard yet → 404.
    let none = http.get(&sb_url).bearer_auth(&jwt).send().await.unwrap();
    assert_eq!(none.status(), 404);

    // Generate with no calls in the project → 400 (nothing to summarize; no Groq call).
    let empty = http
        .post(&sb_url)
        .bearer_auth(&jwt)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(empty.status(), 400);

    // Add a call to the project; org has 0 credits → 402 (pre-check before Groq).
    let sid = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO call_sessions (id, room, org_id, project_id, transcript_status)
         VALUES ($1, $2, $3, $4, 'none')",
    )
    .bind(sid)
    .bind(format!("room-{}", Uuid::new_v4().simple()))
    .bind(org)
    .bind(project)
    .execute(&srv.pool)
    .await
    .unwrap();
    let broke = http
        .post(&sb_url)
        .bearer_auth(&jwt)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(broke.status(), 402);

    // A non-admin member can't generate → 403.
    let (b, jwt_b) = user(&srv).await;
    sqlx::query(
        "INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, 'member')",
    )
    .bind(org)
    .bind(b)
    .execute(&srv.pool)
    .await
    .unwrap();
    let denied = http
        .post(&sb_url)
        .bearer_auth(&jwt_b)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 403);

    // Generate for a non-existent project → 404.
    let ghost = http
        .post(format!(
            "{}/api/business/organizations/{org}/projects/{}/storyboard",
            base(&srv),
            Uuid::new_v4()
        ))
        .bearer_auth(&jwt)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(ghost.status(), 404);
}
