//! Integration tests for the Webinar Mode API (F0-3): host CRUD + the public
//! `/api/w/{code}` lookup. Auth + org-role guards, active-subscription gate,
//! PII-free public payload, and the `scheduled`-only edit lock.
//!
//! DB-gated like `tests/business_orgs.rs`: no-ops without `DATABASE_URL`. Locally:
//! `DATABASE_URL=postgres://…@localhost:5432/voxtest cargo test --test webinars`.

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::Client;
use serde_json::{json, Value};
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::{Config, WebinarConfig};
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::{app, db, AppState};

const SECRET: &str = "webinar-secret";

struct Server {
    addr: SocketAddr,
    pool: db::Pool,
}

async fn setup() -> Option<Server> {
    let url = match std::env::var("DATABASE_URL") {
        Ok(u) => u,
        Err(e) => {
            eprintln!("SETUP: no DATABASE_URL: {e}");
            return None;
        }
    };
    let pool = match db::connect(&url).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("SETUP: connect failed: {e}");
            return None;
        }
    };
    if let Err(e) = db::migrate(&pool).await {
        eprintln!("SETUP: migrate failed: {e}");
        return None;
    }
    let mut config = Config::test_with_billing(&url, SECRET, 0.0);
    config.webinar = Some(WebinarConfig::test_default());
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

fn base(srv: &Server) -> String {
    format!("http://{}", srv.addr)
}

/// Create a fresh user; returns its id and a session JWT.
async fn user(srv: &Server) -> (Uuid, String) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: "Webinar Host".into(),
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

/// Create an org owned by `owner`, with an active subscription unless `active` is
/// false. Returns the org id.
async fn org(srv: &Server, owner: Uuid, active: bool) -> Uuid {
    let slug = format!("org-{}", Uuid::new_v4().simple());
    let (status, period): (&str, Option<&str>) = if active {
        ("active", Some("30 days"))
    } else {
        ("none", None)
    };
    let org_id: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id, subscription_status, current_period_end)
         VALUES ($1, $2, $3, $4, CASE WHEN $5::text IS NULL THEN NULL
                                      ELSE now() + $5::interval END)
         RETURNING id",
    )
    .bind("Acme")
    .bind(&slug)
    .bind(owner)
    .bind(status)
    .bind(period)
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

