//! Integration tests for the org members/invites EDGE cases that the happy-path
//! suite (`business_orgs.rs`) doesn't reach: invalid email/role, expired/accepted
//! invites, the member-limit 409, last-owner protection, and role-change guards.
//! DB-gated — skipped without `DATABASE_URL`.

use std::net::SocketAddr;
use std::sync::Arc;

use chrono::{Duration, Utc};
use reqwest::Client;
use serde_json::{json, Value};
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::Config;
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::{app, db, AppState};

const SECRET: &str = "members-secret";

struct Server {
    addr: SocketAddr,
    pool: db::Pool,
}

/// Build the app with a deliberately tiny member limit (1) so the limit-409 path
/// fires with just the owner already present.
async fn setup() -> Option<Server> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let mut config = Config::test_with_billing(&url, SECRET, 0.0);
    config.business_member_limit = 1; // owner alone already fills a business org
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
        .json(&json!({ "name": "Mem Co", "slug": format!("org-{}", Uuid::new_v4().simple()) }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "create org");
    let body: Value = r.json().await.unwrap();
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

/// Insert an invite row directly so its expiry / accepted state is controllable.
async fn seed_invite(
    srv: &Server,
    org: Uuid,
    inviter: Uuid,
    token: &str,
    expires_in_hours: i64,
    accepted: bool,
) {
    sqlx::query(
        "INSERT INTO organization_invites (org_id, email, role, invited_by, token, expires_at, accepted_at)
         VALUES ($1, $2, 'member', $3, $4, $5, $6)",
    )
    .bind(org)
    .bind(format!("{}@invitee.test", Uuid::new_v4()))
    .bind(inviter)
    .bind(token)
    .bind(Utc::now() + Duration::hours(expires_in_hours))
    .bind(if accepted { Some(Utc::now()) } else { None })
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

#[tokio::test]
async fn create_invite_validates_email_and_role() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (_owner, jwt) = user(&srv, "Owner").await;
    let org = create_org(&http, &srv, &jwt).await;
    let url = format!("{}/api/business/organizations/{}/invites", base(&srv), org);

    let bad_email = http
        .post(&url)
        .bearer_auth(&jwt)
        .json(&json!({ "email": "not-an-email" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_email.status(), 400);

    let bad_role = http
        .post(&url)
        .bearer_auth(&jwt)
        .json(&json!({ "email": "ok@example.com", "role": "superadmin" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_role.status(), 400);
}

#[tokio::test]
async fn get_invite_missing_expired_and_accepted() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org = create_org(&http, &srv, &jwt).await;

    // Unknown token → 404.
    let missing = http
        .get(format!(
            "{}/api/business/invites/{}",
            base(&srv),
            Uuid::new_v4()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);

    // Expired → 410.
    let expired_tok = format!("tok-exp-{}", Uuid::new_v4());
    seed_invite(&srv, org, owner, &expired_tok, -1, false).await;
    let expired = http
        .get(format!(
            "{}/api/business/invites/{}",
            base(&srv),
            expired_tok
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(expired.status(), 410);

    // Already accepted → 410.
    let accepted_tok = format!("tok-acc-{}", Uuid::new_v4());
    seed_invite(&srv, org, owner, &accepted_tok, 48, true).await;
    let accepted = http
        .get(format!(
            "{}/api/business/invites/{}",
            base(&srv),
            accepted_tok
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(accepted.status(), 410);
}

#[tokio::test]
async fn accept_invite_expired_and_member_limit() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, owner_jwt) = user(&srv, "Owner").await;
    let org = create_org(&http, &srv, &owner_jwt).await;

    // Expired invite accept → 410.
    let exp_tok = format!("tok-exp-{}", Uuid::new_v4());
    seed_invite(&srv, org, owner, &exp_tok, -2, false).await;
    let (_u2, u2_jwt) = user(&srv, "Late Joiner").await;
    let late = http
        .post(format!(
            "{}/api/business/invites/{}/accept",
            base(&srv),
            exp_tok
        ))
        .bearer_auth(&u2_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(late.status(), 410);

    // Valid invite but the org is already at its member limit (1) → 409.
    let ok_tok = format!("tok-ok-{}", Uuid::new_v4());
    seed_invite(&srv, org, owner, &ok_tok, 48, false).await;
    let (_u3, u3_jwt) = user(&srv, "Over Limit").await;
    let over = http
        .post(format!(
            "{}/api/business/invites/{}/accept",
            base(&srv),
            ok_tok
        ))
        .bearer_auth(&u3_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(over.status(), 409, "member limit reached");
}

#[tokio::test]
async fn remove_member_guards() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (owner, owner_jwt) = user(&srv, "Owner").await;
    let org = create_org(&http, &srv, &owner_jwt).await;

    // Removing the owner is forbidden (last-owner protection) → 403.
    let rm_owner = http
        .delete(format!(
            "{}/api/business/organizations/{}/members/{}",
            base(&srv),
            org,
            owner
        ))
        .bearer_auth(&owner_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(rm_owner.status(), 403);

    // Removing a non-member → 404.
    let rm_ghost = http
        .delete(format!(
            "{}/api/business/organizations/{}/members/{}",
            base(&srv),
            org,
            Uuid::new_v4()
        ))
        .bearer_auth(&owner_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(rm_ghost.status(), 404);

    // A plain member can't remove anyone → 403.
    let (member_id, member_jwt) = user(&srv, "Plain").await;
    sqlx::query(
        "INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, 'member')",
    )
    .bind(org)
    .bind(member_id)
    .execute(&srv.pool)
    .await
    .unwrap();
    let by_member = http
        .delete(format!(
            "{}/api/business/organizations/{}/members/{}",
            base(&srv),
            org,
            owner
        ))
        .bearer_auth(&member_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(by_member.status(), 403);
}

#[tokio::test]
async fn change_role_validates_role_and_target() {
    let srv = skip_without_db!(setup().await);
    let http = Client::new();
    let (_owner, owner_jwt) = user(&srv, "Owner").await;
    let org = create_org(&http, &srv, &owner_jwt).await;

    // Invalid role string → 400.
    let bad = http
        .patch(format!(
            "{}/api/business/organizations/{}/members/{}",
            base(&srv),
            org,
            Uuid::new_v4()
        ))
        .bearer_auth(&owner_jwt)
        .json(&json!({ "role": "wizard" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);

    // Valid role but the target isn't a member → 404.
    let ghost = http
        .patch(format!(
            "{}/api/business/organizations/{}/members/{}",
            base(&srv),
            org,
            Uuid::new_v4()
        ))
        .bearer_auth(&owner_jwt)
        .json(&json!({ "role": "admin" }))
        .send()
        .await
        .unwrap();
    assert_eq!(ghost.status(), 404);
}
