//! The dashboard's webinar read-side: history list and detail rollup.
//!
//! The interesting rule is the scope. A meet call scopes by participation, but a
//! webinar's audience is anonymous guests — so a plain member sees only the
//! webinars they HOSTED, and admins see the whole org. Everything else here is
//! the paging, filtering and ordering the history list promises.
//!
//! DB-gated: skipped without `DATABASE_URL`.

use std::net::SocketAddr;
use std::sync::Arc;

use chrono::{Duration, Utc};
use reqwest::Client;
use serde_json::Value;
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::Config;
use voxtranslate_server::db::{self, Pool};
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::{app, AppState};

const SECRET: &str = "business-webinars-secret";

struct Server {
    addr: SocketAddr,
    pool: Pool,
}

async fn setup() -> Option<Server> {
    let url = voxtranslate_server::db::test_database_url()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let config = Config::test_with_billing(&url, SECRET, 0.0);
    let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);
    let mut state = AppState::new(config);
    state.billing = Some(BillingService::new(pool.clone(), min_join));
    state.safety = Some(SafetyService::new(pool.clone()));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    Some(Server { addr, pool })
}

fn base(srv: &Server) -> String {
    format!("http://{}", srv.addr)
}

async fn user(srv: &Server, name: &str) -> (Uuid, String) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: name.into(),
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

async fn org(srv: &Server, owner: Uuid) -> Uuid {
    let org_id: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id, subscription_status, current_period_end)
         VALUES ('Webinar Co', $1, $2, 'active', now() + interval '30 days') RETURNING id",
    )
    .bind(format!("wb-{}", Uuid::new_v4().simple()))
    .bind(owner)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(org_id)
    .bind(owner)
    .execute(&srv.pool)
    .await
    .unwrap();
    org_id
}

async fn member(srv: &Server, org_id: Uuid, role: &str, name: &str) -> (Uuid, String) {
    let (user_id, jwt) = user(srv, name).await;
    sqlx::query("INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, $3)")
        .bind(org_id)
        .bind(user_id)
        .bind(role)
        .execute(&srv.pool)
        .await
        .unwrap();
    (user_id, jwt)
}

/// One webinar, positioned in time and state.
struct Seed<'a> {
    title: &'a str,
    status: &'a str,
    host: Uuid,
    project_id: Option<Uuid>,
    scheduled_start: Option<chrono::DateTime<Utc>>,
    actual_start: Option<chrono::DateTime<Utc>>,
    archived: bool,
}

impl<'a> Seed<'a> {
    fn new(title: &'a str, host: Uuid) -> Self {
        Self {
            title,
            status: "ended",
            host,
            project_id: None,
            scheduled_start: None,
            actual_start: Some(Utc::now()),
            archived: false,
        }
    }
}

async fn webinar(srv: &Server, org_id: Uuid, seed: Seed<'_>) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO webinars (org_id, host_user_id, code, title, source_language, status,
                               project_id, scheduled_start, actual_start, archived_at)
         VALUES ($1, $2, $3, $4, 'en', $5, $6, $7, $8, $9) RETURNING id",
    )
    .bind(org_id)
    .bind(seed.host)
    .bind(format!("W{}", &Uuid::new_v4().simple().to_string()[..9]))
    .bind(seed.title)
    .bind(seed.status)
    .bind(seed.project_id)
    .bind(seed.scheduled_start)
    .bind(seed.actual_start)
    .bind(if seed.archived {
        Some(Utc::now())
    } else {
        None
    })
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    id
}

async fn project(srv: &Server, org_id: Uuid, creator: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO projects (org_id, name, created_by) VALUES ($1, 'Nord', $2) RETURNING id",
    )
    .bind(org_id)
    .bind(creator)
    .fetch_one(&srv.pool)
    .await
    .unwrap()
}

fn list_url(srv: &Server, org_id: Uuid, query: &str) -> String {
    format!(
        "{}/api/business/organizations/{org_id}/webinars{query}",
        base(srv)
    )
}

async fn list(http: &Client, srv: &Server, jwt: &str, org_id: Uuid, query: &str) -> Value {
    let r = http
        .get(list_url(srv, org_id, query))
        .bearer_auth(jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "list should succeed");
    r.json().await.unwrap()
}

fn titles(body: &Value) -> Vec<String> {
    body["webinars"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["title"].as_str().unwrap().to_string())
        .collect()
}

