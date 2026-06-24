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

use reqwest::header::CONTENT_TYPE;
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

/// A minimal multipart/form-data body (reqwest has no multipart feature here).
fn multipart(duration: &str, file: &[u8]) -> (String, Vec<u8>) {
    let b = "BoUnDaRy1234567";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{b}\r\nContent-Disposition: form-data; name=\"duration_seconds\"\r\n\r\n{duration}\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(
        format!(
            "--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"rec.webm\"\r\n\
             Content-Type: audio/webm\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(file);
    body.extend_from_slice(format!("\r\n--{b}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={b}"), body)
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

    match deduct_org_credits(&srv.pool, org, 3, "recording", None, "rec")
        .await
        .unwrap()
    {
        OrgCharge::Charged { balance_after } => assert_eq!(balance_after, 7),
        OrgCharge::Insufficient { .. } => panic!("should have charged"),
    }

    match deduct_org_credits(&srv.pool, org, 100, "recording", None, "rec")
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
    let (ct, body) = multipart("30", b"\x00\x01\x02");

    // No token → 401.
    let anon = http
        .post(format!(
            "{}/api/business/rooms/{sid}/recording/complete",
            base(&srv)
        ))
        .header(CONTENT_TYPE, &ct)
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);

    // A non-member → 404 (call existence hidden).
    let (_outsider, jwt_out) = user(&srv).await;
    let outsider = http
        .post(format!(
            "{}/api/business/rooms/{sid}/recording/complete",
            base(&srv)
        ))
        .bearer_auth(&jwt_out)
        .header(CONTENT_TYPE, &ct)
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(outsider.status(), 404);

    // A member, but storage isn't configured in this env → 503 (no charge).
    let member = http
        .post(format!(
            "{}/api/business/rooms/{sid}/recording/complete",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .header(CONTENT_TYPE, &ct)
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(member.status(), 503);
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
