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
    let (u, _) = upsert_google_user(
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

/// Like [`user`] but with a caller-chosen email, so an invite sent to that address
/// can actually be accepted (accept_invite now binds to the invited email, M3).
async fn user_with_email(srv: &Server, email: &str) -> (Uuid, String) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: email.to_string(),
        name: "Biz Tester".into(),
        avatar_url: None,
    };
    let (u, _) = upsert_google_user(
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
    // Unique per run (the test DB persists between runs; email is unique-constrained).
    let member_email = format!("newbie-{}@example.com", Uuid::new_v4());
    let (b_id, jwt_b) = user_with_email(&srv, &member_email).await;

    let org = create_org(&http, &srv, &jwt_a, "Team Co").await;

    // Owner invites a member; with no Resend configured the token still comes back.
    let invite: Value = http
        .post(format!(
            "{}/api/business/organizations/{org}/invites",
            base(&srv)
        ))
        .bearer_auth(&jwt_a)
        .json(&json!({ "email": member_email, "role": "member" }))
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
    let c_email = format!("c-{}@example.com", Uuid::new_v4());
    let (c_id, jwt_c) = user_with_email(&srv, &c_email).await;

    let org = create_org(&http, &srv, &jwt_a, "Roles Co").await;

    // Bring C in as a member via an invite.
    let token = http
        .post(format!(
            "{}/api/business/organizations/{org}/invites",
            base(&srv)
        ))
        .bearer_auth(&jwt_a)
        .json(&json!({ "email": c_email }))
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

#[tokio::test]
async fn compliance_mode_is_enterprise_only() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (_a, jwt) = user(&srv).await;

    // Business org (default plan) cannot enable compliance mode.
    let biz = create_org(&http, &srv, &jwt, "Biz Co").await;
    let denied = http
        .patch(format!("{}/api/business/organizations/{biz}", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "settings": { "compliance_mode": true } }))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 403, "compliance is Enterprise-only");

    // An Enterprise org can.
    let ent: Value = http
        .post(format!("{}/api/business/organizations", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({
            "name": "Ent Co",
            "slug": format!("ent-{}", Uuid::new_v4().simple()),
            "plan": "enterprise",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ent_id = ent["id"].as_str().unwrap();
    let ok = http
        .patch(format!(
            "{}/api/business/organizations/{ent_id}",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .json(&json!({ "settings": { "compliance_mode": true } }))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200, "Enterprise can enable compliance");
}

#[tokio::test]
async fn business_member_cap_enforced_and_lifted_on_enterprise() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (_a, jwt_a) = user(&srv).await;
    let org = create_org(&http, &srv, &jwt_a, "Capped Co").await;

    // Fill the org to its 20-member Business cap (owner + 19 inserted directly).
    sqlx::query(
        "WITH new_users AS (
            INSERT INTO users (google_id, email, name)
            SELECT 'cap-' || g || '-' || $2::text,
                   'cap-' || g || '-' || $2::text || '@x.com',
                   'Cap Member'
            FROM generate_series(1, 19) g
            RETURNING id
         )
         INSERT INTO organization_members (org_id, user_id, role)
         SELECT $1, id, 'member' FROM new_users",
    )
    .bind(org)
    .bind(org.to_string())
    .execute(&srv.pool)
    .await
    .unwrap();

    // Invite a 21st member; accepting is rejected on the Business plan.
    let over_email = format!("over-{}@example.com", Uuid::new_v4());
    let (_b, jwt_b) = user_with_email(&srv, &over_email).await;
    let invite: Value = http
        .post(format!(
            "{}/api/business/organizations/{org}/invites",
            base(&srv)
        ))
        .bearer_auth(&jwt_a)
        .json(&json!({ "email": over_email, "role": "member" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = invite["token"].as_str().unwrap();
    let over = http
        .post(format!(
            "{}/api/business/invites/{token}/accept",
            base(&srv)
        ))
        .bearer_auth(&jwt_b)
        .send()
        .await
        .unwrap();
    assert_eq!(over.status(), 409, "21st member rejected on Business plan");

    // Upgrading to Enterprise lifts the cap — the same pending invite now accepts.
    sqlx::query("UPDATE organizations SET plan = 'enterprise' WHERE id = $1")
        .bind(org)
        .execute(&srv.pool)
        .await
        .unwrap();
    let now_ok = http
        .post(format!(
            "{}/api/business/invites/{token}/accept",
            base(&srv)
        ))
        .bearer_auth(&jwt_b)
        .send()
        .await
        .unwrap();
    assert_eq!(now_ok.status(), 200, "Enterprise is unlimited");
}

/// Invite `accepter_email` to `org` as `role` (owner JWT) and have `accepter_jwt`
/// accept it. The invited address must be the accepter's own email — accept_invite
/// binds to it (M3).
async fn invite_and_accept(
    http: &Client,
    srv: &Server,
    owner_jwt: &str,
    org: Uuid,
    accepter_jwt: &str,
    accepter_email: &str,
    role: &str,
) {
    let invite: Value = http
        .post(format!(
            "{}/api/business/organizations/{org}/invites",
            base(srv)
        ))
        .bearer_auth(owner_jwt)
        .json(&json!({ "email": accepter_email, "role": role }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = invite["token"].as_str().unwrap();
    let accept = http
        .post(format!("{}/api/business/invites/{token}/accept", base(srv)))
        .bearer_auth(accepter_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(accept.status(), 200, "accept invite");
}

#[tokio::test]
async fn teams_crud_and_flexible_membership() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (_a, jwt_a) = user(&srv).await; // owner
    let b_email = format!("member-{}@x.com", Uuid::new_v4());
    let (b_id, jwt_b) = user_with_email(&srv, &b_email).await; // member
    let org = create_org(&http, &srv, &jwt_a, "Teams Co").await;
    invite_and_accept(&http, &srv, &jwt_a, org, &jwt_b, &b_email, "member").await;

    let teams_url = format!("{}/api/business/organizations/{org}/teams", base(&srv));

    // A member can't create a team (ADMIN-gated).
    let denied = http
        .post(&teams_url)
        .bearer_auth(&jwt_b)
        .json(&json!({ "name": "Nope" }))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 403);

    // Owner creates two teams.
    let mk = |name: &str| {
        let url = teams_url.clone();
        let jwt = jwt_a.clone();
        let name = name.to_string();
        let http = http.clone();
        async move {
            let r = http
                .post(&url)
                .bearer_auth(&jwt)
                .json(&json!({ "name": name }))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 201);
            let v: Value = r.json().await.unwrap();
            Uuid::parse_str(v["id"].as_str().unwrap()).unwrap()
        }
    };
    let eng = mk("Engineering").await;
    let sales = mk("Sales").await;

    // Add B to BOTH teams → flexible multi-team membership.
    for team in [eng, sales] {
        let r = http
            .post(format!("{teams_url}/{team}/members"))
            .bearer_auth(&jwt_a)
            .json(&json!({ "user_id": b_id }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 204);
    }
    // Idempotent re-add.
    let again = http
        .post(format!("{teams_url}/{eng}/members"))
        .bearer_auth(&jwt_a)
        .json(&json!({ "user_id": b_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 204);

    // List shows both teams, each with member_count 1.
    let teams: Value = http
        .get(&teams_url)
        .bearer_auth(&jwt_b)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let arr = teams.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert!(arr.iter().all(|t| t["member_count"].as_i64().unwrap() == 1));

    // Team member list includes B.
    let eng_members: Value = http
        .get(format!("{teams_url}/{eng}/members"))
        .bearer_auth(&jwt_a)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(eng_members
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["user_id"] == b_id.to_string()));

    // Adding a NON-org-member is rejected (400).
    let (outsider, _) = user(&srv).await;
    let bad = http
        .post(format!("{teams_url}/{eng}/members"))
        .bearer_auth(&jwt_a)
        .json(&json!({ "user_id": outsider }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);

    // "Move" B out of Sales (remove); Eng membership untouched.
    let removed = http
        .delete(format!("{teams_url}/{sales}/members/{b_id}"))
        .bearer_auth(&jwt_a)
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), 204);
    let sales_members: Value = http
        .get(format!("{teams_url}/{sales}/members"))
        .bearer_auth(&jwt_a)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sales_members.as_array().unwrap().len(), 0);

    // Rename + delete (admin).
    let renamed = http
        .patch(format!("{teams_url}/{eng}"))
        .bearer_auth(&jwt_a)
        .json(&json!({ "name": "Eng" }))
        .send()
        .await
        .unwrap();
    assert_eq!(renamed.status(), 200);
    let del = http
        .delete(format!("{teams_url}/{sales}"))
        .bearer_auth(&jwt_a)
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 204);

    // A non-member can't see this org's teams (404).
    let (_c, jwt_c) = user(&srv).await;
    let outsider_view = http
        .get(&teams_url)
        .bearer_auth(&jwt_c)
        .send()
        .await
        .unwrap();
    assert_eq!(outsider_view.status(), 404);
}
