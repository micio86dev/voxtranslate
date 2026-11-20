//! Platform-operator administration of Live Connect (spec 0121, S15): `/api/admin/live/*`.
//!
//! What these pin down: the shared secret gates every route before the body is read; each
//! action leaves an `admin_audit` row; regenerating a desk never hands the operator the new QR;
//! the transcript is only readable with a real reason and an audit row that exists FIRST;
//! reconciliation reports `ok`, `mismatch` and `unlinked` honestly and changes nothing.
//!
//! DB-gated like the other integration suites. Run against the local pgvector Postgres:
//! `DATABASE_URL=postgres://vox:vox_local_dev@127.0.0.1:55432/voxtranslate_voip_test \
//!   cargo test --test live_admin`. A run of a few milliseconds skipped everything.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use reqwest::StatusCode;
use rust_decimal::Decimal;
use serde_json::{json, Value};
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::Config;
use voxtranslate_server::db::{self, Pool};
use voxtranslate_server::live::billing::{charge_tick, Charge, LivePayer, Tick};
use voxtranslate_server::live::config::LiveConfig;
use voxtranslate_server::live::ws::room_name;
use voxtranslate_server::live::LiveState;
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::transcripts::TranscriptService;
use voxtranslate_server::{app, AppState};

const SECRET: &str = "live-admin-jwt-secret";
const ADMIN: &str = "test-admin-secret"; // matches Config::test_with_billing
const LIVE_KEY: &str = "0123456789abcdef0123456789abcdef-live-admin";
const OPERATOR: &str = "operator@voxtranslate.test";
const GUEST_NAME: &str = "Guest Gustavo";
const STAFF_NAME: &str = "Staff Sabrina";
const SAID: &str = "il mio codice di prenotazione e quattro sette";
const REASON: &str = "customer asked for a copy of what was said";

struct Srv {
    addr: SocketAddr,
    pool: Pool,
    http: reqwest::Client,
}

async fn setup() -> Option<Srv> {
    let url = db::test_database_url()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let config = Config::test_with_billing(&url, SECRET, 5.0);
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
            "LIVE_FINALIZE_GRACE_SECS" => Some("0".into()),
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
    })
}

macro_rules! srv {
    () => {
        match setup().await {
            Some(s) => s,
            None => {
                eprintln!("skipping — no DATABASE_URL");
                return;
            }
        }
    };
}

impl Srv {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// POST with an optional admin secret header; returns the status and the raw body text.
    async fn raw(&self, path: &str, secret: Option<&str>, body: Value) -> (StatusCode, String) {
        let mut req = self.http.post(self.url(path)).json(&body);
        if let Some(s) = secret {
            req = req.header("X-Admin-Secret", s);
        }
        let res = req.send().await.unwrap();
        (res.status(), res.text().await.unwrap())
    }

    /// POST as the operator (valid secret). Returns status, parsed JSON and the raw text.
    async fn admin(&self, path: &str, body: Value) -> (StatusCode, Value, String) {
        let (s, text) = self.raw(path, Some(ADMIN), body).await;
        let v = serde_json::from_str(&text).unwrap_or(Value::Null);
        (s, v, text)
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        bearer: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = match method {
            "GET" => self.http.get(self.url(path)),
            _ => self.http.post(self.url(path)),
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
}

// ---- fixtures --------------------------------------------------------------------------

async fn login(srv: &Srv, name: &str) -> (String, Uuid) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: name.into(),
        avatar_url: None,
    };
    let (user, _) = upsert_google_user(&srv.pool, &identity, Decimal::new(500, 2), None, None)
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
    member: Uuid,
}

async fn org(srv: &Srv) -> Org {
    let (owner_jwt, owner) = login(srv, "Owner").await;
    let (_, member) = login(srv, STAFF_NAME).await;
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
        member,
    }
}

/// Create a desk as the org owner. Returns `(desk id, qr url, public id)`.
async fn new_desk(srv: &Srv, org: &Org) -> (Uuid, String, String) {
    let (s, desk) = srv
        .call(
            "POST",
            &format!("/api/business/organizations/{}/live/desks", org.id),
            Some(&org.owner_jwt),
            Some(
                json!({ "name": "Front desk", "location_label": "Lobby", "default_lang": "it", "project_id": org.project }),
            ),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{desk}");
    (
        desk["id"].as_str().unwrap().parse().unwrap(),
        desk["qr_url"].as_str().unwrap().to_string(),
        desk["public_id"].as_str().unwrap().to_string(),
    )
}

fn segment_of(qr_url: &str) -> String {
    qr_url.rsplit('/').next().unwrap().to_string()
}

/// A scan: a customer opens the desk's QR, which creates a new waiting session.
async fn scan(srv: &Srv, segment: &str) -> (StatusCode, Value) {
    srv.call(
        "POST",
        "/api/live/desks/scan",
        None,
        Some(json!({ "segment": segment, "name": "Walk-in", "lang": "es", "consent": true })),
    )
    .await
}

struct Conv {
    live: Uuid,
    call: Uuid,
    staff_peer: Uuid,
}

enum Owner<'a> {
    Org(&'a Org),
    User(Uuid),
}

/// A conversation as the socket leaves it: the live row, two participants, one call session
/// (named by a `room_session` event) with one utterance said and translated.
async fn seed(srv: &Srv, owner: Owner<'_>, status: &str) -> Conv {
    let pool = &srv.pool;
    let (org_id, project_id, owner_user, staff_user) = match &owner {
        Owner::Org(o) => (Some(o.id), Some(o.project), None, o.member),
        Owner::User(u) => (None, None, Some(*u), *u),
    };
    let live: Uuid = sqlx::query_scalar(
        "INSERT INTO live_sessions
             (org_id, project_id, owner_user_id, status, host_lang, save_transcript, expires_at,
              created_at, started_at, ended_at, end_reason)
         VALUES ($1, $2, $3, $4, 'it', TRUE, now() + interval '1 hour',
                 now() - interval '10 minutes', now() - interval '5 minutes',
                 CASE WHEN $4 IN ('ended', 'expired') THEN now() END,
                 CASE WHEN $4 IN ('ended', 'expired') THEN 'ended_by_staff' END)
         RETURNING id",
    )
    .bind(org_id)
    .bind(project_id)
    .bind(owner_user)
    .bind(status)
    .fetch_one(pool)
    .await
    .unwrap();
    let role = if org_id.is_some() { "staff" } else { "host" };
    let staff_peer: Uuid = sqlx::query_scalar(
        "INSERT INTO live_participants (session_id, role, user_id, display_name, lang, token_hash, consent_at)
         VALUES ($1, $2, $3, $4, 'it', '\\xdeadbeef', now()) RETURNING peer_id",
    )
    .bind(live)
    .bind(role)
    .bind(staff_user)
    .bind(STAFF_NAME)
    .fetch_one(pool)
    .await
    .unwrap();
    let guest_peer: Uuid = sqlx::query_scalar(
        "INSERT INTO live_participants (session_id, role, display_name, lang, token_hash, consent_at)
         VALUES ($1, 'guest', $2, 'es', '\\xcafebabe', now()) RETURNING peer_id",
    )
    .bind(live)
    .bind(GUEST_NAME)
    .fetch_one(pool)
    .await
    .unwrap();
    let call = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO call_sessions (id, room, kind, org_id, project_id, started_at)
         VALUES ($1, $2, 'live_connect', $3, $4, now() - interval '5 minutes')",
    )
    .bind(call)
    .bind(room_name(live))
    .bind(org_id)
    .bind(project_id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO transcript_events
             (session_id, event_type, speaker_peer_id, speaker_name, original_text, original_lang, translations, ts)
         VALUES ($1, 'speech', $2, $3, $4, 'it', $5, now() - interval '4 minutes')",
    )
    .bind(call)
    .bind(guest_peer.to_string())
    .bind(GUEST_NAME)
    .bind(SAID)
    .bind(json!({ "es": "mi codigo de reserva es cuatro siete" }))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO live_session_events (session_id, kind, detail) VALUES ($1, 'room_session', $2)",
    )
    .bind(live)
    .bind(json!({ "call_session_id": call }))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("UPDATE live_sessions SET call_session_id = $2 WHERE id = $1")
        .bind(live)
        .bind(call)
        .execute(pool)
        .await
        .unwrap();
    Conv {
        live,
        call,
        staff_peer,
    }
}

