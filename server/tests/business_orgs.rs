//! Integration tests for the Business org API (spec 0106, Phase 2 PR-A):
//! organizations, members, invites, projects — auth + role guards, happy paths,
//! cross-tenant isolation, and the full invite flow.
//!
//! DB-gated like `tests/billing.rs`: no-ops without `DATABASE_URL`. Locally:
//! `DATABASE_URL=postgres://postgres:postgres@localhost:5433/voxtest \
//!   cargo test --test business_orgs`.

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

const SECRET: &str = "biz-secret";

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

/// Create a fresh user; returns its id and a session JWT.
async fn user(srv: &Server) -> (Uuid, String) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: "Biz Tester".into(),
        avatar_url: None,
    };
    let u = upsert_google_user(&srv.pool, &identity, rust_decimal::Decimal::ZERO, None)
        .await
        .unwrap();
    let jwt = issue_jwt(SECRET, &u.id, &u.email, &u.name, 168).unwrap();
    (u.id, jwt)
}

fn base(srv: &Server) -> String {
    format!("http://{}", srv.addr)
}

/// Create an org and return its id (panics on non-200).
async fn create_org(http: &Client, srv: &Server, jwt: &str, name: &str) -> Uuid {
    let slug = format!("org-{}", Uuid::new_v4().simple());
    let r = http
        .post(format!("{}/api/business/organizations", base(srv)))
        .bearer_auth(jwt)
        .json(&json!({ "name": name, "slug": slug }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "create org");
    let body: Value = r.json().await.unwrap();
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn auth_guard_rejects_anonymous() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();

    // No token → 401 on both the collection and a creation attempt.
    let list = http
        .get(format!("{}/api/business/organizations", base(&srv)))
        .send()
        .await
        .unwrap();
    assert_eq!(list.status(), 401);

    let create = http
        .post(format!("{}/api/business/organizations", base(&srv)))
        .json(&json!({ "name": "X", "slug": "x-org" }))
        .send()
        .await
        .unwrap();
    assert_eq!(create.status(), 401);
}

#[tokio::test]
async fn org_create_list_get_patch() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (_uid, jwt) = user(&srv).await;

    // Create.
    let org_id = create_org(&http, &srv, &jwt, "Acme Inc").await;

    // Invalid slug → 400.
    let bad = http
        .post(format!("{}/api/business/organizations", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "name": "Bad", "slug": "Has Spaces" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);

    // List shows the org with role owner.
    let list: Value = http
        .get(format!("{}/api/business/organizations", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let arr = list.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["role"], "owner");
    assert_eq!(arr[0]["credits_balance"], 0);

    // Detail carries the default settings + the caller's role.
    let detail: Value = http
        .get(format!(
            "{}/api/business/organizations/{org_id}",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail["role"], "owner");
    assert_eq!(detail["settings"]["retention_days"], 90);

    // Patch the name (owner/admin).
    let patched: Value = http
        .patch(format!(
            "{}/api/business/organizations/{org_id}",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .json(&json!({ "name": "Acme Renamed" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(patched["name"], "Acme Renamed");
}

#[tokio::test]
async fn cross_tenant_isolation() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (_a, jwt_a) = user(&srv).await;
    let (_b, jwt_b) = user(&srv).await;

    let org_a = create_org(&http, &srv, &jwt_a, "Org A").await;

    // B is not a member of org A → 404 on detail, members, and patch (existence hidden).
    for (method, path) in [
        ("GET", format!("/api/business/organizations/{org_a}")),
        (
            "GET",
            format!("/api/business/organizations/{org_a}/members"),
        ),
    ] {
        let r = http
            .request(method.parse().unwrap(), format!("{}{path}", base(&srv)))
            .bearer_auth(&jwt_b)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404, "{method} {path} for non-member");
    }
    let patch = http
        .patch(format!("{}/api/business/organizations/{org_a}", base(&srv)))
        .bearer_auth(&jwt_b)
        .json(&json!({ "name": "Hijack" }))
        .send()
        .await
        .unwrap();
    assert_eq!(patch.status(), 404);

    // B's own org list is empty.
    let list_b: Value = http
        .get(format!("{}/api/business/organizations", base(&srv)))
        .bearer_auth(&jwt_b)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list_b.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn invite_create_accept_membership() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (_a, jwt_a) = user(&srv).await;
    let (b_id, jwt_b) = user(&srv).await;

    let org = create_org(&http, &srv, &jwt_a, "Team Co").await;

    // Owner invites a member; with no Resend configured the token still comes back.
    let invite: Value = http
        .post(format!(
            "{}/api/business/organizations/{org}/invites",
            base(&srv)
        ))
        .bearer_auth(&jwt_a)
        .json(&json!({ "email": "newbie@example.com", "role": "member" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = invite["token"].as_str().unwrap().to_string();
    assert_eq!(invite["email_sent"], false);
    assert_eq!(invite["role"], "member");

    // The token endpoint is public and reveals the org name + role.
    let info: Value = http
        .get(format!("{}/api/business/invites/{token}", base(&srv)))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["org_name"], "Team Co");
    assert_eq!(info["role"], "member");

    // B accepts → becomes a member.
    let accept = http
        .post(format!(
            "{}/api/business/invites/{token}/accept",
            base(&srv)
        ))
        .bearer_auth(&jwt_b)
        .send()
        .await
        .unwrap();
    assert_eq!(accept.status(), 200);

    // B now sees the org, and the member list includes B.
    let list_b: Value = http
        .get(format!("{}/api/business/organizations", base(&srv)))
        .bearer_auth(&jwt_b)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list_b.as_array().unwrap().len(), 1);

    let members: Value = http
        .get(format!(
            "{}/api/business/organizations/{org}/members",
            base(&srv)
        ))
        .bearer_auth(&jwt_a)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(members
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["user_id"] == b_id.to_string()));

    // Re-accepting the same (now-consumed) token → 410 Gone.
    let again = http
        .post(format!(
            "{}/api/business/invites/{token}/accept",
            base(&srv)
        ))
        .bearer_auth(&jwt_b)
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 410);
}

#[tokio::test]
async fn roles_remove_and_self_leave() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (a_id, jwt_a) = user(&srv).await;
    let (c_id, jwt_c) = user(&srv).await;

    let org = create_org(&http, &srv, &jwt_a, "Roles Co").await;

    // Bring C in as a member via an invite.
    let token = http
        .post(format!(
            "{}/api/business/organizations/{org}/invites",
            base(&srv)
        ))
        .bearer_auth(&jwt_a)
        .json(&json!({ "email": "c@example.com" }))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    http.post(format!(
        "{}/api/business/invites/{token}/accept",
        base(&srv)
    ))
    .bearer_auth(&jwt_c)
    .send()
    .await
    .unwrap();

    // Owner promotes C to admin.
    let promote = http
        .patch(format!(
            "{}/api/business/organizations/{org}/members/{c_id}",
            base(&srv)
        ))
        .bearer_auth(&jwt_a)
        .json(&json!({ "role": "admin" }))
        .send()
        .await
        .unwrap();
    assert_eq!(promote.status(), 200);

    // C (admin, not owner) cannot change roles — owner-only.
    let denied = http
        .patch(format!(
            "{}/api/business/organizations/{org}/members/{c_id}",
            base(&srv)
        ))
        .bearer_auth(&jwt_c)
        .json(&json!({ "role": "member" }))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 403);

    // The owner can never be removed (even by an admin).
    let remove_owner = http
        .delete(format!(
            "{}/api/business/organizations/{org}/members/{a_id}",
            base(&srv)
        ))
        .bearer_auth(&jwt_c)
        .send()
        .await
        .unwrap();
    assert_eq!(remove_owner.status(), 403);

    // C self-leaves → 204.
    let leave = http
        .delete(format!(
            "{}/api/business/organizations/{org}/members/{c_id}",
            base(&srv)
        ))
        .bearer_auth(&jwt_c)
        .send()
        .await
        .unwrap();
    assert_eq!(leave.status(), 204);

    // C is gone from the org.
    let after: Value = http
        .get(format!("{}/api/business/organizations", base(&srv)))
        .bearer_auth(&jwt_c)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(after.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn projects_crud_and_permissions() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (_a, jwt_a) = user(&srv).await;
    let org = create_org(&http, &srv, &jwt_a, "Proj Co").await;

    // Create.
    let proj: Value = http
        .post(format!(
            "{}/api/business/organizations/{org}/projects",
            base(&srv)
        ))
        .bearer_auth(&jwt_a)
        .json(&json!({ "name": "Q3 Sync", "default_languages": ["it", "de"] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let proj_id = proj["id"].as_str().unwrap().to_string();
    assert_eq!(proj["default_languages"][0], "it");

    // Get + patch.
    let patched: Value = http
        .patch(format!(
            "{}/api/business/organizations/{org}/projects/{proj_id}",
            base(&srv)
        ))
        .bearer_auth(&jwt_a)
        .json(&json!({ "name": "Q3 Planning" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(patched["name"], "Q3 Planning");

    // Soft delete → no longer listed.
    let del = http
        .delete(format!(
            "{}/api/business/organizations/{org}/projects/{proj_id}",
            base(&srv)
        ))
        .bearer_auth(&jwt_a)
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 204);

    let list: Value = http
        .get(format!(
            "{}/api/business/organizations/{org}/projects",
            base(&srv)
        ))
        .bearer_auth(&jwt_a)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list.as_array().unwrap().len(), 0);
}
