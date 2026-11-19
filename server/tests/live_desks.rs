//! Desks (spec 0121, S6/S14): permanent QR codes, and the staff side of a scan.
//!
//! The properties that matter: a QR is never a conversation (every scan is a NEW session), a
//! printed code can be revoked or replaced, a stranger learns nothing about which desks or orgs
//! exist, exactly one colleague answers a waiting customer, and the queue cannot be flooded.
//!
//! DB-gated like the other integration suites. Run against the local pgvector Postgres:
//! `DATABASE_URL=postgres://vox:vox_local_dev@127.0.0.1:55432/voxtranslate_voip_test \
//!   cargo test --test live_desks`. A run of a few milliseconds skipped everything.

use std::net::SocketAddr;
use std::sync::Arc;

use futures::StreamExt as _;
use reqwest::StatusCode;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::{Config, QwenConfig};
use voxtranslate_server::db::{self, Pool};
use voxtranslate_server::engine::realtime_mock::{Dialect, RealtimeMock};
use voxtranslate_server::live::config::LiveConfig;
use voxtranslate_server::live::LiveState;
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::transcripts::TranscriptService;
use voxtranslate_server::{app, AppState};

const SECRET: &str = "live-desks-secret";
const LIVE_KEY: &str = "0123456789abcdef0123456789abcdef-live-desks";

struct Srv {
    addr: SocketAddr,
    pool: Pool,
    http: reqwest::Client,
    _mock: RealtimeMock,
}

async fn setup(max_waiting: Option<&'static str>) -> Option<Srv> {
    let url = db::test_database_url()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let mut config = Config::test_with_billing(&url, SECRET, 5.0);
    config.qwen = QwenConfig {
        api_key: "test-key".into(),
        endpoint: mock.base_url(),
        ..Default::default()
    };
    config.standard_enabled = true;
    let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);
    let mut state = AppState::new(config);
    state.billing = Some(BillingService::new(pool.clone(), min_join));
    state.safety = Some(SafetyService::new(pool.clone()));
    state.transcripts = Some(TranscriptService::new(pool.clone()));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);
    state.live = Arc::new(LiveState::new(
        LiveConfig::load(|k| match k {
            "LIVE_CONNECT_ENABLED" => Some("1".into()),
            "LIVE_TOKEN_KEY" => Some(LIVE_KEY.into()),
            "LIVE_DESK_MAX_WAITING" => max_waiting.map(Into::into),
            _ => None,
        })
        .unwrap(),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    Some(Srv {
        addr,
        pool,
        http: reqwest::Client::new(),
        _mock: mock,
    })
}

macro_rules! srv {
    () => {
        srv!(None)
    };
    ($max:expr) => {
        match setup($max).await {
            Some(s) => s,
            None => {
                eprintln!("skipping — no DATABASE_URL");
                return;
            }
        }
    };
}

impl Srv {
    async fn call(
        &self,
        method: &str,
        path: &str,
        bearer: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let url = format!("http://{}{path}", self.addr);
        let mut req = match method {
            "GET" => self.http.get(url),
            "PATCH" => self.http.patch(url),
            _ => self.http.post(url),
        };
        if let Some(t) = bearer {
            req = req.bearer_auth(t);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req.send().await.unwrap();
        let status = res.status();
        let text = res.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }
    async fn post(&self, path: &str, bearer: Option<&str>, body: Value) -> (StatusCode, Value) {
        self.call("POST", path, bearer, Some(body)).await
    }
    async fn get(&self, path: &str, bearer: &str) -> (StatusCode, Value) {
        self.call("GET", path, Some(bearer), None).await
    }
}

async fn login(srv: &Srv, name: &str) -> (String, Uuid) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: name.into(),
        avatar_url: None,
    };
    let (user, _) = upsert_google_user(
        &srv.pool,
        &identity,
        rust_decimal::Decimal::new(500, 2),
        None,
        None,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE users SET age_confirmed = TRUE, consent_tos_at = now() WHERE id = $1")
        .bind(user.id)
        .execute(&srv.pool)
        .await
        .unwrap();
    (
        issue_jwt(SECRET, &user.id, &user.email, &user.name, 168).unwrap(),
        user.id,
    )
}

struct Org {
    id: Uuid,
    project: Uuid,
    owner_jwt: String,
    member_jwt: String,
    member: Uuid,
}

/// An org with an active subscription and credit, one project, an owner (admin) and a member.
async fn org(srv: &Srv) -> Org {
    let (owner_jwt, owner) = login(srv, "Owner").await;
    let (member_jwt, member) = login(srv, "Receptionist").await;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id, subscription_status, current_period_end, credits_balance)
         VALUES ('Hotel', $1, $2, 'active', now() + interval '30 days', 1000) RETURNING id",
    )
    .bind(format!("slug-{}", Uuid::new_v4().simple()))
    .bind(owner)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    for (user, role) in [(owner, "owner"), (member, "member")] {
        sqlx::query("INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
            .bind(id)
            .bind(user)
            .bind(role)
            .execute(&srv.pool)
            .await
            .unwrap();
    }
    let project: Uuid = sqlx::query_scalar(
        "INSERT INTO projects (org_id, name, created_by) VALUES ($1, 'Reception', $2) RETURNING id",
    )
    .bind(id)
    .bind(owner)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    Org {
        id,
        project,
        owner_jwt,
        member_jwt,
        member,
    }
}

