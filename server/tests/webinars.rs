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
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
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

    // Unauthenticated call → 401.
    let unauth = http
        .get(format!("{}/api/webinars/public", base(&srv)))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), 401, "unauthenticated public list → 401");
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