macro_rules! skip_without_db {
    () => {
        match setup().await {
            Some(srv) => srv,
            None => {
                eprintln!("skipping — no DATABASE_URL");
                return;
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Access
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_history_needs_a_signed_in_caller() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;

    let r = http.get(list_url(&srv, org_id, "")).send().await.unwrap();
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn an_outsider_is_not_told_the_organization_exists() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let (_, outsider) = user(&srv, "Outsider").await;

    let r = http
        .get(list_url(&srv, org_id, ""))
        .bearer_auth(&outsider)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

// ---------------------------------------------------------------------------
// Scope — a webinar audience is anonymous, so participation cannot scope it
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_member_sees_only_the_webinars_they_hosted() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let (member_id, member_jwt) = member(&srv, org_id, "member", "Host").await;

    webinar(&srv, org_id, Seed::new("Mine", member_id)).await;
    webinar(&srv, org_id, Seed::new("The owner's", owner)).await;

    assert_eq!(
        titles(&list(&http, &srv, &member_jwt, org_id, "").await),
        vec!["Mine"]
    );
}

#[tokio::test]
async fn an_admin_sees_every_webinar_in_the_org() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (owner, owner_jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let (member_id, _) = member(&srv, org_id, "member", "Host").await;
    let (_, admin_jwt) = member(&srv, org_id, "admin", "Admin").await;

    webinar(&srv, org_id, Seed::new("Theirs", member_id)).await;
    webinar(&srv, org_id, Seed::new("The owner's", owner)).await;

    assert_eq!(
        list(&http, &srv, &admin_jwt, org_id, "").await["webinars"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        list(&http, &srv, &owner_jwt, org_id, "").await["webinars"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn another_orgs_webinars_are_never_listed() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (owner, owner_jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let (other_owner, _) = user(&srv, "Other").await;
    let other_org = org(&srv, other_owner).await;
    webinar(&srv, other_org, Seed::new("Theirs", other_owner)).await;

    assert!(list(&http, &srv, &owner_jwt, org_id, "").await["webinars"]
        .as_array()
        .unwrap()
        .is_empty());
}

// ---------------------------------------------------------------------------
// Ordering, filtering, paging
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upcoming_and_past_runs_interleave_on_one_timeline_newest_first() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;

    // A finalized run sorts by when it started; a scheduled one by its slot.
    webinar(
        &srv,
        org_id,
        Seed {
            title: "Ran last week",
            actual_start: Some(Utc::now() - Duration::days(7)),
            ..Seed::new("Ran last week", owner)
        },
    )
    .await;
    webinar(
        &srv,
        org_id,
        Seed {
            title: "Scheduled tomorrow",
            status: "scheduled",
            actual_start: None,
            scheduled_start: Some(Utc::now() + Duration::days(1)),
            ..Seed::new("Scheduled tomorrow", owner)
        },
    )
    .await;
    webinar(
        &srv,
        org_id,
        Seed {
            title: "Ran yesterday",
            actual_start: Some(Utc::now() - Duration::days(1)),
            ..Seed::new("Ran yesterday", owner)
        },
    )
    .await;

    assert_eq!(
        titles(&list(&http, &srv, &jwt, org_id, "").await),
        vec!["Scheduled tomorrow", "Ran yesterday", "Ran last week"]
    );
}

#[tokio::test]
async fn archived_webinars_are_hidden_unless_asked_for() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    webinar(&srv, org_id, Seed::new("Live one", owner)).await;
    webinar(
        &srv,
        org_id,
        Seed {
            title: "Deleted one",
            archived: true,
            ..Seed::new("Deleted one", owner)
        },
    )
    .await;

    assert_eq!(
        titles(&list(&http, &srv, &jwt, org_id, "").await),
        vec!["Live one"]
    );
    assert_eq!(
        list(&http, &srv, &jwt, org_id, "?include_archived=true").await["webinars"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn the_status_filter_accepts_only_the_four_real_states() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    webinar(&srv, org_id, Seed::new("Finished", owner)).await;
    webinar(
        &srv,
        org_id,
        Seed {
            title: "Upcoming",
            status: "scheduled",
            actual_start: None,
            scheduled_start: Some(Utc::now() + Duration::days(1)),
            ..Seed::new("Upcoming", owner)
        },
    )
    .await;

    assert_eq!(
        titles(&list(&http, &srv, &jwt, org_id, "?status=ended").await),
        vec!["Finished"]
    );
    // Anything unrecognized is no filter at all, rather than a SQL error or an
    // empty list that looks like "you have none".
    assert_eq!(
        list(&http, &srv, &jwt, org_id, "?status=nonsense").await["webinars"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        list(&http, &srv, &jwt, org_id, "?status=").await["webinars"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn the_date_window_filters_on_the_runs_real_timeline() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    webinar(
        &srv,
        org_id,
        Seed {
            title: "Old",
            actual_start: Some(Utc::now() - Duration::days(30)),
            ..Seed::new("Old", owner)
        },
    )
    .await;
    webinar(&srv, org_id, Seed::new("Recent", owner)).await;

    // `to_rfc3339()` ends in `+00:00`, and a bare `+` in a query string is a
    // space — so the timestamp has to be written with the `Z` form.
    let from = (Utc::now() - Duration::days(2))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    assert_eq!(
        titles(&list(&http, &srv, &jwt, org_id, &format!("?from={from}")).await),
        vec!["Recent"]
    );

    let to = (Utc::now() - Duration::days(10))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    assert_eq!(
        titles(&list(&http, &srv, &jwt, org_id, &format!("?to={to}")).await),
        vec!["Old"]
    );
}

#[tokio::test]
async fn a_date_that_is_not_a_timestamp_is_refused_rather_than_ignored() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;

    for query in ["?from=yesterday", "?to=2026-13-45"] {
        let r = http
            .get(list_url(&srv, org_id, query))
            .bearer_auth(&jwt)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "{query} should be refused");
    }
}

#[tokio::test]
async fn paging_clamps_absurd_inputs() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    for i in 0..3 {
        webinar(
            &srv,
            org_id,
            Seed {
                title: "W",
                actual_start: Some(Utc::now() - Duration::minutes(i)),
                ..Seed::new("W", owner)
            },
        )
        .await;
    }

    let page1 = list(&http, &srv, &jwt, org_id, "?limit=2&page=1").await;
    assert_eq!(page1["webinars"].as_array().unwrap().len(), 2);
    let page2 = list(&http, &srv, &jwt, org_id, "?limit=2&page=2").await;
    assert_eq!(page2["webinars"].as_array().unwrap().len(), 1);

    let clamped = list(&http, &srv, &jwt, org_id, "?limit=100000&page=0").await;
    assert_eq!(clamped["limit"], 100);
    assert_eq!(clamped["page"], 1);
}

#[tokio::test]
async fn a_project_scoped_list_shows_only_that_projects_webinars() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let project_id = project(&srv, org_id, owner).await;
    webinar(
        &srv,
        org_id,
        Seed {
            title: "In the project",
            project_id: Some(project_id),
            ..Seed::new("In the project", owner)
        },
    )
    .await;
    webinar(&srv, org_id, Seed::new("Loose", owner)).await;

    let r = http
        .get(format!(
            "{}/api/business/organizations/{org_id}/projects/{project_id}/webinars",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        titles(&r.json().await.unwrap()),
        vec!["In the project"],
        "a project view must not show the org's loose webinars"
    );
}

#[tokio::test]
async fn a_listed_row_carries_the_project_name_and_the_report_flag() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let project_id = project(&srv, org_id, owner).await;
    webinar(
        &srv,
        org_id,
        Seed {
            title: "Launch",
            project_id: Some(project_id),
            ..Seed::new("Launch", owner)
        },
    )
    .await;

    let body = list(&http, &srv, &jwt, org_id, "").await;
    let row = &body["webinars"][0];
    assert_eq!(row["project_name"], "Nord");
    assert_eq!(row["has_report"], false, "no recap has been generated yet");
    // Rollup columns stay null until the webinar is finalized.
    assert!(row["duration_seconds"].is_null());
}

// ---------------------------------------------------------------------------
// Detail
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_detail_rollup_is_readable_for_a_webinar_you_hosted() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let (member_id, member_jwt) = member(&srv, org_id, "member", "Host").await;
    let webinar_id = webinar(&srv, org_id, Seed::new("Mine", member_id)).await;

    let r = http
        .get(format!(
            "{}/api/business/organizations/{org_id}/webinars/{webinar_id}",
            base(&srv)
        ))
        .bearer_auth(&member_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn a_member_cannot_open_the_detail_of_a_webinar_they_did_not_host() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (owner, _) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let (_, member_jwt) = member(&srv, org_id, "member", "Host").await;
    let webinar_id = webinar(&srv, org_id, Seed::new("The owner's", owner)).await;

    let r = http
        .get(format!(
            "{}/api/business/organizations/{org_id}/webinars/{webinar_id}",
            base(&srv)
        ))
        .bearer_auth(&member_jwt)
        .send()
        .await
        .unwrap();
    assert!(
        r.status() == 403 || r.status() == 404,
        "expected a refusal, got {}",
        r.status()
    );
}

#[tokio::test]
async fn the_detail_of_an_unknown_webinar_is_a_404() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;

    let r = http
        .get(format!(
            "{}/api/business/organizations/{org_id}/webinars/{}",
            base(&srv),
            Uuid::new_v4()
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn a_webinar_cannot_be_opened_through_another_orgs_path() {
    let srv = skip_without_db!();
    let http = Client::new();
    let (owner, jwt) = user(&srv, "Owner").await;
    let org_id = org(&srv, owner).await;
    let (other_owner, _) = user(&srv, "Other").await;
    let other_org = org(&srv, other_owner).await;
    let theirs = webinar(&srv, other_org, Seed::new("Theirs", other_owner)).await;

    let r = http
        .get(format!(
            "{}/api/business/organizations/{org_id}/webinars/{theirs}",
            base(&srv)
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}