fn desks_path(org: &Org) -> String {
    format!("/api/business/organizations/{}/live/desks", org.id)
}

/// Create a desk as the org owner. Returns `(desk id, qr url)`.
async fn new_desk(srv: &Srv, org: &Org) -> (Uuid, String) {
    let (s, desk) = srv
        .post(
            &desks_path(org),
            Some(&org.owner_jwt),
            json!({ "name": "Front desk", "location_label": "Lobby", "default_lang": "it", "project_id": org.project }),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{desk}");
    (
        desk["id"].as_str().unwrap().parse().unwrap(),
        desk["qr_url"].as_str().unwrap().to_string(),
    )
}

fn segment_of(qr_url: &str) -> String {
    qr_url.rsplit('/').next().unwrap().to_string()
}

async fn scan(srv: &Srv, segment: &str, name: &str, lang: &str) -> (StatusCode, Value) {
    srv.post(
        "/api/live/desks/scan",
        None,
        json!({ "segment": segment, "name": name, "lang": lang, "consent": true }),
    )
    .await
}

// ---- administration --------------------------------------------------------------------

#[tokio::test]
async fn only_admins_manage_desks_and_the_project_must_be_the_orgs() {
    let srv = srv!();
    let org = org(&srv).await;
    let (stranger_jwt, _) = login(&srv, "Stranger").await;
    let body = json!({ "name": "Desk", "default_lang": "it", "project_id": org.project });

    // A plain member cannot create; a non-member cannot even tell the org exists.
    assert_eq!(
        srv.post(&desks_path(&org), Some(&org.member_jwt), body.clone())
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        srv.post(&desks_path(&org), Some(&stranger_jwt), body.clone())
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    // The project must be this org's, live, and given.
    let other = self::org(&srv).await;
    let (s, b) = srv
        .post(
            &desks_path(&org),
            Some(&org.owner_jwt),
            json!({ "name": "D", "default_lang": "it", "project_id": other.project }),
        )
        .await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("project_not_found"))
    );
    sqlx::query("UPDATE projects SET archived_at = now() WHERE id = $1")
        .bind(org.project)
        .execute(&srv.pool)
        .await
        .unwrap();
    let (s, b) = srv
        .post(&desks_path(&org), Some(&org.owner_jwt), body.clone())
        .await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("project_not_found"))
    );
    sqlx::query("UPDATE projects SET archived_at = NULL WHERE id = $1")
        .bind(org.project)
        .execute(&srv.pool)
        .await
        .unwrap();
    let (s, b) = srv
        .post(
            &desks_path(&org),
            Some(&org.owner_jwt),
            json!({ "name": "   ", "default_lang": "it", "project_id": org.project }),
        )
        .await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("name_required"))
    );
    let (s, b) = srv
        .post(
            &desks_path(&org),
            Some(&org.owner_jwt),
            json!({ "name": "D", "default_lang": "auto", "project_id": org.project }),
        )
        .await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("bad_lang"))
    );

    // Created: the printed URL carries a handle and a MAC and NOTHING identifying.
    let (desk, qr) = new_desk(&srv, &org).await;
    assert!(qr.contains("/lc/d/"), "{qr}");
    for secret in [
        org.id.to_string(),
        desk.to_string(),
        org.project.to_string(),
    ] {
        assert!(!qr.contains(&secret), "the QR leaked an internal id: {qr}");
    }

    // Members may look at the list (staff pick a desk); strangers may not; the QR is admin-only.
    let (s, list) = srv.get(&desks_path(&org), &org.member_jwt).await;
    assert_eq!(
        (s, list["desks"].as_array().map(Vec::len)),
        (StatusCode::OK, Some(1))
    );
    assert!(
        !list.to_string().contains("/lc/d/"),
        "the list must not hand out the credential"
    );
    assert_eq!(
        srv.get(&desks_path(&org), &stranger_jwt).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        srv.get(&format!("{}/{desk}/qr", desks_path(&org)), &org.member_jwt)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (s, q) = srv
        .get(&format!("{}/{desk}/qr", desks_path(&org)), &org.owner_jwt)
        .await;
    assert_eq!(
        (s, q["url"].as_str()),
        (StatusCode::OK, Some(qr.as_str())),
        "the QR can be downloaded again at any time"
    );

    // Rename / relocate.
    let (s, edited) = srv
        .call(
            "PATCH",
            &format!("{}/{desk}", desks_path(&org)),
            Some(&org.owner_jwt),
            Some(json!({ "name": "Concierge", "default_lang": "en" })),
        )
        .await;
    assert_eq!(
        (s, edited["name"].as_str(), edited["default_lang"].as_str()),
        (StatusCode::OK, Some("Concierge"), Some("en"))
    );
    let (s, _) = srv
        .call(
            "PATCH",
            &format!("{}/{desk}", desks_path(&org)),
            Some(&org.member_jwt),
            Some(json!({ "name": "x" })),
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

// ---- the QR is never a conversation ----------------------------------------------------

#[tokio::test]
async fn every_scan_creates_a_new_session_and_the_qr_never_names_one() {
    let srv = srv!();
    let org = org(&srv).await;
    let (desk, qr) = new_desk(&srv, &org).await;
    let segment = segment_of(&qr);

    let (s1, a) = scan(&srv, &segment, "Anna", "es").await;
    let (s2, b) = scan(&srv, &segment, "Bruno", "fr").await;
    assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK), "{a} {b}");
    assert_ne!(
        a["session_id"], b["session_id"],
        "two scans, one conversation: the QR must never be one"
    );
    assert_ne!(a["token"], b["token"]);

    // Both are fresh, waiting, owned by the org, under the desk's project, and keep a transcript.
    #[allow(clippy::type_complexity)] // a row of columns, read once to assert on
    let rows: Vec<(String, Option<Uuid>, Option<Uuid>, Option<Uuid>, Option<Vec<u8>>, bool)> = sqlx::query_as(
        "SELECT status, org_id, project_id, owner_user_id, invite_hash, save_transcript FROM live_sessions WHERE desk_id = $1",
    )
    .bind(desk)
    .fetch_all(&srv.pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    for (status, org_id, project_id, owner_user, invite, save) in rows {
        assert_eq!(
            (
                status.as_str(),
                org_id,
                project_id,
                owner_user,
                invite,
                save
            ),
            ("waiting", Some(org.id), Some(org.project), None, None, true)
        );
    }

    // The printed code is unchanged by being scanned.
    let (_, q) = srv
        .get(&format!("{}/{desk}/qr", desks_path(&org)), &org.owner_jwt)
        .await;
    assert_eq!(q["url"].as_str(), Some(qr.as_str()));

    // The scanner is a participant with the token it was given — nothing more.
    let sid = a["session_id"].as_str().unwrap();
    let (s, view) = srv
        .get(
            &format!("/api/live/sessions/{sid}"),
            a["token"].as_str().unwrap(),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{view}");
    assert_eq!(view["session"]["participants"].as_array().unwrap().len(), 1);
    assert_eq!(view["you"]["role"], "guest");
}

#[tokio::test]
async fn a_forged_unknown_revoked_or_replaced_qr_is_refused_identically() {
    let srv = srv!();
    let org = org(&srv).await;
    let (desk, qr) = new_desk(&srv, &org).await;
    let segment = segment_of(&qr);
    let (public_id, mac) = segment.split_once('.').unwrap();

    // Wrong MAC, unknown handle, junk: one answer, so the scan is no oracle for which desks exist.
    let forged = format!("{public_id}.{}", "A".repeat(mac.len()));
    let unknown = format!("{}.{mac}", "z".repeat(public_id.len()));
    let (s, reference) = scan(&srv, &forged, "A", "es").await;
    assert_eq!(
        (s, reference["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("invalid_desk_qr"))
    );
    for bad in [unknown.as_str(), "junk", ""] {
        let (s, b) = scan(&srv, bad, "A", "es").await;
        assert_eq!((s, &b), (StatusCode::NOT_FOUND, &reference), "{bad:?}");
    }
    // The consent gate comes before any lookup.
    let (s, b) = srv
        .post(
            "/api/live/desks/scan",
            None,
            json!({ "segment": segment, "lang": "es", "consent": false }),
        )
        .await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("consent_required"))
    );

    // A customer is waiting when the code is compromised: revoking closes it and kills the QR.
    let (_, waiting) = scan(&srv, &segment, "Anna", "es").await;
    let waiting_sid: Uuid = waiting["session_id"].as_str().unwrap().parse().unwrap();
    let (s, rev) = srv
        .post(
            &format!("{}/{desk}/revoke", desks_path(&org)),
            Some(&org.owner_jwt),
            json!({}),
        )
        .await;
    assert_eq!(
        (s, rev["waiting_closed"].as_u64()),
        (StatusCode::OK, Some(1))
    );
    let status: String = sqlx::query_scalar("SELECT status FROM live_sessions WHERE id = $1")
        .bind(waiting_sid)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert_eq!(status, "expired");
    let (s, b) = scan(&srv, &segment, "A", "es").await;
    assert_eq!(
        (s, &b),
        (StatusCode::NOT_FOUND, &reference),
        "a revoked desk looks like no desk"
    );
    assert_eq!(
        srv.post(
            &format!("{}/{desk}/revoke", desks_path(&org)),
            Some(&org.member_jwt),
            json!({})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );

    // Regenerating prints a NEW code and puts the desk back; the old print stays dead.
    let (s, regen) = srv
        .post(
            &format!("{}/{desk}/regenerate", desks_path(&org)),
            Some(&org.owner_jwt),
            json!({}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{regen}");
    let new_qr = regen["qr_url"].as_str().unwrap().to_string();
    assert_ne!(new_qr, qr);
    let (s, b) = scan(&srv, &segment, "A", "es").await;
    assert_eq!(
        (s, &b),
        (StatusCode::NOT_FOUND, &reference),
        "the old print must stay dead"
    );
    let (s, ok) = scan(&srv, &segment_of(&new_qr), "A", "es").await;
    assert_eq!(s, StatusCode::OK, "{ok}");
}

#[tokio::test]
async fn an_org_that_cannot_pay_is_unavailable_without_saying_why() {
    let srv = srv!();
    let org = org(&srv).await;
    let (_, qr) = new_desk(&srv, &org).await;
    for (sql, what) in [
        ("UPDATE organizations SET credits_balance = 0 WHERE id = $1", "no credit"),
        ("UPDATE organizations SET credits_balance = 1000, subscription_status = 'canceled' WHERE id = $1", "no subscription"),
    ] {
        sqlx::query(sql).bind(org.id).execute(&srv.pool).await.unwrap();
        let (s, b) = scan(&srv, &segment_of(&qr), "A", "es").await;
        assert_eq!((s, b["code"].as_str()), (StatusCode::SERVICE_UNAVAILABLE, Some("desk_unavailable")), "{what}");
        let shown = b.to_string().to_lowercase();
        assert!(!shown.contains("credit") && !shown.contains("subscri"), "{what}: a stranger learned about the wallet: {shown}");
    }
}

#[tokio::test]
async fn the_waiting_queue_is_bounded() {
    let srv = srv!(Some("2"));
    let org = org(&srv).await;
    let (_, qr) = new_desk(&srv, &org).await;
    let segment = segment_of(&qr);
    assert_eq!(scan(&srv, &segment, "A", "es").await.0, StatusCode::OK);
    assert_eq!(scan(&srv, &segment, "B", "es").await.0, StatusCode::OK);
    let (s, b) = scan(&srv, &segment, "C", "es").await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::TOO_MANY_REQUESTS, Some("desk_busy"))
    );
}

// ---- staff -----------------------------------------------------------------------------

#[tokio::test]
async fn staff_see_who_is_waiting_and_exactly_one_colleague_answers() {
    let srv = srv!();
    let org = org(&srv).await;
    let (desk, qr) = new_desk(&srv, &org).await;
    let (stranger_jwt, _) = login(&srv, "Stranger").await;
    let (_, guest) = scan(&srv, &segment_of(&qr), "Anna", "es").await;
    let sid = guest["session_id"].as_str().unwrap().to_string();

    // Only this org's members see the queue; an outsider cannot even tell the desk exists.
    let queue = format!("/api/live/desks/{desk}/waiting");
    let (s, q) = srv.get(&queue, &org.member_jwt).await;
    assert_eq!(s, StatusCode::OK, "{q}");
    assert_eq!(q["waiting"][0]["guest_name"], "Anna");
    assert_eq!(q["waiting"][0]["guest_lang"], "es");
    assert_eq!(
        srv.get(&queue, &stranger_jwt).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        srv.get(
            &format!("/api/live/desks/{}/waiting", Uuid::new_v4()),
            &org.member_jwt
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );

    // An outsider cannot answer, and a session that is not a desk session is simply not found.
    let answer = format!("/api/live/sessions/{sid}/answer");
    assert_eq!(
        srv.post(&answer, Some(&stranger_jwt), json!({ "lang": "it" }))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let (_, b2c) = srv
        .post(
            "/api/live/sessions",
            Some(&org.member_jwt),
            json!({ "lang": "it" }),
        )
        .await;
    let b2c_sid = b2c["session_id"].as_str().unwrap();
    assert_eq!(
        srv.post(
            &format!("/api/live/sessions/{b2c_sid}/answer"),
            Some(&org.member_jwt),
            json!({ "lang": "it" })
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );

    // Two colleagues tap Answer at the same instant: one wins, one is told a colleague did.
    let (a, b) = tokio::join!(
        srv.post(&answer, Some(&org.owner_jwt), json!({ "lang": "it" })),
        srv.post(&answer, Some(&org.member_jwt), json!({ "lang": "de" }))
    );
    let mut codes = [a.0, b.0];
    codes.sort();
    assert_eq!(codes, [StatusCode::OK, StatusCode::CONFLICT], "{a:?} {b:?}");
    let (winner, loser) = if a.0 == StatusCode::OK {
        (&a.1, &b.1)
    } else {
        (&b.1, &a.1)
    };
    assert_eq!(loser["code"], "already_answered");
    assert_eq!(winner["guest"]["name"], "Anna");

    // The session is active, the staff seat is taken, and the queue is empty.
    let (s, view) = srv
        .get(
            &format!("/api/live/sessions/{sid}"),
            winner["token"].as_str().unwrap(),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{view}");
    assert_eq!(view["session"]["status"], "active");
    assert_eq!(view["you"]["role"], "staff");
    assert_eq!(view["session"]["participants"].as_array().unwrap().len(), 2);
    let (_, q) = srv.get(&queue, &org.member_jwt).await;
    assert_eq!(q["waiting"].as_array().unwrap().len(), 0);
    // A late answer to a conversation that is already underway is refused, not duplicated.
    let (s, b) = srv
        .post(&answer, Some(&org.member_jwt), json!({ "lang": "it" }))
        .await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::CONFLICT, Some("already_answered"))
    );
}

#[tokio::test]
async fn a_customer_who_stopped_waiting_cannot_be_answered() {
    let srv = srv!();
    let org = org(&srv).await;
    let (desk, qr) = new_desk(&srv, &org).await;
    let (_, guest) = scan(&srv, &segment_of(&qr), "Anna", "es").await;
    let sid = guest["session_id"].as_str().unwrap();
    sqlx::query(
        "UPDATE live_sessions SET expires_at = now() - interval '1 second' WHERE id = $1::uuid",
    )
    .bind(sid)
    .execute(&srv.pool)
    .await
    .unwrap();

    // Gone from the queue even before the sweeper runs, and not answerable.
    let (_, q) = srv
        .get(&format!("/api/live/desks/{desk}/waiting"), &org.member_jwt)
        .await;
    assert_eq!(q["waiting"].as_array().unwrap().len(), 0);
    let (s, b) = srv
        .post(
            &format!("/api/live/sessions/{sid}/answer"),
            Some(&org.member_jwt),
            json!({ "lang": "it" }),
        )
        .await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::GONE, Some("session_expired"))
    );
}

#[tokio::test]
async fn my_desks_lists_only_my_orgs_active_desks_with_how_many_are_waiting() {
    let srv = srv!();
    let org = org(&srv).await;
    let (desk, qr) = new_desk(&srv, &org).await;
    let (revoked, _) = new_desk(&srv, &org).await;
    srv.post(
        &format!("{}/{revoked}/revoke", desks_path(&org)),
        Some(&org.owner_jwt),
        json!({}),
    )
    .await;
    let other = self::org(&srv).await;
    new_desk(&srv, &other).await;
    scan(&srv, &segment_of(&qr), "Anna", "es").await;

    let (s, mine) = srv.get("/api/live/my-desks", &org.member_jwt).await;
    assert_eq!(s, StatusCode::OK, "{mine}");
    let desks = mine["desks"].as_array().unwrap();
    assert_eq!(
        desks.len(),
        1,
        "other orgs' desks and revoked desks are not mine: {mine}"
    );
    assert_eq!(desks[0]["id"], json!(desk));
    assert_eq!(desks[0]["waiting"], 1);
    assert_eq!(desks[0]["org_name"], "Hotel");
}

#[tokio::test]
async fn a_desk_conversation_runs_under_the_orgs_project() {
    let srv = srv!();
    let org = org(&srv).await;
    let (_, qr) = new_desk(&srv, &org).await;
    let (_, guest) = scan(&srv, &segment_of(&qr), "Anna", "es").await;
    let sid: Uuid = guest["session_id"].as_str().unwrap().parse().unwrap();
    let (s, staff) = srv
        .post(
            &format!("/api/live/sessions/{sid}/answer"),
            Some(&org.member_jwt),
            json!({ "lang": "it" }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{staff}");

    // Both phones open their sockets; the room is made, and the meeting-side row carries the org
    // and the desk's project so the conversation lands in that project's history and search.
    let mut sockets = Vec::new();
    for token in [
        guest["token"].as_str().unwrap(),
        staff["token"].as_str().unwrap(),
    ] {
        let (mut ws, _) =
            tokio_tungstenite::connect_async(format!("ws://{}/ws/live?token={token}", srv.addr))
                .await
                .expect("upgrade");
        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(5), ws.next()).await {
                Ok(Some(Ok(Message::Text(t)))) if t.contains("room_joined") => break,
                Ok(Some(Ok(_))) => continue,
                other => panic!("no room_joined: {other:?}"),
            }
        }
        sockets.push(ws);
    }
    let (kind, row_org, row_project): (String, Option<Uuid>, Option<Uuid>) = sqlx::query_as(
        "SELECT cs.kind, cs.org_id, cs.project_id FROM live_sessions ls
         JOIN call_sessions cs ON cs.id = ls.call_session_id WHERE ls.id = $1",
    )
    .bind(sid)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(
        (kind.as_str(), row_org, row_project),
        ("live_connect", Some(org.id), Some(org.project))
    );
    let staff_lang: String =
        sqlx::query_scalar("SELECT host_lang FROM live_sessions WHERE id = $1")
            .bind(sid)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(
        staff_lang, "it",
        "the owner side speaks the language the staff member chose"
    );
    let _ = org.member;
}