/// Charge `n` org ticks through the real billing path (tick + ledger row in one transaction).
async fn charge_org(srv: &Srv, org: &Org, conv: &Conv, n: i64) {
    for seq in 1..=n {
        let r = charge_tick(
            &srv.pool,
            &LivePayer::Org {
                org_id: org.id,
                call_session: conv.call,
            },
            conv.live,
            conv.staff_peer,
            seq,
            Charge {
                seconds: 5,
                usd: Decimal::ZERO,
                credits: 2,
            },
        )
        .await
        .unwrap();
        assert_eq!(r, Tick::Charged { balance: None });
    }
}

async fn audit_rows(srv: &Srv, action: &str, target: Uuid) -> Vec<(String, Value)> {
    sqlx::query_as(
        "SELECT actor, detail FROM admin_audit WHERE action = $1 AND target = $2 ORDER BY created_at",
    )
    .bind(action)
    .bind(target.to_string())
    .fetch_all(&srv.pool)
    .await
    .unwrap()
}

async fn scalar_i64(pool: &Pool, sql: &str, id: Uuid) -> i64 {
    sqlx::query_scalar(sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn code(v: &Value) -> Option<&str> {
    v["code"].as_str()
}

// ---- the shared secret -----------------------------------------------------------------

#[tokio::test]
async fn every_route_refuses_a_missing_or_wrong_secret_before_reading_the_body() {
    let srv = srv!();
    let org = org(&srv).await;
    let (desk, _, _) = new_desk(&srv, &org).await;
    let conv = seed(&srv, Owner::Org(&org), "active").await;

    let routes = [
        (
            "/api/admin/live/desk-revoke",
            json!({ "desk_id": desk, "actor": OPERATOR, "reason": REASON }),
        ),
        (
            "/api/admin/live/desk-regenerate",
            json!({ "desk_id": desk, "actor": OPERATOR, "reason": REASON }),
        ),
        (
            "/api/admin/live/session-end",
            json!({ "session_id": conv.live, "actor": OPERATOR, "reason": REASON }),
        ),
        (
            "/api/admin/live/session-reconcile",
            json!({ "session_id": conv.live }),
        ),
        (
            "/api/admin/live/session-transcript",
            json!({ "session_id": conv.live, "actor": OPERATOR, "reason": REASON }),
        ),
    ];
    for (path, body) in routes {
        for secret in [None, Some("wrong-secret"), Some("")] {
            let (s, text) = srv.raw(path, secret, body.clone()).await;
            assert_eq!(s, StatusCode::FORBIDDEN, "{path} with {secret:?}: {text}");
            // Same refusal as the rest of /api/admin: nothing about the target leaks.
            assert!(!text.contains(&conv.live.to_string()), "{path}: {text}");
        }
        // A body that would be a 4xx must still be a 403 for a caller without the secret.
        let (s, _) = srv
            .raw(path, Some("wrong-secret"), json!({ "nonsense": true }))
            .await;
        assert_eq!(
            s,
            StatusCode::FORBIDDEN,
            "{path} parsed the body before the secret"
        );
    }
    // Bearer is accepted exactly like X-Admin-Secret on the other admin routes.
    let res = srv
        .http
        .post(srv.url("/api/admin/live/session-reconcile"))
        .bearer_auth(ADMIN)
        .json(&json!({ "session_id": conv.live }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // Nothing moved: the refusals did not touch the desk or the session.
    let desk_status: String = sqlx::query_scalar("SELECT status FROM live_desks WHERE id = $1")
        .bind(desk)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    let session_status: String =
        sqlx::query_scalar("SELECT status FROM live_sessions WHERE id = $1")
            .bind(conv.live)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(
        (desk_status.as_str(), session_status.as_str()),
        ("active", "active")
    );
}

// ---- desks -----------------------------------------------------------------------------

#[tokio::test]
async fn revoking_a_desk_switches_the_qr_off_closes_the_queue_and_is_audited() {
    let srv = srv!();
    let org = org(&srv).await;
    let (desk, qr, _) = new_desk(&srv, &org).await;
    let segment = segment_of(&qr);
    let (s, scanned) = scan(&srv, &segment).await;
    assert_eq!(s, StatusCode::OK, "{scanned}");

    // Validation first: no actor, no reason, a thin reason, an unknown desk.
    let path = "/api/admin/live/desk-revoke";
    let (s, v, _) = srv
        .admin(path, json!({ "desk_id": desk, "reason": REASON }))
        .await;
    assert_eq!(
        (s, code(&v)),
        (StatusCode::BAD_REQUEST, Some("actor_required"))
    );
    let (s, v, _) = srv
        .admin(path, json!({ "desk_id": desk, "actor": OPERATOR }))
        .await;
    assert_eq!(
        (s, code(&v)),
        (StatusCode::BAD_REQUEST, Some("reason_required"))
    );
    let (s, v, _) = srv
        .admin(
            path,
            json!({ "desk_id": desk, "actor": OPERATOR, "reason": "x" }),
        )
        .await;
    assert_eq!(
        (s, code(&v)),
        (StatusCode::BAD_REQUEST, Some("reason_too_short"))
    );
    let (s, v, _) = srv
        .admin(
            path,
            json!({ "desk_id": Uuid::new_v4(), "actor": OPERATOR, "reason": REASON }),
        )
        .await;
    assert_eq!(
        (s, code(&v)),
        (StatusCode::NOT_FOUND, Some("desk_not_found"))
    );
    let still: String = sqlx::query_scalar("SELECT status FROM live_desks WHERE id = $1")
        .bind(desk)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert_eq!(still, "active", "a refused request must not revoke");
    assert!(audit_rows(&srv, "live_desk_revoke", desk).await.is_empty());

    let (s, v, _) = srv
        .admin(
            path,
            json!({ "desk_id": desk, "actor": OPERATOR, "reason": REASON }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        (v["status"].as_str(), v["waiting_closed"].as_i64()),
        (Some("revoked"), Some(1))
    );

    let (status, revoked_at): (String, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT status, revoked_at FROM live_desks WHERE id = $1")
            .bind(desk)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(status, "revoked");
    assert!(revoked_at.is_some());
    // The waiting customer was closed with the reason the org's own revoke uses.
    let reasons: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT status, end_reason FROM live_sessions WHERE desk_id = $1")
            .bind(desk)
            .fetch_all(&srv.pool)
            .await
            .unwrap();
    assert_eq!(
        reasons,
        vec![("expired".into(), Some("desk_revoked".into()))]
    );
    // The printed code no longer opens a conversation.
    let (s, again) = scan(&srv, &segment).await;
    assert_eq!(
        (s, code(&again)),
        (StatusCode::NOT_FOUND, Some("invalid_desk_qr"))
    );

    let audit = audit_rows(&srv, "live_desk_revoke", desk).await;
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].0, OPERATOR);
    assert_eq!(audit[0].1["reason"], REASON);
    assert_eq!(audit[0].1["org_id"], json!(org.id));
    assert_eq!(audit[0].1["waiting_closed"], 1);
    assert!(
        audit[0].1.get("actor_ip").is_some(),
        "the caller's IP is recorded"
    );

    // Revoking twice is harmless, like the org admin's.
    let (s, v, _) = srv
        .admin(
            path,
            json!({ "desk_id": desk, "actor": OPERATOR, "reason": REASON }),
        )
        .await;
    assert_eq!((s, v["waiting_closed"].as_i64()), (StatusCode::OK, Some(0)));
}

#[tokio::test]
async fn regenerating_a_desk_never_hands_the_operator_the_new_qr() {
    let srv = srv!();
    let org = org(&srv).await;
    let (desk, old_qr, public_id) = new_desk(&srv, &org).await;
    let old_segment = segment_of(&old_qr);
    let (s, _) = scan(&srv, &old_segment).await;
    assert_eq!(s, StatusCode::OK);
    // Revoke first: regenerating also puts the desk back in service.
    let (s, _, _) = srv
        .admin(
            "/api/admin/live/desk-revoke",
            json!({ "desk_id": desk, "actor": OPERATOR, "reason": REASON }),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = scan(&srv, &old_segment).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let path = "/api/admin/live/desk-regenerate";
    let (s, v, _) = srv
        .admin(
            path,
            json!({ "desk_id": Uuid::new_v4(), "actor": OPERATOR, "reason": REASON }),
        )
        .await;
    assert_eq!(
        (s, code(&v)),
        (StatusCode::NOT_FOUND, Some("desk_not_found"))
    );
    let (s, v, _) = srv
        .admin(path, json!({ "desk_id": desk, "actor": OPERATOR }))
        .await;
    assert_eq!(
        (s, code(&v)),
        (StatusCode::BAD_REQUEST, Some("reason_required"))
    );

    let (s, v, text) = srv
        .admin(
            path,
            json!({ "desk_id": desk, "actor": OPERATOR, "reason": REASON }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{text}");
    assert_eq!(v["status"], "active");
    assert_eq!(v["token_version"], 2);
    assert_eq!(v["waiting_closed"], 0);

    // The credential is nowhere in what the operator receives: not the URL, not the MAC, not
    // even the public handle the URL is built from.
    let new_qr: String = {
        let (s, qr) = srv
            .call(
                "GET",
                &format!(
                    "/api/business/organizations/{}/live/desks/{desk}/qr",
                    org.id
                ),
                Some(&org.owner_jwt),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{qr}");
        qr["url"].as_str().unwrap().to_string()
    };
    let new_segment = segment_of(&new_qr);
    let mac = new_segment.rsplit('.').next().unwrap();
    assert!(
        mac.len() >= 8,
        "test must hold a real MAC to look for: {new_segment}"
    );
    for secret in [
        "/lc/d/",
        mac,
        new_segment.as_str(),
        new_qr.as_str(),
        public_id.as_str(),
        "qr_url",
        "mac",
    ] {
        assert!(
            !text.contains(secret),
            "the admin response leaked `{secret}`: {text}"
        );
    }
    assert_eq!(
        v.as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        ["status", "token_version", "waiting_closed"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        "the response is exactly the version and the count"
    );

    // And it did what an org admin's regenerate does: the old print is dead, the new one works.
    let (s, _) = scan(&srv, &old_segment).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "the old QR must stop working");
    let (s, body) = scan(&srv, &new_segment).await;
    assert_eq!(s, StatusCode::OK, "{body}");

    let audit = audit_rows(&srv, "live_desk_regenerate", desk).await;
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].0, OPERATOR);
    assert_eq!(audit[0].1["reason"], REASON);
    assert_eq!(audit[0].1["token_version"], 2);
    let stored = audit[0].1.to_string();
    assert!(
        !stored.contains(mac) && !stored.contains("/lc/d/"),
        "audit holds no credential: {stored}"
    );
}

// ---- ending a session ------------------------------------------------------------------

#[tokio::test]
async fn an_operator_can_end_an_open_session_once_and_it_is_finalized() {
    let srv = srv!();
    let org = org(&srv).await;
    let conv = seed(&srv, Owner::Org(&org), "active").await;
    let path = "/api/admin/live/session-end";

    let (s, v, _) = srv
        .admin(
            path,
            json!({ "session_id": Uuid::new_v4(), "actor": OPERATOR, "reason": REASON }),
        )
        .await;
    assert_eq!(
        (s, code(&v)),
        (StatusCode::NOT_FOUND, Some("session_not_found"))
    );
    let (s, v, _) = srv
        .admin(
            path,
            json!({ "session_id": conv.live, "actor": "  ", "reason": REASON }),
        )
        .await;
    assert_eq!(
        (s, code(&v)),
        (StatusCode::BAD_REQUEST, Some("actor_required"))
    );
    let (s, v, _) = srv
        .admin(path, json!({ "session_id": conv.live, "actor": OPERATOR }))
        .await;
    assert_eq!(
        (s, code(&v)),
        (StatusCode::BAD_REQUEST, Some("reason_required"))
    );
    let open: String = sqlx::query_scalar("SELECT status FROM live_sessions WHERE id = $1")
        .bind(conv.live)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert_eq!(open, "active", "a refused request must not end the session");

    let (s, v, _) = srv
        .admin(
            path,
            json!({ "session_id": conv.live, "actor": OPERATOR, "reason": REASON }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["end_reason"], "ended_by_admin");

    let (status, end_reason, ended_at): (
        String,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as("SELECT status, end_reason, ended_at FROM live_sessions WHERE id = $1")
        .bind(conv.live)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert_eq!(
        (status.as_str(), end_reason.as_deref()),
        ("ended", Some("ended_by_admin"))
    );
    assert!(ended_at.is_some());

    // One timeline event, with no personal data in it.
    let events: Vec<Value> = sqlx::query_scalar(
        "SELECT detail FROM live_session_events WHERE session_id = $1 AND kind = 'admin_ended'",
    )
    .bind(conv.live)
    .fetch_all(&srv.pool)
    .await
    .unwrap();
    assert_eq!(events, vec![json!({})]);

    // Finalization ran (grace 0): the org conversation became a transcript.
    let mut transcripts = 0;
    for _ in 0..40 {
        transcripts = scalar_i64(
            &srv.pool,
            "SELECT count(*) FROM transcripts WHERE session_id = $1",
            conv.call,
        )
        .await;
        if transcripts == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(transcripts, 1, "ending a session must trigger finalization");

    let audit = audit_rows(&srv, "live_session_end", conv.live).await;
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].0, OPERATOR);
    assert_eq!(audit[0].1["reason"], REASON);
    assert_eq!(audit[0].1["was"], "active");
    let stored = audit[0].1.to_string();
    assert!(
        !stored.contains(SAID) && !stored.contains(GUEST_NAME),
        "{stored}"
    );

    // It cannot be ended twice, and the second attempt leaves no second trace.
    let (s, v, _) = srv
        .admin(
            path,
            json!({ "session_id": conv.live, "actor": OPERATOR, "reason": REASON }),
        )
        .await;
    assert_eq!((s, code(&v)), (StatusCode::CONFLICT, Some("session_over")));
    assert_eq!(
        audit_rows(&srv, "live_session_end", conv.live).await.len(),
        1
    );

    // An expired session is over too; a waiting one is still open and can be ended.
    let expired = seed(&srv, Owner::Org(&org), "expired").await;
    let (s, v, _) = srv
        .admin(
            path,
            json!({ "session_id": expired.live, "actor": OPERATOR, "reason": REASON }),
        )
        .await;
    assert_eq!((s, code(&v)), (StatusCode::CONFLICT, Some("session_over")));
    let waiting = seed(&srv, Owner::Org(&org), "waiting").await;
    let (s, v, _) = srv
        .admin(
            path,
            json!({ "session_id": waiting.live, "actor": OPERATOR, "reason": REASON }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

// ---- reconciliation --------------------------------------------------------------------

#[tokio::test]
async fn reconcile_reports_ok_then_mismatch_when_a_debit_disappears_and_changes_nothing() {
    let srv = srv!();
    let org = org(&srv).await;
    let conv = seed(&srv, Owner::Org(&org), "ended").await;
    charge_org(&srv, &org, &conv, 3).await;
    let path = "/api/admin/live/session-reconcile";

    let (s, v, _) = srv
        .admin(path, json!({ "session_id": Uuid::new_v4() }))
        .await;
    assert_eq!(
        (s, code(&v)),
        (StatusCode::NOT_FOUND, Some("session_not_found"))
    );

    let before: (i32, i64, i64) = sqlx::query_as(
        "SELECT o.credits_balance,
                (SELECT count(*) FROM live_usage_ticks WHERE session_id = $2),
                (SELECT count(*) FROM organization_credits_transactions WHERE org_id = $1)
         FROM organizations o WHERE o.id = $1",
    )
    .bind(org.id)
    .bind(conv.live)
    .fetch_one(&srv.pool)
    .await
    .unwrap();

    let (s, v, _) = srv
        .admin(path, json!({ "session_id": conv.live, "actor": OPERATOR }))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "ok", "{v}");
    assert_eq!(v["payer"], "org");
    assert_eq!(v["ticks"]["count"], 3);
    assert_eq!(v["ticks"]["org_credits"], "6");
    assert_eq!(v["ledger"]["org_entries"], 3);
    assert_eq!(v["ledger"]["org_credits"], "6");
    assert_eq!(
        (v["missing_ticks"].as_i64(), v["duplicate_ticks"].as_i64()),
        (Some(0), Some(0))
    );

    // A tick whose debit is gone: the books no longer explain the metering.
    sqlx::query(
        "DELETE FROM organization_credits_transactions WHERE id = (
             SELECT id FROM organization_credits_transactions
             WHERE org_id = $1 AND type = 'live_connect' AND session_id = $2 LIMIT 1)",
    )
    .bind(org.id)
    .bind(conv.call)
    .execute(&srv.pool)
    .await
    .unwrap();
    let (s, v, _) = srv.admin(path, json!({ "session_id": conv.live })).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["status"], "mismatch", "{v}");
    assert_eq!(
        (v["missing_ticks"].as_i64(), v["duplicate_ticks"].as_i64()),
        (Some(1), Some(0))
    );
    assert_eq!(v["ledger"]["org_entries"], 2);
    assert_eq!(v["ledger"]["org_credits"], "4");

    // A debit no tick accounts for (a charge the idempotency ledger never saw).
    for _ in 0..2 {
        sqlx::query(
            "INSERT INTO organization_credits_transactions (org_id, amount, type, description, session_id)
             VALUES ($1, -2, 'live_connect', 'stray', $2)",
        )
        .bind(org.id)
        .bind(conv.call)
        .execute(&srv.pool)
        .await
        .unwrap();
    }
    let (_, v, _) = srv.admin(path, json!({ "session_id": conv.live })).await;
    assert_eq!(v["status"], "mismatch", "{v}");
    assert_eq!(
        (v["missing_ticks"].as_i64(), v["duplicate_ticks"].as_i64()),
        (Some(0), Some(1))
    );

    // Reconciling is a read: neither the wallet, nor the ticks, nor the ledger moved.
    let after: (i32, i64, i64) = sqlx::query_as(
        "SELECT o.credits_balance,
                (SELECT count(*) FROM live_usage_ticks WHERE session_id = $2),
                (SELECT count(*) FROM organization_credits_transactions WHERE org_id = $1)
         FROM organizations o WHERE o.id = $1",
    )
    .bind(org.id)
    .bind(conv.live)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    // Only the rows this test itself removed (1) and added (2) differ in the ledger count.
    assert_eq!(
        (after.0, after.1, after.2),
        (before.0, before.1, before.2 - 1 + 2)
    );
    // Each look is on record.
    assert!(!audit_rows(&srv, "live_session_reconcile", conv.live)
        .await
        .is_empty());
}

#[tokio::test]
async fn reconcile_says_unlinked_once_erasure_cut_the_ledger_link() {
    let srv = srv!();
    let org = org(&srv).await;
    let conv = seed(&srv, Owner::Org(&org), "ended").await;
    charge_org(&srv, &org, &conv, 2).await;
    let path = "/api/admin/live/session-reconcile";
    let (_, v, _) = srv.admin(path, json!({ "session_id": conv.live })).await;
    assert_eq!(v["status"], "ok", "{v}");

    // Erasure deletes the meeting-side rows; the org's debits survive with session_id NULL.
    voxtranslate_server::live::erase::erase_content(&srv.pool, conv.live)
        .await
        .unwrap();
    let orphaned = scalar_i64(
        &srv.pool,
        "SELECT count(*) FROM organization_credits_transactions
         WHERE type = 'live_connect' AND session_id IS NULL AND org_id = (SELECT org_id FROM live_sessions WHERE id = $1)",
        conv.live,
    )
    .await;
    assert_eq!(orphaned, 2, "the debits outlive the content, unlinked");

    let (s, v, _) = srv.admin(path, json!({ "session_id": conv.live })).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["status"], "unlinked", "{v}");
    assert_eq!(v["ticks"]["count"], 2, "the metering side is still known");
    // Nothing is claimed about the debits it can no longer find.
    assert!(
        v["missing_ticks"].is_null() && v["duplicate_ticks"].is_null(),
        "{v}"
    );
    assert!(v["notes"].as_array().is_some_and(|n| !n.is_empty()));
}

#[tokio::test]
async fn reconcile_compares_a_consumer_conversation_with_its_usage_session() {
    let srv = srv!();
    let (_, user) = login(&srv, "Consumer").await;
    let conv = seed(&srv, Owner::User(user), "ended").await;
    let billing = BillingService::new(srv.pool.clone(), usd(0.05));
    let usage_session = billing
        .create_session(user, &room_name(conv.live), "standard")
        .await
        .unwrap();
    for seq in 1..=2i64 {
        let r = charge_tick(
            &srv.pool,
            &LivePayer::User {
                user_id: user,
                usage_session,
            },
            conv.live,
            conv.staff_peer,
            seq,
            Charge {
                seconds: 5,
                usd: usd(0.03),
                credits: 0,
            },
        )
        .await
        .unwrap();
        assert!(matches!(r, Tick::Charged { .. }));
    }
    let path = "/api/admin/live/session-reconcile";
    let (s, v, _) = srv.admin(path, json!({ "session_id": conv.live })).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["payer"], "user");
    assert_eq!(v["status"], "ok", "{v}");
    assert_eq!(v["ticks"]["count"], 2);
    // Single debits cannot be counted for a consumer; the report says so instead of inventing.
    assert!(v["missing_ticks"].is_null() && v["duplicate_ticks"].is_null());

    sqlx::query("UPDATE usage_sessions SET cost = cost + 0.5 WHERE id = $1")
        .bind(usage_session)
        .execute(&srv.pool)
        .await
        .unwrap();
    let (_, v, _) = srv.admin(path, json!({ "session_id": conv.live })).await;
    assert_eq!(v["status"], "mismatch", "{v}");

    sqlx::query("DELETE FROM usage_sessions WHERE id = $1")
        .bind(usage_session)
        .execute(&srv.pool)
        .await
        .unwrap();
    let (_, v, _) = srv.admin(path, json!({ "session_id": conv.live })).await;
    assert_eq!(v["status"], "unlinked", "{v}");
}

// ---- the privileged read ---------------------------------------------------------------

#[tokio::test]
async fn the_transcript_needs_a_real_reason_and_is_audited_without_its_content() {
    let srv = srv!();
    let org = org(&srv).await;
    let conv = seed(&srv, Owner::Org(&org), "ended").await;
    let path = "/api/admin/live/session-transcript";

    // No reason, a thin one, a repeated character, no actor: nothing is read, nothing logged.
    for (body, expected) in [
        (
            json!({ "session_id": conv.live, "actor": OPERATOR }),
            "reason_required",
        ),
        (
            json!({ "session_id": conv.live, "actor": OPERATOR, "reason": "   " }),
            "reason_required",
        ),
        (
            json!({ "session_id": conv.live, "actor": OPERATOR, "reason": "support" }),
            "reason_too_short",
        ),
        (
            json!({ "session_id": conv.live, "actor": OPERATOR, "reason": "aaaaaaaaaaaaaaaa" }),
            "reason_too_short",
        ),
        (
            json!({ "session_id": conv.live, "reason": REASON }),
            "actor_required",
        ),
    ] {
        let (s, v, text) = srv.admin(path, body).await;
        assert_eq!(
            (s, code(&v)),
            (StatusCode::BAD_REQUEST, Some(expected)),
            "{text}"
        );
        assert!(
            !text.contains(SAID),
            "a refusal must not carry content: {text}"
        );
    }
    assert!(audit_rows(&srv, "live_session_transcript_read", conv.live)
        .await
        .is_empty());

    // An unknown session is a 404 and leaves no "read" on record.
    let ghost = Uuid::new_v4();
    let (s, v, _) = srv
        .admin(
            path,
            json!({ "session_id": ghost, "actor": OPERATOR, "reason": REASON }),
        )
        .await;
    assert_eq!(
        (s, code(&v)),
        (StatusCode::NOT_FOUND, Some("session_not_found"))
    );
    assert!(audit_rows(&srv, "live_session_transcript_read", ghost)
        .await
        .is_empty());

    let (s, v, _) = srv
        .admin(
            path,
            json!({ "session_id": conv.live, "actor": OPERATOR, "reason": REASON }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["audited"], true);
    assert_eq!(v["session"]["id"], json!(conv.live));
    // The tier the conversation runs on; a row with no stored tier reads as Standard.
    assert_eq!(v["session"]["engine_id"], "standard");
    let u = &v["utterances"][0];
    assert_eq!(u["original"], SAID);
    assert_eq!(u["original_lang"], "it");
    assert_eq!(u["speaker_name"], GUEST_NAME);
    assert_eq!(
        u["translations"]["es"],
        "mi codigo de reserva es cuatro siete"
    );
    assert_eq!(u["offset_ms"], 0);
    let names: Vec<&str> = v["participants"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["display_name"].as_str())
        .collect();
    assert!(
        names.contains(&GUEST_NAME) && names.contains(&STAFF_NAME),
        "{names:?}"
    );
    // The raw token hashes are not part of any admin answer.
    assert!(!v.to_string().contains("deadbeef") && !v.to_string().contains("cafebabe"));

    // The trace says who, which session and why — and nothing that was said or who said it.
    let audit = audit_rows(&srv, "live_session_transcript_read", conv.live).await;
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].0, OPERATOR);
    assert_eq!(audit[0].1["reason"], REASON);
    assert_eq!(audit[0].1["org_id"], json!(org.id));
    let stored = audit[0].1.to_string();
    for private in [SAID, GUEST_NAME, STAFF_NAME] {
        assert!(
            !stored.contains(private),
            "the audit holds `{private}`: {stored}"
        );
    }
    // A second read is a second row: access is counted, not cached.
    let (s, _, _) = srv
        .admin(
            path,
            json!({ "session_id": conv.live, "actor": OPERATOR, "reason": REASON }),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        audit_rows(&srv, "live_session_transcript_read", conv.live)
            .await
            .len(),
        2
    );
}

#[tokio::test]
async fn no_audit_row_means_no_transcript() {
    let srv = srv!();
    let org = org(&srv).await;
    let conv = seed(&srv, Owner::Org(&org), "ended").await;
    let breaker = "audit-breaker@test";

    // Make exactly this operator's audit insert fail, as a full disk or a lock would.
    sqlx::query(
        "CREATE OR REPLACE FUNCTION live_admin_test_fail_audit() RETURNS trigger AS $$
         BEGIN RAISE EXCEPTION 'audit unavailable (test)'; END $$ LANGUAGE plpgsql",
    )
    .execute(&srv.pool)
    .await
    .unwrap();
    sqlx::query("DROP TRIGGER IF EXISTS live_admin_test_fail_audit ON admin_audit")
        .execute(&srv.pool)
        .await
        .unwrap();
    sqlx::query(&format!(
        "CREATE TRIGGER live_admin_test_fail_audit BEFORE INSERT ON admin_audit
         FOR EACH ROW WHEN (NEW.actor = '{breaker}')
         EXECUTE FUNCTION live_admin_test_fail_audit()"
    ))
    .execute(&srv.pool)
    .await
    .unwrap();

    let (s, v, text) = srv
        .admin(
            "/api/admin/live/session-transcript",
            json!({ "session_id": conv.live, "actor": breaker, "reason": REASON }),
        )
        .await;
    let _ = sqlx::query("DROP TRIGGER IF EXISTS live_admin_test_fail_audit ON admin_audit")
        .execute(&srv.pool)
        .await;

    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR, "{text}");
    assert_eq!(code(&v), Some("internal_error"));
    assert!(
        !text.contains(SAID) && !text.contains(GUEST_NAME) && v.get("utterances").is_none(),
        "content was returned although the audit could not be written: {text}"
    );
    assert!(audit_rows(&srv, "live_session_transcript_read", conv.live)
        .await
        .is_empty());
}

// ---- audit is part of the action (D2) --------------------------------------------------

/// Anything a log viewer could mangle, and the NUL that Postgres refuses in `jsonb`: refused up
/// front, so the action can neither run unrecorded nor be reported as done.
#[tokio::test]
async fn control_characters_in_the_reason_or_actor_are_refused_and_nothing_runs() {
    let srv = srv!();
    let org = org(&srv).await;
    let (desk, _, _) = new_desk(&srv, &org).await;
    let conv = seed(&srv, Owner::Org(&org), "active").await;

    for bad in [
        "\u{0}", "\n", "\t", "\u{7f}", "\u{1b}", "\u{2028}", "\u{2029}", "\u{202E}", "\u{200B}",
        "\u{2066}",
    ] {
        let reason = format!("customer asked for a copy{bad} of what was said");
        let actor = format!("operator{bad}@voxtranslate.test");
        for (path, id_key, id) in [
            ("/api/admin/live/desk-revoke", "desk_id", desk),
            ("/api/admin/live/desk-regenerate", "desk_id", desk),
            ("/api/admin/live/session-end", "session_id", conv.live),
            (
                "/api/admin/live/session-transcript",
                "session_id",
                conv.live,
            ),
        ] {
            for body in [
                json!({ id_key: id, "actor": OPERATOR, "reason": reason }),
                json!({ id_key: id, "actor": actor, "reason": REASON }),
            ] {
                let (s, v, text) = srv.admin(path, body).await;
                assert_eq!(s, StatusCode::BAD_REQUEST, "{path} {bad:?}: {text}");
                assert!(
                    matches!(code(&v), Some("reason_invalid" | "actor_invalid")),
                    "{path} {bad:?}: {text}"
                );
            }
        }
    }
    let (status, version): (String, i32) =
        sqlx::query_as("SELECT status, token_version FROM live_desks WHERE id = $1")
            .bind(desk)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!((status.as_str(), version), ("active", 1), "nothing ran");
    let live_status: String = sqlx::query_scalar("SELECT status FROM live_sessions WHERE id = $1")
        .bind(conv.live)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert_eq!(live_status, "active");
    // And a reconcile cannot be made to run with an actor that its audit row cannot hold.
    let (s, v, _) = srv
        .admin(
            "/api/admin/live/session-reconcile",
            json!({ "session_id": conv.live, "actor": "op\u{0}erator" }),
        )
        .await;
    assert_eq!(
        (s, code(&v)),
        (StatusCode::BAD_REQUEST, Some("actor_invalid"))
    );
}

/// The audit row is written in the same transaction as the mutation: when it cannot be written
/// the action does not happen and the operator is not told it did.
#[tokio::test]
async fn an_action_whose_audit_row_cannot_be_written_does_not_happen() {
    let srv = srv!();
    let org = org(&srv).await;
    let (desk, qr, _) = new_desk(&srv, &org).await;
    let (s, scanned) = scan(&srv, &segment_of(&qr)).await;
    assert_eq!(s, StatusCode::OK, "{scanned}");
    let conv = seed(&srv, Owner::Org(&org), "active").await;
    let breaker = format!("audit-breaker-{}@test", Uuid::new_v4().simple());

    sqlx::query(
        "CREATE OR REPLACE FUNCTION live_admin_test_fail_audit_mut() RETURNS trigger AS $$
         BEGIN RAISE EXCEPTION 'audit unavailable (test)'; END $$ LANGUAGE plpgsql",
    )
    .execute(&srv.pool)
    .await
    .unwrap();
    let trigger = format!("live_admin_fail_{}", Uuid::new_v4().simple());
    sqlx::query(&format!(
        "CREATE TRIGGER {trigger} BEFORE INSERT ON admin_audit
         FOR EACH ROW WHEN (NEW.actor = '{breaker}')
         EXECUTE FUNCTION live_admin_test_fail_audit_mut()"
    ))
    .execute(&srv.pool)
    .await
    .unwrap();

    let mut answers = vec![];
    for (path, body) in [
        (
            "/api/admin/live/desk-revoke",
            json!({ "desk_id": desk, "actor": breaker, "reason": REASON }),
        ),
        (
            "/api/admin/live/desk-regenerate",
            json!({ "desk_id": desk, "actor": breaker, "reason": REASON }),
        ),
        (
            "/api/admin/live/session-end",
            json!({ "session_id": conv.live, "actor": breaker, "reason": REASON }),
        ),
    ] {
        answers.push((path, srv.admin(path, body).await));
    }
    let _ = sqlx::query(&format!("DROP TRIGGER IF EXISTS {trigger} ON admin_audit"))
        .execute(&srv.pool)
        .await;

    for (path, (s, v, text)) in &answers {
        assert_eq!(*s, StatusCode::INTERNAL_SERVER_ERROR, "{path}: {text}");
        assert_eq!(code(v), Some("internal_error"), "{path}");
    }
    // Nothing moved: the desk is active on its first version, the queue is intact, the
    // conversation is open and carries no admin end reason.
    let (status, version): (String, i32) =
        sqlx::query_as("SELECT status, token_version FROM live_desks WHERE id = $1")
            .bind(desk)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!((status.as_str(), version), ("active", 1));
    let waiting: i64 = scalar_i64(
        &srv.pool,
        "SELECT count(*) FROM live_sessions WHERE desk_id = $1 AND status = 'waiting'",
        desk,
    )
    .await;
    assert_eq!(waiting, 1, "the customer's place in the queue survived");
    let (live_status, reason): (String, Option<String>) =
        sqlx::query_as("SELECT status, end_reason FROM live_sessions WHERE id = $1")
            .bind(conv.live)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!((live_status.as_str(), reason), ("active", None));
    let events = scalar_i64(
        &srv.pool,
        "SELECT count(*) FROM live_session_events WHERE session_id = $1 AND kind = 'admin_ended'",
        conv.live,
    )
    .await;
    assert_eq!(events, 0);
}

// ---- reasons say what the rule really is (D7) ------------------------------------------

#[tokio::test]
async fn the_refusal_states_the_real_reason_rule() {
    let srv = srv!();
    let org = org(&srv).await;
    let (desk, _, _) = new_desk(&srv, &org).await;
    let (s, v, text) = srv
        .admin(
            "/api/admin/live/desk-revoke",
            json!({ "desk_id": desk, "actor": OPERATOR, "reason": "aaaaaaaaaaaaaaaa" }),
        )
        .await;
    assert_eq!(
        (s, code(&v)),
        (StatusCode::BAD_REQUEST, Some("reason_too_short")),
        "{text}"
    );
    // The old text only said "at least 3 characters" while 16 of them were still refused.
    let message = v["message"].as_str().unwrap();
    assert!(
        message.contains("at least 3 characters") && message.contains("4 different non-space"),
        "{message}"
    );
}

// ---- the privileged read says when it was cut short (D6) -------------------------------

#[tokio::test]
async fn a_transcript_longer_than_the_bound_says_it_was_truncated() {
    let srv = srv!();
    let org = org(&srv).await;
    let path = "/api/admin/live/session-transcript";

    let short = seed(&srv, Owner::Org(&org), "ended").await;
    let (s, v, _) = srv
        .admin(
            path,
            json!({ "session_id": short.live, "actor": OPERATOR, "reason": REASON }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["truncated"], false, "one utterance is not cut short");

    let long = seed(&srv, Owner::Org(&org), "ended").await;
    sqlx::query(
        "INSERT INTO transcript_events
             (session_id, event_type, speaker_peer_id, speaker_name, original_text, original_lang, translations, ts)
         SELECT $1, 'speech', $2, 'Gus', 'n' || g, 'it', '{}'::jsonb, now() - interval '3 minutes' + g * interval '1 millisecond'
         FROM generate_series(1, 5000) g",
    )
    .bind(long.call)
    .bind(long.staff_peer.to_string())
    .execute(&srv.pool)
    .await
    .unwrap();
    // 5001 utterances in all (the seeded one plus 5000).
    let (s, v, _) = srv
        .admin(
            path,
            json!({ "session_id": long.live, "actor": OPERATOR, "reason": REASON }),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["utterances"].as_array().unwrap().len(), 5000);
    assert_eq!(v["truncated"], true, "a cut transcript must say so");

    // Exactly at the bound is not truncated.
    sqlx::query("DELETE FROM transcript_events WHERE session_id = $1 AND original_text = 'n5000'")
        .bind(long.call)
        .execute(&srv.pool)
        .await
        .unwrap();
    let (_, v, _) = srv
        .admin(
            path,
            json!({ "session_id": long.live, "actor": OPERATOR, "reason": REASON }),
        )
        .await;
    assert_eq!(v["utterances"].as_array().unwrap().len(), 5000);
    assert_eq!(v["truncated"], false);
}

// ---- reconcile is a read, proven by content (D5) and compares amounts one by one (D8) --

/// An md5 of every row of the tables a reconcile could touch, scoped to this test's own org and
/// conversation, so tests sharing one database cannot disturb each other. Row COUNTS would not
/// notice a rewritten amount.
async fn snapshot(srv: &Srv, org: &Org, conv: &Conv) -> Vec<(&'static str, String)> {
    let probes: [(&'static str, &str, Uuid); 9] = [
        ("live_sessions", "id", conv.live),
        ("live_participants", "session_id", conv.live),
        ("live_usage_ticks", "session_id", conv.live),
        ("live_session_events", "session_id", conv.live),
        ("call_sessions", "id", conv.call),
        ("transcript_events", "session_id", conv.call),
        ("organization_credits_transactions", "org_id", org.id),
        ("organizations", "id", org.id),
        ("live_desks", "org_id", org.id),
    ];
    let mut out = Vec::new();
    for (table, col, id) in probes {
        let hash: String = sqlx::query_scalar(&format!(
            "SELECT COALESCE(md5(string_agg(x::text, '|' ORDER BY x::text)), 'empty')
             FROM {table} x WHERE x.{col} = $1"
        ))
        .bind(id)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
        out.push((table, hash));
    }
    out
}

#[tokio::test]
async fn reconcile_changes_no_row_of_any_table_it_reads() {
    let srv = srv!();
    let org = org(&srv).await;
    let path = "/api/admin/live/session-reconcile";

    // Every verdict the report can give, each compared byte for byte before and after.
    let ok = seed(&srv, Owner::Org(&org), "ended").await;
    charge_org(&srv, &org, &ok, 3).await;
    let mismatch = seed(&srv, Owner::Org(&org), "ended").await;
    charge_org(&srv, &org, &mismatch, 2).await;
    sqlx::query("UPDATE organization_credits_transactions SET amount = -5 WHERE id = (SELECT id FROM organization_credits_transactions WHERE session_id = $1 LIMIT 1)")
        .bind(mismatch.call)
        .execute(&srv.pool)
        .await
        .unwrap();
    let unlinked = seed(&srv, Owner::Org(&org), "ended").await;
    charge_org(&srv, &org, &unlinked, 1).await;
    sqlx::query("DELETE FROM call_sessions WHERE id = $1")
        .bind(unlinked.call)
        .execute(&srv.pool)
        .await
        .unwrap();
    let (_, user) = login(&srv, "Solo").await;
    let consumer = seed(&srv, Owner::User(user), "ended").await;

    for (conv, expected) in [
        (&ok, "ok"),
        (&mismatch, "mismatch"),
        (&unlinked, "unlinked"),
        (&consumer, "ok"),
    ] {
        let before = snapshot(&srv, &org, conv).await;
        let (s, v, _) = srv
            .admin(path, json!({ "session_id": conv.live, "actor": OPERATOR }))
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["status"], expected, "{v}");
        assert_eq!(snapshot(&srv, &org, conv).await, before, "{expected}");
    }
}

/// Counts and sums agree, the single debits do not (`-1` and `-3` against two ticks of 2): that
/// is not a conversation the books explain.
#[tokio::test]
async fn reconcile_compares_each_debit_with_its_tick_not_just_the_totals() {
    let srv = srv!();
    let org = org(&srv).await;
    let conv = seed(&srv, Owner::Org(&org), "ended").await;
    charge_org(&srv, &org, &conv, 2).await;
    sqlx::query("UPDATE organization_credits_transactions SET amount = -1 WHERE id = (SELECT id FROM organization_credits_transactions WHERE session_id = $1 ORDER BY created_at, id LIMIT 1)")
        .bind(conv.call)
        .execute(&srv.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE organization_credits_transactions SET amount = -3 WHERE id = (SELECT id FROM organization_credits_transactions WHERE session_id = $1 AND amount = -2 LIMIT 1)")
        .bind(conv.call)
        .execute(&srv.pool)
        .await
        .unwrap();
    let (s, v, _) = srv
        .admin(
            "/api/admin/live/session-reconcile",
            json!({ "session_id": conv.live }),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    // Totals still agree...
    assert_eq!(v["ticks"]["org_credits"], v["ledger"]["org_credits"], "{v}");
    assert_eq!(v["ticks"]["count"], v["ledger"]["org_entries"], "{v}");
    // ...and the verdict is a mismatch all the same.
    assert_eq!(v["status"], "mismatch", "{v}");
}

// ---- several guests in one conversation (spec 0121, S1) --------------------------------

#[tokio::test]
async fn an_operator_reads_and_ends_a_conversation_with_several_guests() {
    let srv = srv!();
    let org = org(&srv).await;
    let conv = seed(&srv, Owner::Org(&org), "active").await;
    for (name, lang) in [("Ben Guest", "en"), ("Carla Guest", "pt")] {
        sqlx::query(
            "INSERT INTO live_participants (session_id, role, display_name, lang, token_hash, consent_at, joined_at)
             VALUES ($1, 'guest', $2, $3, '\\xfeedface', now(), now() + interval '1 minute')",
        )
        .bind(conv.live)
        .bind(name)
        .bind(lang)
        .execute(&srv.pool)
        .await
        .unwrap();
    }

    let (s, v, text) = srv
        .admin(
            "/api/admin/live/session-transcript",
            json!({ "session_id": conv.live, "actor": OPERATOR, "reason": REASON }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{text}");
    let who: Vec<(&str, &str)> = v["participants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["role"].as_str().unwrap(),
                p["display_name"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(who.len(), 4, "{who:?}");
    assert_eq!(who.iter().filter(|(role, _)| *role == "guest").count(), 3);
    assert!(who.contains(&("guest", "Carla Guest")));
    assert!(!text.contains("feedface"), "token hashes never leave");

    let (s, v, text) = srv
        .admin(
            "/api/admin/live/session-end",
            json!({ "session_id": conv.live, "actor": OPERATOR, "reason": REASON }),
        )
        .await;
    assert_eq!(
        (s, v["end_reason"].as_str()),
        (StatusCode::OK, Some("ended_by_admin")),
        "{text}"
    );
}
