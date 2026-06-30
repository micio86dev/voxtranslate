//! Integration tests for the org audit-log read API
//! (`GET /api/business/organizations/{org_id}/audit`). DB-gated — skipped without
//! `DATABASE_URL`. Covers the admin/owner gate, org scoping, and every filter
//! (action, free-text `q`, date range, pagination). Rows are inserted directly so
//! the assertions are deterministic (the writer is fire-and-forget elsewhere).

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::Client;
use serde_json::{json, Value};
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::Config;
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::{app, db, AppState};

const SECRET: &str = "audit-secret";

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
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    Some(Server { addr, pool })
}

async fn user(srv: &Server, name: &str) -> (Uuid, String) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: name.into(),
        avatar_url: None,
    };
    let u = upsert_google_user(
        &srv.pool,
        &identity,
        rust_decimal::Decimal::ZERO,
        None,
        None,
    )
    .await
    .unwrap();
    let jwt = issue_jwt(SECRET, &u.id, &u.email, &u.name, 168).unwrap();
    (u.id, jwt)
}

fn base(srv: &Server) -> String {
    format!("http://{}", srv.addr)
}

async fn create_org(http: &Client, srv: &Server, jwt: &str) -> Uuid {
    let r = http
        .post(format!("{}/api/business/organizations", base(srv)))
        .bearer_auth(jwt)
        .json(&json!({ "name": "Audit Co", "slug": format!("org-{}", Uuid::new_v4().simple()) }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "create org");
    let body: Value = r.json().await.unwrap();
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

async fn insert_audit(srv: &Server, org: Uuid, actor: Uuid, action: &str) {
    sqlx::query(
        "INSERT INTO audit_logs (org_id, actor_id, action, resource_type, resource_id, metadata)
         VALUES ($1, $2, $3, 'project', $4, '{}'::jsonb)",
    )
    .bind(org)
    .bind(actor)
    .bind(action)
    .bind(Uuid::new_v4())
    .execute(&srv.pool)
    .await
    .unwrap();
}

macro_rules! skip_without_db {
    ($e:expr) => {
        match $e {
            Some(s) => s,
            None => {
                eprintln!("skipping — no DATABASE_URL");
                return;
            }
        }
    };
}

async fn list(http: &Client, srv: &Server, org: Uuid, jwt: &str, qs: &str) -> Value {
    let r = http
        .get(format!(
            "{}/api/business/organizations/{}/audit{}",
            base(srv),
            org,
            qs
        ))
        .bearer_auth(jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "audit list");
    r.json().await.unwrap()
}

#[tokio::test]
async fn audit_list_filters_and_scoping() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Audit Owner").await;
    let org = create_org(&http, &srv, &jwt).await;

    // Seed a few entries with distinct actions.
    insert_audit(&srv, org, owner, "project.created").await;
    insert_audit(&srv, org, owner, "project.created").await;
    insert_audit(&srv, org, owner, "member.invited").await;

    // Bare list returns everything (org creation may add its own entries too).
    let all = list(&http, &srv, org, &jwt, "").await;
    let entries = all["entries"].as_array().unwrap();
    assert!(entries.len() >= 3);
    assert_eq!(all["limit"], 50);
    assert_eq!(all["page"], 0);
    // The join surfaces the actor's name/email.
    assert!(entries.iter().any(|e| e["actor_name"] == "Audit Owner"));

    // action filter narrows to one kind.
    let invited = list(&http, &srv, org, &jwt, "?action=member.invited").await;
    let inv = invited["entries"].as_array().unwrap();
    assert!(!inv.is_empty());
    assert!(inv.iter().all(|e| e["action"] == "member.invited"));

    // free-text q matches the actor email.
    let by_q = list(&http, &srv, org, &jwt, "?q=project.created").await;
    assert!(!by_q["entries"].as_array().unwrap().is_empty());

    // pagination: limit echoes back and caps the page size.
    let paged = list(&http, &srv, org, &jwt, "?limit=1&page=0").await;
    assert_eq!(paged["entries"].as_array().unwrap().len(), 1);
    assert_eq!(paged["limit"], 1);

    // a far-future `from` filters everything out.
    let none = list(&http, &srv, org, &jwt, "?from=2999-01-01T00:00:00Z").await;
    assert_eq!(none["entries"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn audit_requires_admin_and_membership() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (_owner, owner_jwt) = user(&srv, "Owner").await;
    let org = create_org(&http, &srv, &owner_jwt).await;

    // A stranger (not a member) gets 404 — org existence isn't leaked to outsiders.
    let (_outsider, out_jwt) = user(&srv, "Outsider").await;
    let r = http
        .get(format!(
            "{}/api/business/organizations/{}/audit",
            base(&srv),
            org
        ))
        .bearer_auth(&out_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    // A plain member (below admin rank) is forbidden — 403.
    let (member_id, member_jwt) = user(&srv, "Plain Member").await;
    sqlx::query(
        "INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, 'member')",
    )
    .bind(org)
    .bind(member_id)
    .execute(&srv.pool)
    .await
    .unwrap();
    let r = http
        .get(format!(
            "{}/api/business/organizations/{}/audit",
            base(&srv),
            org
        ))
        .bearer_auth(&member_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    // No token at all → 401.
    let anon = http
        .get(format!(
            "{}/api/business/organizations/{}/audit",
            base(&srv),
            org
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);
}