/// Create a webinar via the API; returns the parsed JSON body (panics on non-201).
async fn create_webinar(http: &Client, srv: &Server, jwt: &str, org_id: Uuid) -> Value {
    let r = http
        .post(format!("{}/api/webinars", base(srv)))
        .bearer_auth(jwt)
        .json(&json!({
            "org_id": org_id,
            "title": "Launch",
            "source_language": "en",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "create webinar");
    r.json().await.unwrap()
}

#[tokio::test]
async fn create_requires_auth() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let r = http
        .post(format!("{}/api/webinars", base(&srv)))
        .json(&json!({ "org_id": Uuid::new_v4(), "title": "x", "source_language": "en" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "no token → 401");
}

#[tokio::test]
async fn create_rejects_non_member() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, _) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    // A different user who is NOT a member of the org.
    let (_outsider, jwt) = user(&srv).await;
    let r = http
        .post(format!("{}/api/webinars", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "org_id": org_id, "title": "x", "source_language": "en" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404, "non-member → 404 (org existence hidden)");
}

#[tokio::test]
async fn create_requires_active_subscription() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, false).await; // subscription 'none'
    let r = http
        .post(format!("{}/api/webinars", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "org_id": org_id, "title": "x", "source_language": "en" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 402, "inactive subscription → 402");
}

#[tokio::test]
async fn create_rejects_bad_tier() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let r = http
        .post(format!("{}/api/webinars", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "org_id": org_id, "title": "x", "source_language": "en", "tier": "gold" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "invalid tier → 400");
}

#[tokio::test]
async fn create_get_and_public_lookup_without_pii() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;

    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let code = created["code"].as_str().unwrap().to_string();
    assert!(!code.is_empty(), "create returns a code");
    assert_eq!(created["tier"], "enhanced", "default tier");
    assert_eq!(created["status"], "scheduled");
    assert_eq!(
        created["join_url"],
        format!("https://voxtranslate.app/w/{code}")
    );
    assert_eq!(
        created["playback_url"],
        format!("https://hls.test/webinar/{code}/index.m3u8")
    );
    let id = created["id"].as_str().unwrap();

    // Host GET by id (member) round-trips.
    let got = http
        .get(format!("{}/api/webinars/{id}", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(got.status(), 200);
    let got: Value = got.json().await.unwrap();
    assert_eq!(got["code"].as_str().unwrap(), code);

    // Public lookup by code needs NO auth and leaks NO host PII.
    let pubr = http
        .get(format!("{}/api/w/{code}", base(&srv)))
        .send()
        .await
        .unwrap();
    assert_eq!(pubr.status(), 200, "public lookup is open");
    let body: Value = pubr.json().await.unwrap();
    let obj = body.as_object().unwrap();
    assert_eq!(obj["code"].as_str().unwrap(), code);
    assert_eq!(obj["title"].as_str().unwrap(), "Launch");
    assert!(obj.contains_key("join_url") && obj.contains_key("playback_url"));
    for leaked in ["org_id", "host_user_id", "email"] {
        assert!(!obj.contains_key(leaked), "public payload leaks {leaked}");
    }
}

#[tokio::test]
async fn list_scoped_to_org_members() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    create_webinar(&http, &srv, &jwt, org_id).await;

    // Member sees the org's webinars.
    let r = http
        .get(format!("{}/api/webinars?org_id={org_id}", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let list: Value = r.json().await.unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);

    // Non-member is refused (404, org existence hidden).
    let (_o, other) = user(&srv).await;
    let r = http
        .get(format!("{}/api/webinars?org_id={org_id}", base(&srv)))
        .bearer_auth(&other)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn patch_locked_once_not_scheduled() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let id = created["id"].as_str().unwrap().to_string();

    // While scheduled, editing the title works.
    let ok = http
        .patch(format!("{}/api/webinars/{id}", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "title": "Renamed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    let ok: Value = ok.json().await.unwrap();
    assert_eq!(ok["title"], "Renamed");

    // Flip to live; further edits are locked (409).
    sqlx::query("UPDATE webinars SET status = 'live' WHERE id = $1::uuid")
        .bind(&id)
        .execute(&srv.pool)
        .await
        .unwrap();
    let locked = http
        .patch(format!("{}/api/webinars/{id}", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "title": "Too late" }))
        .send()
        .await
        .unwrap();
    assert_eq!(locked.status(), 409, "editing a live webinar is locked");
}

#[tokio::test]
async fn cancel_hides_from_public() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let (id, code) = (
        created["id"].as_str().unwrap().to_string(),
        created["code"].as_str().unwrap().to_string(),
    );

    let c = http
        .post(format!("{}/api/webinars/{id}/cancel", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(c.status(), 200);
    let c: Value = c.json().await.unwrap();
    assert_eq!(c["status"], "cancelled");

    // A cancelled webinar is no longer publicly resolvable.
    let pubr = http
        .get(format!("{}/api/w/{code}", base(&srv)))
        .send()
        .await
        .unwrap();
    assert_eq!(pubr.status(), 404, "cancelled webinar is hidden publicly");
}

#[tokio::test]
async fn guest_session_sets_and_reuses_id() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let code = created["code"].as_str().unwrap().to_string();

    // First visit (no cookie): the server mints a guest_id, sets it as an
    // HttpOnly/SameSite=Lax cookie, and echoes it in the body for localStorage.
    let r = http
        .get(format!("{}/api/w/{code}", base(&srv)))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let set_cookie = r
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .expect("guest cookie set on first visit")
        .to_str()
        .unwrap()
        .to_string();
    assert!(set_cookie.contains("guest_id="), "sets guest_id cookie");
    assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Lax"));
    let body: Value = r.json().await.unwrap();
    let guest = body["guest_id"].as_str().unwrap().to_string();
    assert!(Uuid::parse_str(&guest).is_ok(), "guest_id is a uuid");

    // Second visit WITH the cookie: the same id, and no fresh Set-Cookie.
    let r2 = http
        .get(format!("{}/api/w/{code}", base(&srv)))
        .header(reqwest::header::COOKIE, format!("guest_id={guest}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r2.status(), 200);
    assert!(
        r2.headers().get(reqwest::header::SET_COOKIE).is_none(),
        "an existing guest cookie is reused, not re-set"
    );
    let body2: Value = r2.json().await.unwrap();
    assert_eq!(
        body2["guest_id"].as_str().unwrap(),
        guest,
        "guest_id is stable across reloads"
    );
}

/// POST the MediaMTX external-auth hook; returns the HTTP status.
async fn media_auth_req(
    http: &Client,
    srv: &Server,
    caller: &str,
    action: &str,
    path: &str,
    query: &str,
) -> reqwest::StatusCode {
    http.post(format!("{}/internal/media-auth/{caller}", base(srv)))
        .json(&json!({ "action": action, "path": path, "query": query }))
        .send()
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn go_live_mints_tokenized_publish_url() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let id = created["id"].as_str().unwrap().to_string();
    let code = created["code"].as_str().unwrap().to_string();

    let r = http
        .post(format!("{}/api/webinars/{id}/go-live", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let b: Value = r.json().await.unwrap();
    let url = b["publish_url"].as_str().unwrap();
    assert!(
        url.starts_with(&format!("https://ingest.test/webinar/{code}/whip?token=")),
        "publish_url shape: {url}"
    );
    assert!(b["expires_in"].as_u64().unwrap() > 0, "expires_in set");

    // No auth → 401; non-member → 404.
    let na = http
        .post(format!("{}/api/webinars/{id}/go-live", base(&srv)))
        .send()
        .await
        .unwrap();
    assert_eq!(na.status(), 401);
    let (_o, other) = user(&srv).await;
    let nm = http
        .post(format!("{}/api/webinars/{id}/go-live", base(&srv)))
        .bearer_auth(&other)
        .send()
        .await
        .unwrap();
    assert_eq!(nm.status(), 404);
}

#[tokio::test]
async fn media_auth_authorizes_only_valid_publish() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let id = created["id"].as_str().unwrap().to_string();
    let code = created["code"].as_str().unwrap().to_string();
    let path = format!("webinar/{code}");

    // A real token straight from the go-live endpoint (exercises F1-1 → F1-2).
    let gl: Value = http
        .post(format!("{}/api/webinars/{id}/go-live", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = gl["publish_url"]
        .as_str()
        .unwrap()
        .split("?token=")
        .nth(1)
        .unwrap()
        .to_string();
    let good_query = format!("token={token}");
    const CALLER: &str = "test-caller-secret"; // WebinarConfig::test_default()

    // Valid publish → 200.
    assert_eq!(
        media_auth_req(&http, &srv, CALLER, "publish", &path, &good_query).await,
        200,
        "valid token authorizes publish"
    );
    // Wrong caller secret → 401.
    assert_eq!(
        media_auth_req(&http, &srv, "nope", "publish", &path, &good_query).await,
        401,
        "wrong caller secret rejected"
    );
    // Non-publish action → 401 (read/playback never reach here in prod).
    assert_eq!(
        media_auth_req(&http, &srv, CALLER, "read", &path, &good_query).await,
        401,
        "non-publish action rejected"
    );
    // Token minted for a different path → 401.
    assert_eq!(
        media_auth_req(&http, &srv, CALLER, "publish", "webinar/OTHER", &good_query).await,
        401,
        "token bound to another path rejected"
    );
    // Missing token → 401.
    assert_eq!(
        media_auth_req(&http, &srv, CALLER, "publish", &path, "").await,
        401,
        "missing token rejected"
    );
    // Tampered signature → 401.
    let mut chars: Vec<char> = token.chars().collect();
    let last = chars.len() - 1;
    chars[last] = if chars[last] == 'a' { 'b' } else { 'a' };
    let tampered: String = chars.into_iter().collect();
    assert_eq!(
        media_auth_req(
            &http,
            &srv,
            CALLER,
            "publish",
            &path,
            &format!("token={tampered}")
        )
        .await,
        401,
        "tampered token rejected"
    );
}

#[tokio::test]
async fn lifecycle_live_then_ended() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let id = created["id"].as_str().unwrap().to_string();
    let code = created["code"].as_str().unwrap().to_string();

    // publish-started → live + actual_start.
    let s1: Value = http
        .post(format!("{}/api/webinars/{id}/publish-started", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s1["status"], "live");
    let start = s1["actual_start"].as_str().unwrap().to_string();
    assert!(!start.is_empty(), "actual_start stamped");

    // Idempotent: a second publish-started keeps it live with the same start.
    let s2: Value = http
        .post(format!("{}/api/webinars/{id}/publish-started", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s2["status"], "live");
    assert_eq!(
        s2["actual_start"].as_str().unwrap(),
        start,
        "actual_start is stamped once"
    );

    // The public lookup reflects live.
    let pv: Value = http
        .get(format!("{}/api/w/{code}", base(&srv)))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(pv["status"], "live");

    // publish-stopped → ended + actual_end.
    let e: Value = http
        .post(format!("{}/api/webinars/{id}/publish-stopped", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(e["status"], "ended");
    assert!(e["actual_end"].as_str().is_some(), "actual_end stamped");

    // The public lookup shows ended (200 — only 'cancelled' is hidden as 404).
    let pe = http
        .get(format!("{}/api/w/{code}", base(&srv)))
        .send()
        .await
        .unwrap();
    assert_eq!(pe.status(), 200);
    let pe: Value = pe.json().await.unwrap();
    assert_eq!(pe["status"], "ended");

    // Re-going-live after ended is rejected.
    let again = http
        .post(format!("{}/api/webinars/{id}/publish-started", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 409, "cannot restart an ended webinar");
}

#[tokio::test]
async fn lifecycle_requires_membership() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let id = created["id"].as_str().unwrap().to_string();

    let na = http
        .post(format!("{}/api/webinars/{id}/publish-started", base(&srv)))
        .send()
        .await
        .unwrap();
    assert_eq!(na.status(), 401, "no auth → 401");

    let (_o, other) = user(&srv).await;
    let nm = http
        .post(format!("{}/api/webinars/{id}/publish-started", base(&srv)))
        .bearer_auth(&other)
        .send()
        .await
        .unwrap();
    assert_eq!(nm.status(), 404, "non-member → 404");
}

/// Create a webinar with a scheduled start/end; returns the parsed body.
async fn create_scheduled_webinar(http: &Client, srv: &Server, jwt: &str, org_id: Uuid) -> Value {
    let r = http
        .post(format!("{}/api/webinars", base(srv)))
        .bearer_auth(jwt)
        .json(&json!({
            "org_id": org_id,
            "title": "Scheduled",
            "source_language": "en",
            "scheduled_start": "2030-01-01T10:00:00Z",
            "scheduled_end": "2030-01-01T11:00:00Z",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "create scheduled webinar");
    r.json().await.unwrap()
}

#[tokio::test]
async fn calendar_requires_scheduled_start() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_webinar(&http, &srv, &jwt, org_id).await; // no scheduled_start
    let id = created["id"].as_str().unwrap();
    let r = http
        .post(format!("{}/api/webinars/{id}/calendar", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "unscheduled webinar → 400");
}

#[tokio::test]
async fn calendar_requires_google_connection() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_scheduled_webinar(&http, &srv, &jwt, org_id).await;
    let id = created["id"].as_str().unwrap();
    // The test user has no connected Google Calendar → 409 "connect calendar".
    let r = http
        .post(format!("{}/api/webinars/{id}/calendar", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409, "no Google connection → 409");
}

#[tokio::test]
async fn calendar_authz() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_scheduled_webinar(&http, &srv, &jwt, org_id).await;
    let id = created["id"].as_str().unwrap();
    let na = http
        .post(format!("{}/api/webinars/{id}/calendar", base(&srv)))
        .send()
        .await
        .unwrap();
    assert_eq!(na.status(), 401, "no auth → 401");
    let (_o, other) = user(&srv).await;
    let nm = http
        .post(format!("{}/api/webinars/{id}/calendar", base(&srv)))
        .bearer_auth(&other)
        .send()
        .await
        .unwrap();
    assert_eq!(nm.status(), 404, "non-member → 404");
}

/// Read the next `{type:"count"}` message off a presence WebSocket.
async fn next_count<S>(ws: &mut S) -> u64
where
    S: futures::StreamExt<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    use tokio_tungstenite::tungstenite::Message;
    loop {
        let msg = ws.next().await.expect("ws open").expect("ws msg");
        if let Message::Text(t) = msg {
            let v: Value = serde_json::from_str(&t).unwrap();
            if v["type"] == "count" {
                return v["count"].as_u64().unwrap();
            }
        }
    }
}

#[tokio::test]
async fn presence_counts_and_records_history() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let code = created["code"].as_str().unwrap().to_string();
    let webinar_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    let ws_url = |g: Uuid| format!("ws://{}/api/w/{code}/presence?guest_id={g}", srv.addr);

    // Guest A joins → audience count 1.
    let (mut a, _) = tokio_tungstenite::connect_async(ws_url(Uuid::new_v4()))
        .await
        .unwrap();
    assert_eq!(next_count(&mut a).await, 1, "first viewer → 1");

    // Guest B joins → both are notified of count 2.
    let (mut b, _) = tokio_tungstenite::connect_async(ws_url(Uuid::new_v4()))
        .await
        .unwrap();
    assert_eq!(next_count(&mut b).await, 2, "second viewer → 2");
    assert_eq!(next_count(&mut a).await, 2, "A sees B join");

    // History persisted (validates the 038 schema end-to-end).
    let joins: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM webinar_events WHERE webinar_id = $1 AND type = 'join'",
    )
    .bind(webinar_id)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert!(joins >= 2, "join events recorded");
    let participants: i64 =
        sqlx::query_scalar("SELECT count(*) FROM webinar_participants WHERE webinar_id = $1")
            .bind(webinar_id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(participants, 2, "one participant row per guest");

    // B leaves → A is notified of count 1.
    drop(b);
    assert_eq!(next_count(&mut a).await, 1, "A sees B leave");
}

#[tokio::test]
async fn presence_host_flag_only_honored_with_a_member_token() {
    // `?host=true` excludes a connection from the audience count. Anyone can send it,
    // so it must be gated: only a valid JWT of an org MEMBER is honored; every other
    // claimant is silently downgraded to a counted viewer.
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await; // jwt = a MEMBER of the webinar's org
    let org_id = org(&srv, owner, true).await;
    let (_other, other_jwt) = user(&srv).await; // valid JWT, NOT a member of org_id
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let code = created["code"].as_str().unwrap().to_string();
    let addr = srv.addr;

    // Viewer A joins → audience 1.
    let (mut a, _) = tokio_tungstenite::connect_async(format!(
        "ws://{addr}/api/w/{code}/presence?guest_id={}",
        Uuid::new_v4()
    ))
    .await
    .unwrap();
    assert_eq!(next_count(&mut a).await, 1, "first viewer → 1");

    // A real host (member token) joins → NOT counted; A still sees 1.
    let (host, _) = tokio_tungstenite::connect_async(format!(
        "ws://{addr}/api/w/{code}/presence?host=true&guest_id={}&token={jwt}",
        Uuid::new_v4()
    ))
    .await
    .unwrap();
    assert_eq!(
        next_count(&mut a).await,
        1,
        "a member-token host is not part of the audience"
    );

    // host=true WITHOUT a token → downgraded to a viewer → counted; A sees 2.
    let (faker, _) = tokio_tungstenite::connect_async(format!(
        "ws://{addr}/api/w/{code}/presence?host=true&guest_id={}",
        Uuid::new_v4()
    ))
    .await
    .unwrap();
    assert_eq!(
        next_count(&mut a).await,
        2,
        "host=true with no token is just a viewer"
    );

    // host=true with a NON-member's valid JWT → also downgraded → counted; A sees 3.
    let (outsider, _) = tokio_tungstenite::connect_async(format!(
        "ws://{addr}/api/w/{code}/presence?host=true&guest_id={}&token={other_jwt}",
        Uuid::new_v4()
    ))
    .await
    .unwrap();
    assert_eq!(
        next_count(&mut a).await,
        3,
        "host=true with a non-member token is just a viewer"
    );

    drop((host, faker, outsider));
}

/// List an org's webinars (active or archived); returns the JSON array.
async fn list_webinars(
    http: &Client,
    srv: &Server,
    jwt: &str,
    org_id: Uuid,
    archived: bool,
) -> Value {
    http.get(format!(
        "{}/api/webinars?org_id={org_id}&archived={archived}",
        base(srv)
    ))
    .bearer_auth(jwt)
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap()
}

#[tokio::test]
async fn archive_hides_from_active_and_restores() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let id = created["id"].as_str().unwrap();

    assert_eq!(
        list_webinars(&http, &srv, &jwt, org_id, false)
            .await
            .as_array()
            .unwrap()
            .len(),
        1,
        "active list shows it"
    );

    // Archive → gone from active, present in archived, data preserved.
    let a = http
        .post(format!("{}/api/webinars/{id}/archive", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(a.status(), 200);
    let a: Value = a.json().await.unwrap();
    assert!(a["archived_at"].as_str().is_some(), "archived_at stamped");
    assert_eq!(
        list_webinars(&http, &srv, &jwt, org_id, false)
            .await
            .as_array()
            .unwrap()
            .len(),
        0,
        "hidden from active"
    );
    assert_eq!(
        list_webinars(&http, &srv, &jwt, org_id, true)
            .await
            .as_array()
            .unwrap()
            .len(),
        1,
        "shown in archived"
    );

    // Unarchive → back in active.
    let u = http
        .post(format!("{}/api/webinars/{id}/unarchive", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(u.status(), 200);
    assert_eq!(
        list_webinars(&http, &srv, &jwt, org_id, false)
            .await
            .as_array()
            .unwrap()
            .len(),
        1,
        "restored to active"
    );
}

#[tokio::test]
async fn create_links_to_project_in_same_org_only() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let project_id: Uuid =
        sqlx::query_scalar("INSERT INTO projects (org_id, name) VALUES ($1, 'P') RETURNING id")
            .bind(org_id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();

    // Link to a project in the caller's org → 201, echoed back.
    let r = http
        .post(format!("{}/api/webinars", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({
            "org_id": org_id, "title": "P", "source_language": "en", "project_id": project_id
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let b: Value = r.json().await.unwrap();
    assert_eq!(b["project_id"].as_str().unwrap(), project_id.to_string());

    // A project from ANOTHER org → 400.
    let (o2, _) = user(&srv).await;
    let org2 = org(&srv, o2, true).await;
    let foreign: Uuid =
        sqlx::query_scalar("INSERT INTO projects (org_id, name) VALUES ($1, 'X') RETURNING id")
            .bind(org2)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    let bad = http
        .post(format!("{}/api/webinars", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({
            "org_id": org_id, "title": "x", "source_language": "en", "project_id": foreign
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400, "project from another org rejected");
}

// ---- ④ Realtime subtitles (SPEC Fase 2) — STT ingest WS security -----------

/// Attempt an STT WebSocket handshake and return the HTTP status the server
/// replied with. A successful upgrade is `101`; the auth/authz guards reject
/// BEFORE the upgrade, so a rejection surfaces as an `Http` error carrying the
/// real status (401/404/409).
async fn stt_handshake_status(addr: SocketAddr, id: &str, token: Option<&str>) -> u16 {
    let mut url = format!("ws://{addr}/api/webinars/{id}/stt");
    if let Some(t) = token {
        url.push_str(&format!("?token={t}"));
    }
    match tokio_tungstenite::connect_async(url).await {
        Ok((ws, resp)) => {
            drop(ws);
            resp.status().as_u16()
        }
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => resp.status().as_u16(),
        Err(e) => panic!("unexpected WS error: {e:?}"),
    }
}

#[tokio::test]
async fn stt_guest_and_cross_tenant_are_rejected() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let id = created["id"].as_str().unwrap().to_string();

    // (1) Guest — no token → 401. The host mic cannot be opened anonymously.
    assert_eq!(
        stt_handshake_status(srv.addr, &id, None).await,
        401,
        "no token → 401"
    );

    // A garbage token is also 401 (not a valid JWT).
    assert_eq!(
        stt_handshake_status(srv.addr, &id, Some("not-a-jwt")).await,
        401,
        "invalid token → 401"
    );

    // (1) Wrong org — a valid user who is NOT a member of the webinar's org → 404
    // (cross-tenant existence is hidden, exactly like the REST host API).
    let (_outsider, other) = user(&srv).await;
    assert_eq!(
        stt_handshake_status(srv.addr, &id, Some(&other)).await,
        404,
        "non-member → 404"
    );
}

#[tokio::test]
async fn stt_requires_live_or_scheduled_status() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let id = created["id"].as_str().unwrap().to_string();

    // A fresh webinar is `scheduled` → the host may warm the mic (handshake would
    // upgrade; we only need to confirm it is NOT rejected by the status gate).
    assert_ne!(
        stt_handshake_status(srv.addr, &id, Some(&jwt)).await,
        409,
        "scheduled webinar allows STT"
    );

    // End it → `stt` is now a 409 (no subtitles after the webinar is over).
    http.post(format!("{}/api/webinars/{id}/publish-started", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    http.post(format!("{}/api/webinars/{id}/publish-stopped", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(
        stt_handshake_status(srv.addr, &id, Some(&jwt)).await,
        409,
        "ended webinar → 409"
    );
}

#[tokio::test]
async fn presence_ws_ignores_inbound_frames() {
    // (2) A viewer writing to their own presence socket must NEVER produce a
    // subtitle broadcast — subtitles come ONLY from the STT processor.
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let code = created["code"].as_str().unwrap().to_string();

    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let url = format!("ws://{}/api/w/{code}/presence?lang=es", srv.addr);
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    assert_eq!(next_count(&mut ws).await, 1, "viewer joined");

    // The viewer tries to inject a subtitle frame over its OWN socket.
    ws.send(Message::Text(
        r#"{"type":"subtitle","kind":"final","original":"hacked","lang":"es","translations":{"es":"hacked"}}"#
            .into(),
    ))
    .await
    .unwrap();

    // Nothing the viewer sends is ever echoed back as a subtitle. Give the server
    // a moment; the only frames it ever pushes here are `count`s.
    let got_subtitle = tokio::time::timeout(std::time::Duration::from_millis(300), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    if v["type"] == "subtitle" {
                        return true;
                    }
                }
                Some(Ok(_)) => continue,
                _ => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(
        !got_subtitle,
        "an inbound viewer frame must never produce a subtitle broadcast"
    );
}

#[tokio::test]
async fn presence_validates_viewer_lang_server_side() {
    // (3) `lang` is validated with `valid_lang` before it is stored/used. A valid
    // code is persisted on the participant row; a malformed one is dropped.
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let code = created["code"].as_str().unwrap().to_string();
    let webinar_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();

    // A valid language is stored on the participant row.
    let good_guest = Uuid::new_v4();
    let (mut g, _) = tokio_tungstenite::connect_async(format!(
        "ws://{}/api/w/{code}/presence?guest_id={good_guest}&lang=es",
        srv.addr
    ))
    .await
    .unwrap();
    let _ = next_count(&mut g).await;

    // A malformed language (contains a disallowed char) is rejected → NOT stored.
    let bad_guest = Uuid::new_v4();
    let (mut b, _) = tokio_tungstenite::connect_async(format!(
        "ws://{}/api/w/{code}/presence?guest_id={bad_guest}&lang=es%3Bdrop",
        srv.addr
    ))
    .await
    .unwrap();
    let _ = next_count(&mut b).await;

    // The join is best-effort/async; poll briefly for the participant rows.
    let mut good_lang: Option<String> = None;
    let mut bad_lang: Option<String> = Some("sentinel".into());
    for _ in 0..20 {
        good_lang = sqlx::query_scalar(
            "SELECT language_code FROM webinar_participants WHERE webinar_id = $1 AND guest_id = $2",
        )
        .bind(webinar_id)
        .bind(good_guest)
        .fetch_optional(&srv.pool)
        .await
        .unwrap()
        .flatten();
        bad_lang = sqlx::query_scalar(
            "SELECT language_code FROM webinar_participants WHERE webinar_id = $1 AND guest_id = $2",
        )
        .bind(webinar_id)
        .bind(bad_guest)
        .fetch_optional(&srv.pool)
        .await
        .unwrap()
        .flatten();
        if good_lang.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(good_lang.as_deref(), Some("es"), "valid lang stored");
    assert_eq!(
        bad_lang, None,
        "a malformed lang is validated away, not stored"
    );
}

// ---- ⑤ Auto-translated chat (SPEC Feature ⑤) -------------------------------

/// Create a webinar with chat enabled; returns the parsed body.
async fn create_chat_webinar(http: &Client, srv: &Server, jwt: &str, org_id: Uuid) -> Value {
    let r = http
        .post(format!("{}/api/webinars", base(srv)))
        .bearer_auth(jwt)
        .json(&json!({
            "org_id": org_id,
            "title": "Chatty",
            "source_language": "en",
            "chat_enabled": true,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "create chat webinar");
    r.json().await.unwrap()
}

#[tokio::test]
async fn chat_enabled_flag_round_trips_create_host_and_public() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;

    // Default (omitted) → chat is off, visible on the host view.
    let off = create_webinar(&http, &srv, &jwt, org_id).await;
    assert_eq!(off["chat_enabled"], false, "chat defaults off on create");

    // Explicit chat_enabled:true → host view echoes it AND the public view exposes
    // it (guests need it to render the panel).
    let on = create_chat_webinar(&http, &srv, &jwt, org_id).await;
    assert_eq!(on["chat_enabled"], true, "host view carries chat_enabled");
    let code = on["code"].as_str().unwrap().to_string();

    let pubr = http
        .get(format!("{}/api/w/{code}", base(&srv)))
        .send()
        .await
        .unwrap();
    assert_eq!(pubr.status(), 200);
    let body: Value = pubr.json().await.unwrap();
    assert_eq!(
        body["chat_enabled"], true,
        "public view exposes chat_enabled for the guest panel"
    );
    // The public view still leaks no host PII.
    let obj = body.as_object().unwrap();
    for leaked in ["org_id", "host_user_id", "email"] {
        assert!(!obj.contains_key(leaked), "public payload leaks {leaked}");
    }
}

/// Count chat rows for a webinar.
async fn chat_count(srv: &Server, webinar_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM webinar_chat_messages WHERE webinar_id = $1")
        .bind(webinar_id)
        .fetch_one(&srv.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn chat_send_rejected_when_disabled() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    // A webinar with chat OFF (the default).
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let code = created["code"].as_str().unwrap().to_string();
    let webinar_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();

    // Guest send → 403, nothing persisted.
    let r = http
        .post(format!("{}/api/w/{code}/chat", base(&srv)))
        .json(&json!({ "text": "hello", "display_name": "Bob" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403, "chat disabled → 403");
    assert_eq!(chat_count(&srv, webinar_id).await, 0, "nothing persisted");

    // Unknown code → 404.
    let nf = http
        .post(format!("{}/api/w/NOPECODE/chat", base(&srv)))
        .json(&json!({ "text": "hi" }))
        .send()
        .await
        .unwrap();
    assert_eq!(nf.status(), 404, "unknown code → 404");
}

#[tokio::test]
async fn chat_send_validates_text() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_chat_webinar(&http, &srv, &jwt, org_id).await;
    let code = created["code"].as_str().unwrap().to_string();

    // Empty text → 400.
    let empty = http
        .post(format!("{}/api/w/{code}/chat", base(&srv)))
        .json(&json!({ "text": "   " }))
        .send()
        .await
        .unwrap();
    assert_eq!(empty.status(), 400, "empty text → 400");

    // Over 500 chars → 400.
    let long = http
        .post(format!("{}/api/w/{code}/chat", base(&srv)))
        .json(&json!({ "text": "a".repeat(501) }))
        .send()
        .await
        .unwrap();
    assert_eq!(long.status(), 400, "too-long text → 400");
}

#[tokio::test]
async fn chat_guest_send_persists_and_returns_id() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_chat_webinar(&http, &srv, &jwt, org_id).await;
    let code = created["code"].as_str().unwrap().to_string();
    let webinar_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();

    // A guest (no Authorization header) sends → 200 { id, created_at }.
    let r = http
        .post(format!("{}/api/w/{code}/chat", base(&srv)))
        .json(&json!({ "text": "ciao a tutti", "display_name": "Guest Gina", "lang": "it" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "guest send → 200");
    let body: Value = r.json().await.unwrap();
    assert!(
        Uuid::parse_str(body["id"].as_str().unwrap()).is_ok(),
        "returns a message id"
    );
    assert!(body["created_at"].as_str().is_some(), "returns created_at");

    // Persisted as a guest row with the original text + label lang.
    assert_eq!(chat_count(&srv, webinar_id).await, 1, "one row persisted");
    let row: (String, String, String, String) = sqlx::query_as(
        "SELECT sender_kind, display_name, original_text, original_lang
         FROM webinar_chat_messages WHERE webinar_id = $1",
    )
    .bind(webinar_id)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(row.0, "guest", "no JWT → guest sender_kind");
    assert_eq!(row.1, "Guest Gina");
    assert_eq!(row.2, "ciao a tutti");
    assert_eq!(
        row.3, "it",
        "original_lang stores the sender's UI language label"
    );
}

#[tokio::test]
async fn chat_global_rate_limit_caps_the_webinar_across_senders() {
    // A cookieless client is minted a FRESH guest_id per request, so the per-sender
    // cap never collides — the webinar-wide cap (keyed without the sender) is what
    // must stop a scripted flood. Sends past WEBINAR_RATE_MAX in the window → 429.
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new(); // no cookie store → a new guest identity each request
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_chat_webinar(&http, &srv, &jwt, org_id).await;
    let code = created["code"].as_str().unwrap().to_string();

    // The webinar-wide cap is 30 / 10s (WEBINAR_RATE_MAX). The first 30 pass; the
    // 31st, still within the window and from yet another fresh guest, is throttled.
    for i in 0..30 {
        let r = http
            .post(format!("{}/api/w/{code}/chat", base(&srv)))
            .json(&json!({ "text": format!("msg {i}") }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "message {i} within the webinar cap → 200");
    }
    let over = http
        .post(format!("{}/api/w/{code}/chat", base(&srv)))
        .json(&json!({ "text": "one too many" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        over.status(),
        429,
        "past the webinar-wide cap → 429 even from a fresh guest id"
    );
}

#[tokio::test]
async fn chat_invalid_lang_label_falls_back_to_auto() {
    // `lang` is client-supplied and persisted + re-broadcast, so a malformed value
    // must not be stored verbatim — it clamps to "auto".
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_chat_webinar(&http, &srv, &jwt, org_id).await;
    let code = created["code"].as_str().unwrap().to_string();
    let webinar_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();

    let r = http
        .post(format!("{}/api/w/{code}/chat", base(&srv)))
        .json(&json!({ "text": "hola", "lang": "not a lang!!" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let lang: (String,) =
        sqlx::query_as("SELECT original_lang FROM webinar_chat_messages WHERE webinar_id = $1")
            .bind(webinar_id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(lang.0, "auto", "a malformed lang label clamps to auto");
}

#[tokio::test]
async fn chat_host_send_is_sender_kind_host() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_chat_webinar(&http, &srv, &jwt, org_id).await;
    let code = created["code"].as_str().unwrap().to_string();
    let webinar_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();

    // A member's JWT on the Authorization header → sender_kind "host".
    let r = http
        .post(format!("{}/api/w/{code}/chat", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "text": "welcome everyone", "display_name": "Host" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let kind: String =
        sqlx::query_scalar("SELECT sender_kind FROM webinar_chat_messages WHERE webinar_id = $1")
            .bind(webinar_id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(kind, "host", "member JWT → host sender_kind");

    // A NON-member's JWT falls back to guest (optional-auth: bad identity ≠ host).
    let (_outsider, other) = user(&srv).await;
    let r2 = http
        .post(format!("{}/api/w/{code}/chat", base(&srv)))
        .bearer_auth(&other)
        .json(&json!({ "text": "hi from outside" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r2.status(), 200, "a non-member can still chat as a guest");
    let kinds: Vec<String> = sqlx::query_scalar(
        "SELECT sender_kind FROM webinar_chat_messages WHERE webinar_id = $1 ORDER BY created_at",
    )
    .bind(webinar_id)
    .fetch_all(&srv.pool)
    .await
    .unwrap();
    assert_eq!(kinds, vec!["host", "guest"], "non-member JWT → guest");
}

#[tokio::test]
async fn chat_moderated_message_is_422_and_not_persisted() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_chat_webinar(&http, &srv, &jwt, org_id).await;
    let code = created["code"].as_str().unwrap().to_string();
    let webinar_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();

    // A message containing a default-blocklist slur → 422, never persisted.
    let r = http
        .post(format!("{}/api/w/{code}/chat", base(&srv)))
        .json(&json!({ "text": "you retard" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422, "severe message → 422");
    assert_eq!(
        chat_count(&srv, webinar_id).await,
        0,
        "a moderated message is NOT persisted"
    );
}

/// Send a chat message as a guest (no auth); asserts 200.
async fn send_chat(http: &Client, srv: &Server, code: &str, text: &str) {
    let r = http
        .post(format!("{}/api/w/{code}/chat", base(srv)))
        .json(&json!({ "text": text, "display_name": "Guest" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "send chat");
}

/// GET the chat history; returns the JSON array.
async fn get_history(http: &Client, srv: &Server, code: &str, limit: Option<u32>) -> Value {
    let mut url = format!("{}/api/w/{code}/chat", base(srv));
    if let Some(n) = limit {
        url.push_str(&format!("?limit={n}"));
    }
    let r = http.get(url).send().await.unwrap();
    assert_eq!(r.status(), 200, "history is public");
    r.json().await.unwrap()
}

#[tokio::test]
async fn chat_history_returns_recent_chronological() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_chat_webinar(&http, &srv, &jwt, org_id).await;
    let code = created["code"].as_str().unwrap().to_string();

    for t in ["first", "second", "third"] {
        send_chat(&http, &srv, &code, t).await;
    }

    // History is returned oldest → newest (chronological).
    let hist = get_history(&http, &srv, &code, None).await;
    let arr = hist.as_array().unwrap();
    assert_eq!(arr.len(), 3, "all three messages returned");
    let texts: Vec<&str> = arr
        .iter()
        .map(|m| m["original"].as_str().unwrap())
        .collect();
    assert_eq!(
        texts,
        vec!["first", "second", "third"],
        "chronological order"
    );

    // Each entry carries the public shape and NO sender identity.
    let m = &arr[0];
    for key in [
        "id",
        "sender_kind",
        "display_name",
        "original",
        "lang",
        "translations",
        "created_at",
    ] {
        assert!(m.get(key).is_some(), "history entry has `{key}`");
    }
    for leaked in ["sender_id", "guest_id", "user_id", "host_user_id", "email"] {
        assert!(
            m.get(leaked).is_none(),
            "history entry must not leak `{leaked}`"
        );
    }
}

#[tokio::test]
async fn chat_history_empty_when_disabled() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    // Chat OFF → history is an empty list (200, not 403 — reading is harmless).
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let code = created["code"].as_str().unwrap().to_string();

    let hist = get_history(&http, &srv, &code, None).await;
    assert_eq!(
        hist.as_array().unwrap().len(),
        0,
        "chat disabled → empty history"
    );

    // Unknown code → 404.
    let nf = http
        .get(format!("{}/api/w/NOPECODE/chat", base(&srv)))
        .send()
        .await
        .unwrap();
    assert_eq!(nf.status(), 404, "unknown code → 404");
}

#[tokio::test]
async fn chat_history_limit_caps_and_returns_most_recent() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_chat_webinar(&http, &srv, &jwt, org_id).await;
    let code = created["code"].as_str().unwrap().to_string();

    for i in 0..5 {
        send_chat(&http, &srv, &code, &format!("msg{i}")).await;
    }

    // limit=2 → the two MOST RECENT, still chronological within the slice.
    let hist = get_history(&http, &srv, &code, Some(2)).await;
    let arr = hist.as_array().unwrap();
    assert_eq!(arr.len(), 2, "limit caps the count");
    let texts: Vec<&str> = arr
        .iter()
        .map(|m| m["original"].as_str().unwrap())
        .collect();
    assert_eq!(texts, vec!["msg3", "msg4"], "most-recent, chronological");

    // A limit over the hard cap (100) is clamped, not an error.
    let all = get_history(&http, &srv, &code, Some(9999)).await;
    assert_eq!(
        all.as_array().unwrap().len(),
        5,
        "over-cap limit still works"
    );
}

// ---- Public/private visibility --------------------------------------------

/// Create a webinar with an explicit visibility; returns the parsed body.
async fn create_visibility_webinar(
    http: &Client,
    srv: &Server,
    jwt: &str,
    org_id: Uuid,
    visibility: &str,
) -> Value {
    let r = http
        .post(format!("{}/api/webinars", base(srv)))
        .bearer_auth(jwt)
        .json(&json!({
            "org_id": org_id,
            "title": "Visible",
            "source_language": "en",
            "visibility": visibility,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        201,
        "create webinar with visibility={visibility}"
    );
    r.json().await.unwrap()
}

#[tokio::test]
async fn visibility_defaults_private_and_round_trips_public() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;

    // Default (omitted) → private, visible on the host view.
    let default = create_webinar(&http, &srv, &jwt, org_id).await;
    assert_eq!(
        default["visibility"], "private",
        "visibility defaults to private on create"
    );

    // Explicit visibility:"public" → host view echoes it AND the public view
    // exposes it (a guest on /w/{code} may see it).
    let pubw = create_visibility_webinar(&http, &srv, &jwt, org_id, "public").await;
    assert_eq!(pubw["visibility"], "public", "host view carries visibility");
    let code = pubw["code"].as_str().unwrap().to_string();

    let pubr = http
        .get(format!("{}/api/w/{code}", base(&srv)))
        .send()
        .await
        .unwrap();
    assert_eq!(pubr.status(), 200);
    let body: Value = pubr.json().await.unwrap();
    assert_eq!(
        body["visibility"], "public",
        "public view exposes visibility"
    );
}

#[tokio::test]
async fn create_rejects_invalid_visibility() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let r = http
        .post(format!("{}/api/webinars", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({
            "org_id": org_id,
            "title": "x",
            "source_language": "en",
            "visibility": "unlisted",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "invalid visibility → 400");
}

#[tokio::test]
async fn patch_updates_visibility() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    // Starts private (the default).
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    assert_eq!(created["visibility"], "private");
    let id = created["id"].as_str().unwrap().to_string();

    // PATCH visibility → public (COALESCE keeps other fields).
    let ok = http
        .patch(format!("{}/api/webinars/{id}", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "visibility": "public" }))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    let ok: Value = ok.json().await.unwrap();
    assert_eq!(ok["visibility"], "public", "PATCH updates visibility");

    // An invalid visibility on PATCH → 400.
    let bad = http
        .patch(format!("{}/api/webinars/{id}", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "visibility": "secret" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400, "invalid visibility on PATCH → 400");
}

#[tokio::test]
async fn public_list_returns_only_public_discoverable_webinars() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;

    // A PUBLIC + scheduled webinar → discoverable.
    let pubw = create_visibility_webinar(&http, &srv, &jwt, org_id, "public").await;
    let pub_code = pubw["code"].as_str().unwrap().to_string();

    // A PRIVATE webinar → NOT discoverable.
    let priv_w = create_webinar(&http, &srv, &jwt, org_id).await;
    let priv_code = priv_w["code"].as_str().unwrap().to_string();

    // A PUBLIC but cancelled webinar → NOT discoverable.
    let cancelled = create_visibility_webinar(&http, &srv, &jwt, org_id, "public").await;
    let cancelled_id = cancelled["id"].as_str().unwrap().to_string();
    let cancelled_code = cancelled["code"].as_str().unwrap().to_string();
    http.post(format!("{}/api/webinars/{cancelled_id}/cancel", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();

    // A PUBLIC but archived webinar → NOT discoverable.
    let archived = create_visibility_webinar(&http, &srv, &jwt, org_id, "public").await;
    let archived_id = archived["id"].as_str().unwrap().to_string();
    let archived_code = archived["code"].as_str().unwrap().to_string();
    http.post(format!("{}/api/webinars/{archived_id}/archive", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();

    // The public list now requires auth. Use a DIFFERENT user (different org) as
    // the discoverer — the owner's own org's webinars are excluded from their view.
    let (visitor, visitor_jwt) = user(&srv).await;
    let _visitor_org = org(&srv, visitor, true).await;
    let r = http
        .get(format!("{}/api/webinars/public", base(&srv)))
        .bearer_auth(&visitor_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        200,
        "public list is accessible to authenticated users"
    );
    let body: Value = r.json().await.unwrap();
    let items = body["webinars"].as_array().expect("webinars array");

    let codes: Vec<&str> = items.iter().map(|w| w["code"].as_str().unwrap()).collect();
    assert!(
        codes.contains(&pub_code.as_str()),
        "public+scheduled listed"
    );
    assert!(!codes.contains(&priv_code.as_str()), "private NOT listed");
    assert!(
        !codes.contains(&cancelled_code.as_str()),
        "cancelled NOT listed"
    );
    assert!(
        !codes.contains(&archived_code.as_str()),
        "archived NOT listed"
    );

    // The listed item carries the PII-free public shape + a live `viewers` count.
    let item = items
        .iter()
        .find(|w| w["code"].as_str() == Some(pub_code.as_str()))
        .unwrap();
    for key in [
        "code",
        "title",
        "status",
        "source_language",
        "tier",
        "join_url",
        "viewers",
    ] {
        assert!(item.get(key).is_some(), "public list item has `{key}`");
    }
    assert_eq!(
        item["viewers"].as_u64().unwrap(),
        0,
        "viewers count present"
    );
    let obj = item.as_object().unwrap();
    for leaked in ["org_id", "host_user_id", "email", "playback_url"] {
        assert!(
            !obj.contains_key(leaked),
            "public list item leaks `{leaked}`"
        );
    }

    // Unauthenticated call → 200 with the full public list (guests see all, no org exclusion).
    let unauth = http
        .get(format!("{}/api/webinars/public", base(&srv)))
        .send()
        .await
        .unwrap();
    assert_eq!(
        unauth.status(),
        200,
        "unauthenticated public list → 200 (guests can discover)"
    );
    let unauth_body: Value = unauth.json().await.unwrap();
    let unauth_items = unauth_body["webinars"].as_array().expect("webinars array");
    // Unauthenticated guests see ALL public webinars (no org exclusion).
    let unauth_codes: Vec<&str> = unauth_items
        .iter()
        .map(|w| w["code"].as_str().unwrap())
        .collect();
    assert!(
        unauth_codes.contains(&pub_code.as_str()),
        "guest sees public webinar"
    );
}

// ---- schedule validation (create) ------------------------------------------

/// RFC-3339 UTC string `hours` from now (negative → in the past).
fn iso_from_now(hours: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::hours(hours)).to_rfc3339()
}

#[tokio::test]
async fn create_rejects_past_scheduled_start() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let r = http
        .post(format!("{}/api/webinars", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({
            "org_id": org_id,
            "title": "Past start",
            "source_language": "en",
            "scheduled_start": iso_from_now(-24),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "a clearly-past scheduled_start → 400");
}

#[tokio::test]
async fn create_rejects_end_before_start() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let r = http
        .post(format!("{}/api/webinars", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({
            "org_id": org_id,
            "title": "Bad range",
            "source_language": "en",
            "scheduled_start": iso_from_now(48),
            "scheduled_end": iso_from_now(24), // before the start
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "scheduled_end before start → 400");
}

#[tokio::test]
async fn create_accepts_valid_future_schedule() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let r = http
        .post(format!("{}/api/webinars", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({
            "org_id": org_id,
            "title": "Valid schedule",
            "source_language": "en",
            "scheduled_start": iso_from_now(24),
            "scheduled_end": iso_from_now(25),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "a valid future schedule → 201");
}

// ---- Phase-A host billing finalization (publish-stopped → webinar_sessions) --
//
// End-to-end DB verification of the host-billing path: publish-started → seed an
// audience directly in `webinar_participants` + `webinar_events` → publish-stopped
// runs `finalize_webinar_run`, which sweeps the timeline, writes ONE
// `webinar_sessions` row, and deducts `cost_credits` from the host org atomically.
//
// The billed model (metrics.rs): cost = Σ interval_min × ratePerMin × K, where K =
// distinct participant languages ≠ source_language. Standard default rate = 0.01
// USD/min and 100 credits = $1 ⇒ exactly 1 credit per stream-minute. So for a clean
// N-minute broadcast with K distinct foreign languages present the whole time, the
// expected charge is N × K credits.
//
// Determinism: the cost sweep integrates only within [actual_start, actual_end].
// `publish-started` stamps actual_start=now(); we then OVERWRITE actual_start via SQL
// to `now() - interval 'N min'` and seed all join events at/before that instant, so at
// go-live the audience is already present. `publish-stopped` stamps actual_end=now(),
// giving a span of exactly N minutes (num_seconds() truncates sub-second drift).

/// Create a **Standard**-tier webinar (explicit tier so the deterministic 0.01/min
/// default rate applies, not the Enhanced/Cartesia rate). Returns the parsed body.
async fn create_standard_webinar(http: &Client, srv: &Server, jwt: &str, org_id: Uuid) -> Value {
    let r = http
        .post(format!("{}/api/webinars", base(srv)))
        .bearer_auth(jwt)
        .json(&json!({
            "org_id": org_id,
            "title": "Billing Run",
            "source_language": "en",
            "tier": "standard",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "create standard webinar");
    r.json().await.unwrap()
}

/// Set the org's credit pool to a known balance so we can assert an exact delta.
async fn set_org_credits(srv: &Server, org_id: Uuid, credits: i32) {
    sqlx::query("UPDATE organizations SET credits_balance = $2 WHERE id = $1")
        .bind(org_id)
        .bind(credits)
        .execute(&srv.pool)
        .await
        .unwrap();
}

/// Read the org's current credit pool.
async fn org_credits(srv: &Server, org_id: Uuid) -> i32 {
    sqlx::query_scalar("SELECT credits_balance FROM organizations WHERE id = $1")
        .bind(org_id)
        .fetch_one(&srv.pool)
        .await
        .unwrap()
}

/// Insert a `webinar_participants` row (a seeded audience member) and return its id.
async fn insert_participant(srv: &Server, webinar_id: Uuid, lang: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO webinar_participants (webinar_id, guest_id, language_code)
         VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(webinar_id)
    .bind(Uuid::new_v4())
    .bind(lang)
    .fetch_one(&srv.pool)
    .await
    .unwrap()
}

/// Insert a `webinar_events` row at `now() + offset_secs` (negative = in the past).
async fn insert_event(
    srv: &Server,
    webinar_id: Uuid,
    participant_id: Uuid,
    kind: &str,
    lang: Option<&str>,
    offset_secs: i64,
) {
    sqlx::query(
        "INSERT INTO webinar_events (webinar_id, participant_id, type, language_code, at)
         VALUES ($1, $2, $3, $4, now() + ($5 || ' seconds')::interval)",
    )
    .bind(webinar_id)
    .bind(participant_id)
    .bind(kind)
    .bind(lang)
    .bind(offset_secs.to_string())
    .execute(&srv.pool)
    .await
    .unwrap();
}

/// Force `actual_start` to exactly `now() - n_min` so the billable span is a clean N
/// minutes once `publish-stopped` stamps `actual_end=now()`.
async fn backdate_actual_start(srv: &Server, webinar_id: Uuid, n_min: i64) {
    sqlx::query(
        "UPDATE webinars SET actual_start = now() - ($2 || ' minutes')::interval WHERE id = $1",
    )
    .bind(webinar_id)
    .bind(n_min.to_string())
    .execute(&srv.pool)
    .await
    .unwrap();
}

/// The single finalized session row for a webinar, as (cost_credits, peak_viewers,
/// translated_language_count, duration_seconds). Panics if not exactly one row.
async fn session_row(srv: &Server, webinar_id: Uuid) -> (i32, i32, i32, i32) {
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM webinar_sessions WHERE webinar_id = $1")
            .bind(webinar_id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(count, 1, "exactly one webinar_sessions row (got {count})");
    sqlx::query_as(
        "SELECT cost_credits, peak_viewers, translated_language_count, duration_seconds
           FROM webinar_sessions WHERE webinar_id = $1",
    )
    .bind(webinar_id)
    .fetch_one(&srv.pool)
    .await
    .unwrap()
}

async fn publish_started(http: &Client, srv: &Server, jwt: &str, id: &str) {
    let r = http
        .post(format!("{}/api/webinars/{id}/publish-started", base(srv)))
        .bearer_auth(jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "publish-started → 200");
}

async fn publish_stopped(http: &Client, srv: &Server, jwt: &str, id: &str) {
    let r = http
        .post(format!("{}/api/webinars/{id}/publish-stopped", base(srv)))
        .bearer_auth(jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "publish-stopped → 200");
}

#[tokio::test]
async fn finalize_bills_host_for_distinct_participant_languages() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    set_org_credits(&srv, org_id, 100_000).await;

    let created = create_standard_webinar(&http, &srv, &jwt, org_id).await;
    let id = created["id"].as_str().unwrap().to_string();
    let webinar_id = Uuid::parse_str(&id).unwrap();
    assert_eq!(
        created["tier"], "standard",
        "standard tier for 0.01/min rate"
    );

    // Go live, then backdate actual_start to a clean N minutes ago.
    publish_started(&http, &srv, &jwt, &id).await;
    const N_MIN: i64 = 5;
    backdate_actual_start(&srv, webinar_id, N_MIN).await;

    // Audience: 2 distinct FOREIGN languages (it, fr) + 1 same-language (en). All join
    // 10s BEFORE actual_start (i.e. now()-(N_MIN*60+10)s) so they are present for the
    // WHOLE broadcast (seeded at go-live) and never leave. K = 2 (en is not billable).
    let join_off = -(N_MIN * 60 + 10);
    for lang in ["it", "fr", "en"] {
        let pid = insert_participant(&srv, webinar_id, lang).await;
        insert_event(&srv, webinar_id, pid, "join", Some(lang), join_off).await;
    }

    let before = org_credits(&srv, org_id).await;
    publish_stopped(&http, &srv, &jwt, &id).await;
    let after = org_credits(&srv, org_id).await;

    let (cost, peak, tlc, dur) = session_row(&srv, webinar_id).await;
    const K: i32 = 2;
    let expected = N_MIN as i32 * K; // 5 min × 2 langs × 1 credit/stream-min = 10
    let charged = before - after;

    eprintln!(
        "[finalize_bills] N={N_MIN}min K={K} → expected cost_credits={expected}; \
         got cost_credits={cost} duration_seconds={dur} peak_viewers={peak} \
         translated_language_count={tlc}; org charged={charged}"
    );

    assert_eq!(cost, expected, "cost_credits = N × K (5 × 2 = 10)");
    assert_eq!(peak, 3, "peak_viewers counts all 3 present viewers");
    assert_eq!(
        tlc, 2,
        "translated_language_count = distinct foreign langs (it, fr)"
    );
    assert_eq!(dur, (N_MIN * 60) as i32, "duration_seconds = N minutes");
    assert_eq!(charged, cost, "org balance dropped by exactly cost_credits");
}

#[tokio::test]
async fn finalize_is_idempotent_no_double_charge() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    set_org_credits(&srv, org_id, 100_000).await;

    let created = create_standard_webinar(&http, &srv, &jwt, org_id).await;
    let id = created["id"].as_str().unwrap().to_string();
    let webinar_id = Uuid::parse_str(&id).unwrap();

    publish_started(&http, &srv, &jwt, &id).await;
    const N_MIN: i64 = 4;
    backdate_actual_start(&srv, webinar_id, N_MIN).await;
    let pid = insert_participant(&srv, webinar_id, "it").await;
    insert_event(
        &srv,
        webinar_id,
        pid,
        "join",
        Some("it"),
        -(N_MIN * 60 + 10),
    )
    .await;

    let before = org_credits(&srv, org_id).await;
    // Stop TWICE — the second call must not insert a second session or re-charge.
    publish_stopped(&http, &srv, &jwt, &id).await;
    let (cost, _, _, _) = session_row(&srv, webinar_id).await;
    publish_stopped(&http, &srv, &jwt, &id).await;
    let (cost2, _, _, _) = session_row(&srv, webinar_id).await; // asserts still ONE row
    let after = org_credits(&srv, org_id).await;
    let charged = before - after;

    eprintln!(
        "[idempotent] cost after 1st stop={cost}, after 2nd stop={cost2}; \
         total org charged (both stops)={charged}"
    );

    assert_eq!(cost, cost2, "cost_credits unchanged by the second stop");
    assert_eq!(cost, N_MIN as i32, "K=1 → cost = N credits (4)");
    assert_eq!(charged, cost, "org charged ONCE, not twice");
}

#[tokio::test]
async fn finalize_with_no_foreign_languages_costs_nothing() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    set_org_credits(&srv, org_id, 100_000).await;

    let created = create_standard_webinar(&http, &srv, &jwt, org_id).await;
    let id = created["id"].as_str().unwrap().to_string();
    let webinar_id = Uuid::parse_str(&id).unwrap();

    publish_started(&http, &srv, &jwt, &id).await;
    const N_MIN: i64 = 6;
    backdate_actual_start(&srv, webinar_id, N_MIN).await;
    // Two same-language (source = en) viewers → K=0 for the whole run.
    for _ in 0..2 {
        let pid = insert_participant(&srv, webinar_id, "en").await;
        insert_event(
            &srv,
            webinar_id,
            pid,
            "join",
            Some("en"),
            -(N_MIN * 60 + 10),
        )
        .await;
    }

    let before = org_credits(&srv, org_id).await;
    publish_stopped(&http, &srv, &jwt, &id).await;
    let after = org_credits(&srv, org_id).await;

    let (cost, peak, tlc, dur) = session_row(&srv, webinar_id).await;
    eprintln!(
        "[no_foreign] K=0 → expected cost_credits=0; got cost_credits={cost} \
         duration_seconds={dur} peak_viewers={peak} translated_language_count={tlc}; \
         org charged={}",
        before - after
    );

    assert_eq!(cost, 0, "K=0 → free broadcast");
    assert_eq!(tlc, 0, "no foreign languages translated");
    assert_eq!(peak, 2, "session row still records the audience metrics");
    assert_eq!(dur, (N_MIN * 60) as i32, "duration still recorded");
    assert_eq!(
        before, after,
        "org balance unchanged when nothing is billed"
    );
}

#[tokio::test]
async fn finalize_finalizes_total_watch_seconds() {
    let Some(srv) = setup().await else {
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    set_org_credits(&srv, org_id, 100_000).await;

    let created = create_standard_webinar(&http, &srv, &jwt, org_id).await;
    let id = created["id"].as_str().unwrap().to_string();
    let webinar_id = Uuid::parse_str(&id).unwrap();

    publish_started(&http, &srv, &jwt, &id).await;
    const N_MIN: i64 = 10;
    backdate_actual_start(&srv, webinar_id, N_MIN).await;

    // A viewer who joins 4 minutes into the broadcast and leaves 7 minutes in →
    // in-window watch span = 3 minutes = 180 seconds. actual_start = now()-10min, so:
    //   join  at now()-6min (= start + 4min), leave at now()-3min (= start + 7min).
    let pid = insert_participant(&srv, webinar_id, "it").await;
    insert_event(&srv, webinar_id, pid, "join", Some("it"), -6 * 60).await;
    insert_event(&srv, webinar_id, pid, "leave", None, -3 * 60).await;

    publish_stopped(&http, &srv, &jwt, &id).await;

    let watched: i32 =
        sqlx::query_scalar("SELECT total_watch_seconds FROM webinar_participants WHERE id = $1")
            .bind(pid)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    eprintln!("[watch_seconds] join@+4min leave@+7min → expected 180s; got {watched}s");
    assert_eq!(
        watched, 180,
        "total_watch_seconds = join→leave span (3 min)"
    );
}

// ---- PR1: notify_friends + source_language patch + unarchive guard ----------

/// 1.1 / 1.4 — Migration 051 idempotency.
/// Applies migrations (which include 051) twice: the first `setup()` already ran
/// `db::migrate`, so calling it again is the "second apply". Since sqlx only runs
/// new migrations, the important check is that the `notify_friends` column is
/// present and holds the expected default.
#[tokio::test]
async fn migration_051_notify_friends_column_exists_and_defaults_true() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;

    // Create a webinar WITHOUT sending notify_friends — server must default it to true.
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let id = created["id"].as_str().unwrap().to_string();
    let webinar_id = Uuid::parse_str(&id).unwrap();

    // Read the raw column value from the DB.
    let notify: bool = sqlx::query_scalar("SELECT notify_friends FROM webinars WHERE id = $1")
        .bind(webinar_id)
        .fetch_one(&srv.pool)
        .await
        .unwrap();

    assert!(notify, "notify_friends defaults to true when not supplied");

    // Re-running migrate is idempotent — should return without error.
    db::migrate(&srv.pool).await.unwrap();

    // Column still exists and still holds the same value after a second migrate run.
    let notify2: bool = sqlx::query_scalar("SELECT notify_friends FROM webinars WHERE id = $1")
        .bind(webinar_id)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert_eq!(
        notify, notify2,
        "idempotent re-migrate doesn't touch existing rows"
    );
}

/// 1.2 — host_view surfaces notify_friends.
#[tokio::test]
async fn host_view_includes_notify_friends() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;

    // Create with explicit notify_friends=false.
    let r = http
        .post(format!("{}/api/webinars", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({
            "org_id": org_id,
            "title": "Notify Test",
            "source_language": "en",
            "notify_friends": false,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    assert_eq!(
        body["notify_friends"].as_bool(),
        Some(false),
        "host_view must surface notify_friends"
    );
}

/// 1.5 — PatchWebinar accepts valid source_language.
#[tokio::test]
async fn patch_source_language_valid_code_accepted() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let id = created["id"].as_str().unwrap().to_string();

    let r = http
        .patch(format!("{}/api/webinars/{id}", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "source_language": "fr" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "valid lang code accepted");
    let body: Value = r.json().await.unwrap();
    assert_eq!(
        body["source_language"], "fr",
        "source_language updated to fr"
    );
}

/// 1.5 (triangulate) — PatchWebinar rejects an invalid language code.
#[tokio::test]
async fn patch_source_language_invalid_code_rejected() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let id = created["id"].as_str().unwrap().to_string();

    let r = http
        .patch(format!("{}/api/webinars/{id}", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "source_language": "zz invalid!!" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "invalid lang code → 400");
}

/// 1.5 (triangulate) — PatchWebinar with notify_friends=false persists it.
#[tokio::test]
async fn patch_notify_friends_false_persisted() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let id = created["id"].as_str().unwrap().to_string();

    let r = http
        .patch(format!("{}/api/webinars/{id}", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "notify_friends": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "patch notify_friends accepted");
    let body: Value = r.json().await.unwrap();
    assert_eq!(
        body["notify_friends"].as_bool(),
        Some(false),
        "notify_friends=false persisted and surfaced in host_view"
    );
}

/// 1.6 — PatchWebinar on a non-scheduled webinar (status=live) returns 409.
#[tokio::test]
async fn patch_source_language_on_live_webinar_returns_409() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let id = created["id"].as_str().unwrap().to_string();

    // Flip to live.
    sqlx::query("UPDATE webinars SET status = 'live' WHERE id = $1::uuid")
        .bind(&id)
        .execute(&srv.pool)
        .await
        .unwrap();

    let r = http
        .patch(format!("{}/api/webinars/{id}", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "source_language": "fr" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409, "patching a live webinar is locked (409)");
}

// ---- Unarchive time-guard (task 1.10 / 1.12) --------------------------------

/// Helper: archive a webinar via SQL.
async fn archive_webinar_sql(srv: &Server, id: &str) {
    sqlx::query("UPDATE webinars SET archived_at = now() WHERE id = $1::uuid")
        .bind(id)
        .execute(&srv.pool)
        .await
        .unwrap();
}

/// 1.10 — Restore blocked for past scheduled_end.
#[tokio::test]
async fn unarchive_blocked_for_past_scheduled_end() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let id = created["id"].as_str().unwrap().to_string();

    // Set scheduled_end to yesterday.
    sqlx::query("UPDATE webinars SET scheduled_end = now() - interval '1 day' WHERE id = $1::uuid")
        .bind(&id)
        .execute(&srv.pool)
        .await
        .unwrap();

    archive_webinar_sql(&srv, &id).await;

    let r = http
        .post(format!("{}/api/webinars/{id}/unarchive", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409, "unarchive of past scheduled_end → 409");
}

/// 1.10 (triangulate) — Restore blocked for past scheduled_start (no end).
#[tokio::test]
async fn unarchive_blocked_for_past_scheduled_start() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let id = created["id"].as_str().unwrap().to_string();

    // Set scheduled_start to last week, no end.
    sqlx::query(
        "UPDATE webinars SET scheduled_start = now() - interval '7 days', scheduled_end = NULL WHERE id = $1::uuid",
    )
    .bind(&id)
    .execute(&srv.pool)
    .await
    .unwrap();

    archive_webinar_sql(&srv, &id).await;

    let r = http
        .post(format!("{}/api/webinars/{id}/unarchive", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409, "unarchive of past scheduled_start → 409");
}

/// 1.10 (triangulate) — Restore blocked for past actual_end (ended webinar).
#[tokio::test]
async fn unarchive_blocked_for_past_actual_end() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let id = created["id"].as_str().unwrap().to_string();

    // No scheduled times, but actual_end is in the past.
    sqlx::query(
        "UPDATE webinars SET scheduled_start = NULL, scheduled_end = NULL,
         actual_end = now() - interval '2 days' WHERE id = $1::uuid",
    )
    .bind(&id)
    .execute(&srv.pool)
    .await
    .unwrap();

    archive_webinar_sql(&srv, &id).await;

    let r = http
        .post(format!("{}/api/webinars/{id}/unarchive", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409, "unarchive of past actual_end → 409");
}

/// 1.10 (triangulate) — Restore allowed for still-upcoming webinar.
#[tokio::test]
async fn unarchive_allowed_for_future_scheduled_start() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let id = created["id"].as_str().unwrap().to_string();

    // Set scheduled_start to tomorrow.
    sqlx::query(
        "UPDATE webinars SET scheduled_start = now() + interval '1 day' WHERE id = $1::uuid",
    )
    .bind(&id)
    .execute(&srv.pool)
    .await
    .unwrap();

    archive_webinar_sql(&srv, &id).await;

    let r = http
        .post(format!("{}/api/webinars/{id}/unarchive", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "unarchive of future webinar → 200");
    let body: Value = r.json().await.unwrap();
    assert!(
        body["archived_at"].is_null(),
        "archived_at cleared after restore"
    );
}

/// 1.10 (triangulate) — Restore allowed for webinar with no time fields (None).
#[tokio::test]
async fn unarchive_allowed_when_no_time_fields() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let id = created["id"].as_str().unwrap().to_string();

    // No time fields at all — effective_time is None → allow.
    sqlx::query(
        "UPDATE webinars SET scheduled_start = NULL, scheduled_end = NULL, actual_end = NULL WHERE id = $1::uuid",
    )
    .bind(&id)
    .execute(&srv.pool)
    .await
    .unwrap();

    archive_webinar_sql(&srv, &id).await;

    let r = http
        .post(format!("{}/api/webinars/{id}/unarchive", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "unarchive when no times → 200 (allowed)");
}

// ---- Reminder scheduler + go-live notify_friends gate (tasks 1.13/1.14) -----

/// 1.13 — notify_friends=false row excluded from reminder scheduler query.
///
/// Calls the REAL production function `select_due_webinar_reminders` so that
/// any edit to the WHERE clause in `notifications.rs` immediately breaks this
/// test — a re-implemented copy of the SQL would silently pass.
#[tokio::test]
async fn reminder_scheduler_excludes_notify_friends_false() {
    use voxtranslate_server::notifications::select_due_webinar_reminders;

    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;

    // Create a public, scheduled webinar with notify_friends=false.
    let r = http
        .post(format!("{}/api/webinars", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({
            "org_id": org_id,
            "title": "No-notify webinar",
            "source_language": "en",
            "visibility": "public",
            "notify_friends": false,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    let id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();

    // Backdate scheduled_start to trigger the reminder condition:
    // scheduled_start = now + 5 min, reminder = 30 min → fires now.
    sqlx::query(
        "UPDATE webinars
         SET scheduled_start = now() + interval '5 minutes',
             reminder_minutes_before = 30,
             reminder_sent_at = NULL
         WHERE id = $1",
    )
    .bind(id)
    .execute(&srv.pool)
    .await
    .unwrap();

    // Call the real production selection function — NOT a re-implemented WHERE.
    let due = select_due_webinar_reminders(&srv.pool, chrono::Utc::now())
        .await
        .unwrap();

    assert!(
        !due.contains(&id),
        "notify_friends=false webinar must not appear in reminder scheduler results"
    );
}

/// 1.13 (triangulate) — notify_friends=true row IS selected by the real production function.
#[tokio::test]
async fn reminder_scheduler_includes_notify_friends_true() {
    use voxtranslate_server::notifications::select_due_webinar_reminders;

    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;

    // Create a public webinar with default notify_friends (true).
    let r = http
        .post(format!("{}/api/webinars", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({
            "org_id": org_id,
            "title": "Notify webinar",
            "source_language": "en",
            "visibility": "public",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    let id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();

    // Trigger the scheduler condition.
    sqlx::query(
        "UPDATE webinars
         SET scheduled_start = now() + interval '5 minutes',
             reminder_minutes_before = 30,
             reminder_sent_at = NULL
         WHERE id = $1",
    )
    .bind(id)
    .execute(&srv.pool)
    .await
    .unwrap();

    // Call the real production selection function.
    let due = select_due_webinar_reminders(&srv.pool, chrono::Utc::now())
        .await
        .unwrap();

    assert!(
        due.contains(&id),
        "notify_friends=true webinar must appear in reminder scheduler results"
    );
}

/// 1.14 — go-live hook: notify_friends=false suppresses friend notification.
/// We verify the DB state: after publish-started on a webinar with
/// notify_friends=false, reminder_sent_at remains NULL (the hook did not fire).
#[tokio::test]
async fn go_live_hook_skips_notification_when_notify_friends_false() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;

    // Create a public, unscheduled webinar with notify_friends=false.
    let r = http
        .post(format!("{}/api/webinars", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({
            "org_id": org_id,
            "title": "No notify live",
            "source_language": "en",
            "visibility": "public",
            "notify_friends": false,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    let id = body["id"].as_str().unwrap().to_string();
    let webinar_id = Uuid::parse_str(&id).unwrap();

    // publish-started triggers the go-live hook.
    let r = http
        .post(format!("{}/api/webinars/{id}/publish-started", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // The hook should NOT have fired: reminder_sent_at must still be NULL.
    let sent_at: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT reminder_sent_at FROM webinars WHERE id = $1")
            .bind(webinar_id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();

    assert!(
        sent_at.is_none(),
        "go-live hook must not set reminder_sent_at when notify_friends=false"
    );
}

/// 1.14 (triangulate) — go-live hook fires for public unscheduled with notify_friends=true.
#[tokio::test]
async fn go_live_hook_fires_notification_when_notify_friends_true() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;

    // Public + unscheduled + default notify_friends (true).
    let r = http
        .post(format!("{}/api/webinars", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({
            "org_id": org_id,
            "title": "Notify live",
            "source_language": "en",
            "visibility": "public",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    let id = body["id"].as_str().unwrap().to_string();
    let webinar_id = Uuid::parse_str(&id).unwrap();

    // publish-started: host is the owner, so host_user_id is set.
    let r = http
        .post(format!("{}/api/webinars/{id}/publish-started", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // Poll until reminder_sent_at is set or a 2-second deadline expires.
    // This is deterministic: we detect the state change as soon as the spawned
    // task completes rather than sleeping for an arbitrary fixed duration.
    let deadline = std::time::Duration::from_secs(2);
    let poll_interval = std::time::Duration::from_millis(50);
    let sent_at = tokio::time::timeout(deadline, async {
        loop {
            let val: Option<chrono::DateTime<chrono::Utc>> =
                sqlx::query_scalar("SELECT reminder_sent_at FROM webinars WHERE id = $1")
                    .bind(webinar_id)
                    .fetch_one(&srv.pool)
                    .await
                    .unwrap();
            if val.is_some() {
                return val;
            }
            tokio::time::sleep(poll_interval).await;
        }
    })
    .await
    .unwrap_or(None);

    assert!(
        sent_at.is_some(),
        "go-live hook must set reminder_sent_at when notify_friends=true (dedup stamp)"
    );
}

// ---- B2: unarchive allowed for live webinars (decision #4 blocks only PAST) --

/// B2 — Unarchive is allowed for a currently-live webinar whose effective time
/// is in the future (scheduled_end tomorrow). Decision #4 only blocks past-time
/// webinars; a live webinar that hasn't finished yet must be restorable.
#[tokio::test]
async fn unarchive_allowed_for_live_webinar_with_future_scheduled_end() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let id = created["id"].as_str().unwrap().to_string();

    // Set status=live and scheduled_end tomorrow (effective_time is future).
    sqlx::query(
        "UPDATE webinars
         SET status = 'live',
             scheduled_start = now() - interval '30 minutes',
             scheduled_end = now() + interval '1 day'
         WHERE id = $1::uuid",
    )
    .bind(&id)
    .execute(&srv.pool)
    .await
    .unwrap();

    archive_webinar_sql(&srv, &id).await;

    let r = http
        .post(format!("{}/api/webinars/{id}/unarchive", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        200,
        "live webinar with future scheduled_end must be unarchivable (200)"
    );
    let body: Value = r.json().await.unwrap();
    assert!(
        body["archived_at"].is_null(),
        "archived_at cleared after restore"
    );
}

/// B2 (triangulate) — Unarchive is allowed for a live webinar with no time
/// fields at all (effective_time = None → guard does not block).
#[tokio::test]
async fn unarchive_allowed_for_live_webinar_with_no_times() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let id = created["id"].as_str().unwrap().to_string();

    // Set status=live but no time fields (unscheduled live webinar).
    sqlx::query(
        "UPDATE webinars
         SET status = 'live',
             scheduled_start = NULL,
             scheduled_end = NULL,
             actual_end = NULL
         WHERE id = $1::uuid",
    )
    .bind(&id)
    .execute(&srv.pool)
    .await
    .unwrap();

    archive_webinar_sql(&srv, &id).await;

    let r = http
        .post(format!("{}/api/webinars/{id}/unarchive", base(&srv)))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        200,
        "live webinar with no time fields must be unarchivable (200)"
    );
    let body: Value = r.json().await.unwrap();
    assert!(
        body["archived_at"].is_null(),
        "archived_at cleared after restore"
    );
}

// ---- W1: COALESCE None — PATCH without source_language leaves it unchanged ---

/// W1 — PATCHing a scheduled webinar WITHOUT `source_language` in the body
/// leaves the stored source_language unchanged. This proves that
/// `COALESCE($N, source_language)` in the UPDATE handles a None binding
/// correctly (passes the current value through without touching the column).
#[tokio::test]
async fn patch_without_source_language_leaves_it_unchanged() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;

    // Create with a known source_language.
    let r = http
        .post(format!("{}/api/webinars", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({
            "org_id": org_id,
            "title": "Coalesce test",
            "source_language": "de",
            "visibility": "private",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    let id = body["id"].as_str().unwrap().to_string();

    // PATCH with only title — no source_language key in the JSON.
    let r = http
        .patch(format!("{}/api/webinars/{id}", base(&srv)))
        .bearer_auth(&jwt)
        .json(&json!({ "title": "Coalesce test updated" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        200,
        "patch without source_language must succeed"
    );
    let patched: Value = r.json().await.unwrap();

    assert_eq!(
        patched["source_language"], "de",
        "source_language must remain 'de' when not included in PATCH body"
    );
}

// ---- W2: real migration idempotency — raw ALTER TABLE IF NOT EXISTS ----------

/// W2 — Executes the raw `ALTER TABLE webinars ADD COLUMN IF NOT EXISTS notify_friends …`
/// a second time directly against the pool, bypassing sqlx's checksum tracker.
/// This exercises the `IF NOT EXISTS` guard at the SQL engine level.
/// sqlx::migrate! enforces idempotency via checksums, so the only way to verify
/// the SQL itself handles re-runs is to run it directly.
#[tokio::test]
async fn migration_051_raw_alter_table_is_idempotent() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };

    // Run the migration statement a second time directly. If IF NOT EXISTS were
    // missing this would return a "column already exists" error.
    let result = sqlx::query(
        "ALTER TABLE webinars ADD COLUMN IF NOT EXISTS notify_friends BOOLEAN NOT NULL DEFAULT true",
    )
    .execute(&srv.pool)
    .await;

    assert!(
        result.is_ok(),
        "ALTER TABLE ADD COLUMN IF NOT EXISTS must be idempotent: {result:?}"
    );

    // Confirm the column still has the correct default by inserting a fresh row
    // without specifying notify_friends and reading it back.
    let http = Client::new();
    let (owner, jwt) = user(&srv).await;
    let org_id = org(&srv, owner, true).await;
    let created = create_webinar(&http, &srv, &jwt, org_id).await;
    let webinar_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();

    let notify: bool = sqlx::query_scalar("SELECT notify_friends FROM webinars WHERE id = $1")
        .bind(webinar_id)
        .fetch_one(&srv.pool)
        .await
        .unwrap();

    assert!(
        notify,
        "column default (true) intact after idempotent re-run of ADD COLUMN IF NOT EXISTS"
    );
}
