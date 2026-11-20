//! Live Connect REST (spec 0121, S1): create, join, read, end — and every way each can be
//! refused. Drives the real router over a socket against a real database.
//!
//! DB-gated like the other integration suites: with no `DATABASE_URL` every test returns early.
//! Run against the local pgvector Postgres (`docker compose up -d --wait postgres`):
//! `DATABASE_URL=postgres://vox:vox_local_dev@127.0.0.1:55432/voxtranslate_voip_test \
//!   cargo test --test live_rest`. A run of a few milliseconds skipped everything.

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::StatusCode;
use serde_json::{json, Value};
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::{Config, QwenConfig};
use voxtranslate_server::db::{self, Pool};
use voxtranslate_server::live::config::LiveConfig;
use voxtranslate_server::live::tokens::{GuestClaims, LiveKeys, PeerRole};
use voxtranslate_server::live::LiveState;
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::{app, AppState};

const SECRET: &str = "live-rest-secret";
const LIVE_KEY: &str = "0123456789abcdef0123456789abcdef-live-rest";

struct Srv {
    addr: SocketAddr,
    pool: Pool,
    http: reqwest::Client,
}

#[derive(Default, Clone, Copy)]
struct Opts {
    /// `false` leaves the feature off (no routes).
    disabled: bool,
    /// Upstream session cap; 1 makes the capacity gate refuse.
    qwen_max_sessions: Option<usize>,
    max_concurrent: Option<&'static str>,
    /// `LIVE_MAX_GUESTS`: the most guests a host may ask for.
    max_guests: Option<&'static str>,
}

async fn setup(opts: Opts) -> Option<Srv> {
    let url = db::test_database_url()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let mut config = Config::test_with_billing(&url, SECRET, 5.0);
    config.qwen = QwenConfig {
        api_key: "test-key".into(),
        max_sessions: opts.qwen_max_sessions.unwrap_or(32),
        ..Default::default()
    };
    config.standard_enabled = true;
    let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);

    let mut state = AppState::new(config);
    state.billing = Some(BillingService::new(pool.clone(), min_join));
    state.safety = Some(SafetyService::new(pool.clone()));
    state.glossary = Some(voxtranslate_server::glossary::GlossaryService::new(
        pool.clone(),
        50,
    ));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);
    let live_cfg = if opts.disabled {
        LiveConfig::disabled()
    } else {
        LiveConfig::load(|k| match k {
            "LIVE_CONNECT_ENABLED" => Some("1".into()),
            "LIVE_TOKEN_KEY" => Some(LIVE_KEY.into()),
            "LIVE_MAX_CONCURRENT_PER_OWNER" => opts.max_concurrent.map(Into::into),
            "LIVE_MAX_GUESTS" => opts.max_guests.map(Into::into),
            _ => None,
        })
        .unwrap()
    };
    state.live = Arc::new(LiveState::new(live_cfg));

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
    ($opts:expr) => {
        match setup($opts).await {
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

/// A signed-in user with credit. Returns `(account jwt, user id)`.
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

async fn create_b2c(srv: &Srv, jwt: &str, lang: &str) -> (StatusCode, Value) {
    srv.post("/api/live/sessions", Some(jwt), json!({ "lang": lang }))
        .await
}

/// The invitation code is the last path segment of the invite URL.
fn code_of(created: &Value) -> String {
    created["invite_url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_string()
}

async fn join(srv: &Srv, code: &str, name: &str, lang: &str) -> (StatusCode, Value) {
    srv.post(
        "/api/live/join",
        None,
        json!({ "code": code, "name": name, "lang": lang, "consent": true }),
    )
    .await
}

// ---- B2C happy path --------------------------------------------------------------------

#[tokio::test]
async fn an_owner_creates_a_session_and_a_guest_joins_without_an_account() {
    let srv = srv!(Opts::default());
    let (jwt, _) = login(&srv, "Marco").await;

    let (status, created) = create_b2c(&srv, &jwt, "IT").await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let invite = created["invite_url"].as_str().unwrap();
    assert!(invite.contains("/lc/"), "{invite}");
    assert_eq!(
        created["qr_payload"], created["invite_url"],
        "the QR carries exactly the link"
    );
    assert_eq!(created["ws_path"], "/ws/live");
    let host_token = created["token"].as_str().unwrap();
    assert!(
        !host_token.contains(&jwt),
        "the account JWT never leaves the account"
    );

    // The invitation code is stored hashed: the raw code is nowhere in the database.
    let code = code_of(&created);
    let leaked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM live_sessions WHERE encode(invite_hash, 'escape') = $1",
    )
    .bind(&code)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(leaked, 0);

    // No account, no Authorization header: just the code, a name, a language and consent.
    let (status, joined) = join(&srv, &code, "Lucia", "es").await;
    assert_eq!(status, StatusCode::OK, "{joined}");
    assert_eq!(joined["session_id"], created["session_id"]);
    assert_eq!(joined["host"]["lang"], "it", "the language was normalised");
    assert_eq!(joined["host"]["name"], "Marco");

    // Both sides now see two participants and an active session.
    let sid = created["session_id"].as_str().unwrap();
    let (status, view) = srv
        .get(
            &format!("/api/live/sessions/{sid}"),
            joined["token"].as_str().unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["session"]["status"], "active");
    assert_eq!(view["session"]["participants"].as_array().unwrap().len(), 2);
    assert_eq!(view["you"]["role"], "guest");
}

#[tokio::test]
async fn one_invitation_admits_exactly_one_guest() {
    let srv = srv!(Opts::default());
    let (jwt, _) = login(&srv, "Marco").await;
    let (_, created) = create_b2c(&srv, &jwt, "it").await;
    let code = code_of(&created);

    assert_eq!(join(&srv, &code, "First", "es").await.0, StatusCode::OK);
    let (status, body) = join(&srv, &code, "Second", "fr").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "session_full");
}

#[tokio::test]
async fn joining_needs_consent_a_real_code_and_a_real_language() {
    let srv = srv!(Opts::default());
    let (jwt, _) = login(&srv, "Marco").await;
    let (_, created) = create_b2c(&srv, &jwt, "it").await;
    let code = code_of(&created);

    let (s, b) = srv
        .post(
            "/api/live/join",
            None,
            json!({ "code": code, "name": "G", "lang": "es", "consent": false }),
        )
        .await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("consent_required"))
    );

    let (s, b) = join(&srv, &code, "G", "auto").await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("bad_lang")),
        "language is declared, never guessed"
    );

    // A malformed code and an unknown one answer identically: no oracle.
    let unknown = "A".repeat(32);
    let (s1, b1) = join(&srv, "short", "G", "es").await;
    let (s2, b2) = join(&srv, &unknown, "G", "es").await;
    assert_eq!((s1, s2), (StatusCode::NOT_FOUND, StatusCode::NOT_FOUND));
    assert_eq!(b1, b2);
    assert_eq!(b1["code"], "invite_not_found");
}

#[tokio::test]
async fn an_unused_invitation_expires() {
    let srv = srv!(Opts::default());
    let (jwt, _) = login(&srv, "Marco").await;
    let (_, created) = create_b2c(&srv, &jwt, "it").await;
    let sid: Uuid = created["session_id"].as_str().unwrap().parse().unwrap();
    sqlx::query("UPDATE live_sessions SET expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(sid)
        .execute(&srv.pool)
        .await
        .unwrap();

    let (s, b) = join(&srv, &code_of(&created), "G", "es").await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::GONE, Some("invite_expired"))
    );
    let status: String = sqlx::query_scalar("SELECT status FROM live_sessions WHERE id = $1")
        .bind(sid)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert_eq!(status, "expired");
}

// ---- ending ----------------------------------------------------------------------------

#[tokio::test]
async fn ending_a_session_closes_every_seat() {
    let srv = srv!(Opts::default());
    let (jwt, _) = login(&srv, "Marco").await;
    let (_, created) = create_b2c(&srv, &jwt, "it").await;
    let sid = created["session_id"].as_str().unwrap();
    let code = code_of(&created);
    let (_, joined) = join(&srv, &code, "Lucia", "es").await;

    let (s, b) = srv
        .post(
            &format!("/api/live/sessions/{sid}/end"),
            Some(joined["token"].as_str().unwrap()),
            json!({}),
        )
        .await;
    assert_eq!((s, b["ended"].as_bool()), (StatusCode::OK, Some(true)));

    // The host's token stops working the moment the session is over, and so does the code.
    let (s, b) = srv
        .get(
            &format!("/api/live/sessions/{sid}"),
            created["token"].as_str().unwrap(),
        )
        .await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::GONE, Some("session_ended"))
    );
    let (s, b) = join(&srv, &code, "Late", "fr").await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::GONE, Some("session_ended"))
    );

    // Ending twice: the seat is already closed, so the second call is refused, not repeated.
    let (s, _) = srv
        .post(
            &format!("/api/live/sessions/{sid}/end"),
            Some(created["token"].as_str().unwrap()),
            json!({}),
        )
        .await;
    assert_eq!(s, StatusCode::GONE);
}

#[tokio::test]
async fn a_token_works_only_for_its_own_session_and_cannot_be_forged() {
    let srv = srv!(Opts::default());
    let (jwt, _) = login(&srv, "Marco").await;
    let (_, a) = create_b2c(&srv, &jwt, "it").await;
    let (_, b) = create_b2c(&srv, &jwt, "it").await;
    let sid_a = a["session_id"].as_str().unwrap();

    let (s, body) = srv
        .get(
            &format!("/api/live/sessions/{sid_a}"),
            b["token"].as_str().unwrap(),
        )
        .await;
    assert_eq!(
        (s, body["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("wrong_session"))
    );

    // The account JWT is not a Live Connect token.
    let (s, _) = srv.get(&format!("/api/live/sessions/{sid_a}"), &jwt).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // A token signed with another key is refused, even with real ids in it.
    let other =
        LiveKeys::from_secret("a-completely-different-secret-of-sufficient-length").unwrap();
    let peer: Uuid = a["peer_id"].as_str().unwrap().parse().unwrap();
    let forged = other.mint_guest_token(&GuestClaims::new(
        sid_a.parse().unwrap(),
        peer,
        PeerRole::Host,
        4_000_000_000,
    ));
    let (s, _) = srv
        .get(&format!("/api/live/sessions/{sid_a}"), &forged)
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // A correctly signed token that claims the OTHER seat's role is refused too.
    let mine = LiveKeys::from_secret(LIVE_KEY).unwrap();
    let wrong_role = mine.mint_guest_token(&GuestClaims::new(
        sid_a.parse().unwrap(),
        peer,
        PeerRole::Guest,
        4_000_000_000,
    ));
    let (s, _) = srv
        .get(&format!("/api/live/sessions/{sid_a}"), &wrong_role)
        .await;
    assert_eq!(s, StatusCode::GONE);
}

// ---- limits and gates ------------------------------------------------------------------

#[tokio::test]
async fn creating_needs_an_account_with_credit() {
    let srv = srv!(Opts::default());
    let (s, _) = srv
        .post("/api/live/sessions", None, json!({ "lang": "it" }))
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    let (jwt, uid) = login(&srv, "Broke").await;
    sqlx::query("UPDATE users SET balance = 0 WHERE id = $1")
        .bind(uid)
        .execute(&srv.pool)
        .await
        .unwrap();
    let (s, b) = create_b2c(&srv, &jwt, "it").await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::PAYMENT_REQUIRED, Some("insufficient_balance"))
    );
}

#[tokio::test]
async fn an_owner_cannot_hold_more_open_sessions_than_the_cap() {
    let srv = srv!(Opts {
        max_concurrent: Some("2"),
        ..Opts::default()
    });
    let (jwt, _) = login(&srv, "Marco").await;

    let (s1, first) = create_b2c(&srv, &jwt, "it").await;
    let (s2, _) = create_b2c(&srv, &jwt, "it").await;
    assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK));
    let (s, b) = create_b2c(&srv, &jwt, "it").await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::TOO_MANY_REQUESTS, Some("too_many_sessions"))
    );

    // Ending one frees the slot.
    let sid = first["session_id"].as_str().unwrap();
    srv.post(
        &format!("/api/live/sessions/{sid}/end"),
        Some(first["token"].as_str().unwrap()),
        json!({}),
    )
    .await;
    assert_eq!(create_b2c(&srv, &jwt, "it").await.0, StatusCode::OK);
}

#[tokio::test]
async fn simultaneous_creates_cannot_beat_the_cap() {
    let srv = Arc::new(srv!(Opts {
        max_concurrent: Some("2"),
        ..Opts::default()
    }));
    let (jwt, _) = login(&srv, "Racer").await;

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let srv = srv.clone();
        let jwt = jwt.clone();
        tasks.push(tokio::spawn(
            async move { create_b2c(&srv, &jwt, "it").await.0 },
        ));
    }
    let mut ok = 0;
    for t in tasks {
        if t.await.unwrap() == StatusCode::OK {
            ok += 1;
        }
    }
    assert_eq!(ok, 2, "exactly the cap, however many arrive at once");
}

#[tokio::test]
async fn the_engine_capacity_gate_refuses_before_a_direction_would_go_silent() {
    let srv = srv!(Opts {
        qwen_max_sessions: Some(1),
        ..Opts::default()
    });
    let (jwt, _) = login(&srv, "Marco").await;
    let (s, b) = create_b2c(&srv, &jwt, "it").await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("live_busy"))
    );
}

#[tokio::test]
async fn switched_off_means_the_routes_do_not_exist() {
    let srv = srv!(Opts {
        disabled: true,
        ..Opts::default()
    });
    let (jwt, _) = login(&srv, "Marco").await;
    assert_eq!(create_b2c(&srv, &jwt, "it").await.0, StatusCode::NOT_FOUND);
    let (s, _) = srv.post("/api/live/join", None, json!({})).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

// ---- B2B -------------------------------------------------------------------------------

struct Org {
    id: Uuid,
    project: Uuid,
}

/// An org with an active subscription and credit, one project, and `member` as a member.
async fn org_with_member(srv: &Srv, owner: Uuid, member: Option<Uuid>) -> Org {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id, subscription_status, current_period_end, credits_balance)
         VALUES ('Hotel', $1, $2, 'active', now() + interval '30 days', 1000) RETURNING id",
    )
    .bind(format!("slug-{}", Uuid::new_v4().simple()))
    .bind(owner)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, 'owner') ON CONFLICT DO NOTHING")
        .bind(id)
        .bind(owner)
        .execute(&srv.pool)
        .await
        .unwrap();
    if let Some(m) = member {
        sqlx::query(
            "INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, 'member')",
        )
        .bind(id)
        .bind(m)
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
    Org { id, project }
}

async fn create_b2b(srv: &Srv, jwt: &str, org: Uuid, project: Option<Uuid>) -> (StatusCode, Value) {
    srv.post(
        "/api/live/sessions",
        Some(jwt),
        json!({ "lang": "it", "org_id": org, "project_id": project }),
    )
    .await
}

#[tokio::test]
async fn a_member_creates_a_business_session_bound_to_a_project_of_their_org() {
    let srv = srv!(Opts::default());
    let (_, owner) = login(&srv, "Owner").await;
    let (jwt, member) = login(&srv, "Receptionist").await;
    let org = org_with_member(&srv, owner, Some(member)).await;

    let (s, created) = create_b2b(&srv, &jwt, org.id, Some(org.project)).await;
    assert_eq!(s, StatusCode::OK, "{created}");

    // Ownership is the org's, the project is persisted, and the transcript is kept.
    let sid: Uuid = created["session_id"].as_str().unwrap().parse().unwrap();
    let (owner_user, org_id, project_id, save): (Option<Uuid>, Option<Uuid>, Option<Uuid>, bool) =
        sqlx::query_as("SELECT owner_user_id, org_id, project_id, save_transcript FROM live_sessions WHERE id = $1")
            .bind(sid)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(
        (owner_user, org_id, project_id, save),
        (None, Some(org.id), Some(org.project), true)
    );
}

#[tokio::test]
async fn business_sessions_refuse_foreign_orgs_projects_and_ineligible_wallets() {
    let srv = srv!(Opts::default());
    let (_, owner) = login(&srv, "Owner").await;
    let (jwt, member) = login(&srv, "Receptionist").await;
    let org = org_with_member(&srv, owner, Some(member)).await;
    let (_, other_owner) = login(&srv, "Other").await;
    let other = org_with_member(&srv, other_owner, None).await;

    // Not a member of that org: it does not exist as far as they can tell.
    let (s, _) = create_b2b(&srv, &jwt, other.id, Some(other.project)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // A project from ANOTHER tenant, with a valid org of their own.
    let (s, b) = create_b2b(&srv, &jwt, org.id, Some(other.project)).await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("project_not_found"))
    );

    // A business conversation belongs to a project.
    let (s, b) = create_b2b(&srv, &jwt, org.id, None).await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("project_required"))
    );

    // An archived project is gone for this purpose.
    sqlx::query("UPDATE projects SET archived_at = now() WHERE id = $1")
        .bind(org.project)
        .execute(&srv.pool)
        .await
        .unwrap();
    let (s, b) = create_b2b(&srv, &jwt, org.id, Some(org.project)).await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("project_not_found"))
    );
    sqlx::query("UPDATE projects SET archived_at = NULL WHERE id = $1")
        .bind(org.project)
        .execute(&srv.pool)
        .await
        .unwrap();

    // Out of credit, then lapsed: the org wallet gates it before anything is created.
    sqlx::query("UPDATE organizations SET credits_balance = 0 WHERE id = $1")
        .bind(org.id)
        .execute(&srv.pool)
        .await
        .unwrap();
    let (s, b) = create_b2b(&srv, &jwt, org.id, Some(org.project)).await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::PAYMENT_REQUIRED, Some("insufficient_credits"))
    );
    sqlx::query("UPDATE organizations SET credits_balance = 1000, subscription_status = 'canceled' WHERE id = $1")
        .bind(org.id)
        .execute(&srv.pool)
        .await
        .unwrap();
    let (s, b) = create_b2b(&srv, &jwt, org.id, Some(org.project)).await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::PAYMENT_REQUIRED, Some("subscription_required"))
    );
}

#[tokio::test]
async fn business_sessions_are_capped_per_member_too() {
    let srv = srv!(Opts {
        max_concurrent: Some("2"),
        ..Opts::default()
    });
    let (_, owner) = login(&srv, "Owner").await;
    let (jwt, member) = login(&srv, "Receptionist").await;
    let org = org_with_member(&srv, owner, Some(member)).await;

    // The cap follows the PERSON who opens conversations, not only a personal wallet: one
    // member of an org cannot be used to open an unbounded number of them.
    let (a, _) = create_b2b(&srv, &jwt, org.id, Some(org.project)).await;
    let (b, _) = create_b2b(&srv, &jwt, org.id, Some(org.project)).await;
    assert_eq!((a, b), (StatusCode::OK, StatusCode::OK));
    let (s, body) = create_b2b(&srv, &jwt, org.id, Some(org.project)).await;
    assert_eq!(
        (s, body["code"].as_str()),
        (StatusCode::TOO_MANY_REQUESTS, Some("too_many_sessions"))
    );
}

// ---- a live conversation's room is not a place to keep a glossary (R6) -----------------

/// The room name of a conversation is visible to every member of an org (waiting queue, search
/// hits). Any signed-in account could write a glossary into it, which the conversation's engine
/// would then apply to what its two people say. The glossary routes treat it as reserved, like
/// `/ws` and the room binding do; every other room name is as open as it ever was.
#[tokio::test]
async fn a_live_conversations_room_name_cannot_carry_a_glossary() {
    let srv = srv!(Opts::default());
    let (jwt, _) = login(&srv, "Mallory").await;
    let live = format!("live-{}", Uuid::new_v4());
    let ordinary = "live-room";
    let send = |method: reqwest::Method, path: String, body: Option<Value>| {
        let mut req = srv
            .http
            .request(method, format!("http://{}{path}", srv.addr))
            .bearer_auth(&jwt);
        if let Some(b) = body {
            req = req.json(&b);
        }
        async move { req.send().await.unwrap().status() }
    };
    let glossary = json!({ "name": "x", "entries": [] });

    for (method, suffix, body) in [
        (reqwest::Method::GET, "glossary", None),
        (reqwest::Method::POST, "glossary", Some(glossary.clone())),
        (reqwest::Method::DELETE, "glossary", None),
        (
            reqwest::Method::POST,
            "glossary/import",
            Some(json!({ "csv": "source,target\nhello,hola\n" })),
        ),
    ] {
        let status = send(method.clone(), format!("/api/rooms/{live}/{suffix}"), body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {suffix}");
    }
    // Nothing was stored for it.
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM room_glossaries WHERE room = $1")
        .bind(&live)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert_eq!(stored, 0);
    // A room that merely starts with "live-" is an ordinary room.
    let status = send(
        reqwest::Method::POST,
        format!("/api/rooms/{ordinary}/glossary"),
        Some(glossary),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// Inviting people to a room by e-mail sends a link to `/?room=<room>`. For a live conversation
/// that link opens nothing (its door is the owner's invitation), so the route must not become a
/// way to e-mail strangers a link that carries a conversation's name.
#[tokio::test]
async fn a_live_conversations_room_cannot_be_the_subject_of_an_email_invite() {
    let srv = srv!(Opts::default());
    let (jwt, _) = login(&srv, "Mallory").await;
    let invite = |room: String| {
        let req = srv
            .http
            .post(format!("http://{}/api/rooms/{room}/invite", srv.addr))
            .bearer_auth(&jwt)
            .json(&json!({ "emails": ["someone@example.com"] }));
        async move { req.send().await.unwrap().status() }
    };
    let live = format!("live-{}", Uuid::new_v4());
    assert_eq!(invite(live.clone()).await, StatusCode::FORBIDDEN);
    assert_eq!(invite(live.to_uppercase()).await, StatusCode::FORBIDDEN);
    // An ordinary room is untouched by the guard (no e-mail provider here: 503, not 403).
    assert_eq!(
        invite("live-room".into()).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

// ---- several guests on one invitation (spec 0121, S1) -----------------------------------

async fn create_with(srv: &Srv, jwt: &str, lang: &str, max_guests: Value) -> (StatusCode, Value) {
    srv.post(
        "/api/live/sessions",
        Some(jwt),
        json!({ "lang": lang, "max_guests": max_guests }),
    )
    .await
}

async fn guest_rows(srv: &Srv, session: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM live_participants WHERE session_id = $1::uuid AND role = 'guest'",
    )
    .bind(session)
    .fetch_one(&srv.pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn a_session_defaults_to_one_guest_and_says_how_many_it_admits() {
    let srv = srv!(Opts::default());
    let (jwt, _) = login(&srv, "Marco").await;

    let (s, created) = create_b2c(&srv, &jwt, "it").await;
    assert_eq!(s, StatusCode::OK, "{created}");
    assert_eq!(created["max_guests"], 1, "today's behaviour is the default");
    let sid = created["session_id"].as_str().unwrap();
    let stored: i16 =
        sqlx::query_scalar("SELECT max_guests FROM live_sessions WHERE id = $1::uuid")
            .bind(sid)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(stored, 1);

    // `null` is "not given".
    let (s, created) = create_with(&srv, &jwt, "it", Value::Null).await;
    assert_eq!(
        (s, created["max_guests"].as_i64()),
        (StatusCode::OK, Some(1))
    );

    // Asking for three is persisted, returned, and visible to every participant.
    let (s, created) = create_with(&srv, &jwt, "it", json!(3)).await;
    assert_eq!(s, StatusCode::OK, "{created}");
    assert_eq!(created["max_guests"], 3);
    let (_, joined) = join(&srv, &code_of(&created), "Lucia", "es").await;
    assert_eq!(joined["max_guests"], 3, "the guest is told too");
    let id = created["session_id"].as_str().unwrap();
    let (_, view) = srv
        .get(
            &format!("/api/live/sessions/{id}"),
            joined["token"].as_str().unwrap(),
        )
        .await;
    assert_eq!(view["session"]["max_guests"], 3);
}

#[tokio::test]
async fn a_bad_guest_count_is_refused_with_a_stable_code_and_creates_nothing() {
    let srv = srv!(Opts::default());
    let (jwt, user) = login(&srv, "Marco").await;
    for bad in [
        json!(0),
        json!(4),
        json!(-1),
        json!(99),
        json!("2"),
        json!(1.5),
        json!(true),
        json!([2]),
    ] {
        let (s, b) = create_with(&srv, &jwt, "it", bad.clone()).await;
        assert_eq!(
            (s, b["code"].as_str()),
            (StatusCode::BAD_REQUEST, Some("bad_max_guests")),
            "{bad}"
        );
    }
    let made: i64 =
        sqlx::query_scalar("SELECT count(*) FROM live_sessions WHERE owner_user_id = $1")
            .bind(user)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(made, 0, "a refused create leaves no session behind");
}

#[tokio::test]
async fn the_operator_can_lower_how_many_guests_a_host_may_ask_for() {
    let srv = srv!(Opts {
        max_guests: Some("2"),
        ..Opts::default()
    });
    let (jwt, _) = login(&srv, "Marco").await;
    assert_eq!(
        create_with(&srv, &jwt, "it", json!(2)).await.0,
        StatusCode::OK
    );
    let (s, b) = create_with(&srv, &jwt, "it", json!(3)).await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("bad_max_guests"))
    );
}

#[tokio::test]
async fn one_invitation_admits_guests_one_after_the_other_each_with_a_seat_of_their_own() {
    let srv = srv!(Opts::default());
    let (jwt, _) = login(&srv, "Marco").await;
    let (_, created) = create_with(&srv, &jwt, "it", json!(3)).await;
    let code = code_of(&created);
    let sid = created["session_id"].as_str().unwrap();

    let mut seats = Vec::new();
    for (name, lang) in [("Lucia", "es"), ("Pablo", "es"), ("Ann", "en")] {
        let (s, joined) = join(&srv, &code, name, lang).await;
        assert_eq!(s, StatusCode::OK, "{name}: {joined}");
        assert_eq!(joined["session_id"], created["session_id"]);
        seats.push((
            joined["peer_id"].as_str().unwrap().to_string(),
            joined["token"].as_str().unwrap().to_string(),
        ));
    }
    // Three different seats and three different credentials, each with its own consent.
    let mut peers: Vec<_> = seats.iter().map(|(p, _)| p.clone()).collect();
    let mut tokens: Vec<_> = seats.iter().map(|(_, t)| t.clone()).collect();
    peers.sort();
    peers.dedup();
    tokens.sort();
    tokens.dedup();
    assert_eq!((peers.len(), tokens.len()), (3, 3));
    let consented: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM live_participants
         WHERE session_id = $1::uuid AND role = 'guest' AND consent_at IS NOT NULL AND token_hash IS NOT NULL",
    )
    .bind(sid)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(consented, 3);

    // Everybody sees everybody, in join order, host first.
    let (_, view) = srv
        .get(&format!("/api/live/sessions/{sid}"), &seats[2].1)
        .await;
    let people: Vec<(&str, &str)> = view["session"]["participants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["display_name"].as_str().unwrap(),
                p["lang"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        people,
        vec![
            ("Marco", "it"),
            ("Lucia", "es"),
            ("Pablo", "es"),
            ("Ann", "en")
        ]
    );
    assert_eq!(view["session"]["status"], "active");

    // A fourth guest finds the conversation full, with the same stable code as before.
    let (s, b) = join(&srv, &code, "Late", "fr").await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::CONFLICT, Some("session_full"))
    );
    assert_eq!(guest_rows(&srv, sid).await, 3, "and took no seat");
}

#[tokio::test]
async fn a_session_that_admits_two_is_full_at_two() {
    let srv = srv!(Opts::default());
    let (jwt, _) = login(&srv, "Marco").await;
    let (_, created) = create_with(&srv, &jwt, "it", json!(2)).await;
    let code = code_of(&created);
    assert_eq!(join(&srv, &code, "A", "es").await.0, StatusCode::OK);
    assert_eq!(join(&srv, &code, "B", "en").await.0, StatusCode::OK);
    let (s, b) = join(&srv, &code, "C", "fr").await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::CONFLICT, Some("session_full"))
    );
}

#[tokio::test]
async fn simultaneous_joins_admit_exactly_as_many_guests_as_the_session_allows() {
    let srv = Arc::new(srv!(Opts::default()));
    let (jwt, _) = login(&srv, "Marco").await;
    for max in [1i64, 3] {
        let (_, created) = create_with(&srv, &jwt, "it", json!(max)).await;
        let code = code_of(&created);
        let sid = created["session_id"].as_str().unwrap().to_string();
        let mut tasks = Vec::new();
        for i in 0..10 {
            let (srv, code) = (srv.clone(), code.clone());
            tasks.push(tokio::spawn(async move {
                join(&srv, &code, &format!("G{i}"), "es").await
            }));
        }
        let (mut ok, mut full) = (0, 0);
        for t in tasks {
            let (s, b) = t.await.unwrap();
            match (s, b["code"].as_str()) {
                (StatusCode::OK, _) => ok += 1,
                (StatusCode::CONFLICT, Some("session_full")) => full += 1,
                other => panic!("unexpected answer {other:?}"),
            }
        }
        assert_eq!((ok, full), (max, 10 - max), "max_guests = {max}");
        assert_eq!(guest_rows(&srv, &sid).await, max, "the database agrees");
        // Ending frees the owner's concurrent-session slot for the next round.
        sqlx::query("UPDATE live_sessions SET status = 'ended' WHERE id = $1::uuid")
            .bind(&sid)
            .execute(&srv.pool)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn the_join_window_closes_on_a_session_nobody_joined_but_not_on_one_in_progress() {
    let srv = srv!(Opts::default());
    let (jwt, _) = login(&srv, "Marco").await;
    let (_, created) = create_with(&srv, &jwt, "it", json!(3)).await;
    let code = code_of(&created);
    let sid = created["session_id"].as_str().unwrap();
    assert_eq!(join(&srv, &code, "First", "es").await.0, StatusCode::OK);

    // The window the first guest had is long over; the conversation is under way, so the same
    // link still admits the next one.
    sqlx::query(
        "UPDATE live_sessions SET expires_at = now() - interval '1 hour' WHERE id = $1::uuid",
    )
    .bind(sid)
    .execute(&srv.pool)
    .await
    .unwrap();
    let (s, b) = join(&srv, &code, "Second", "en").await;
    assert_eq!(s, StatusCode::OK, "{b}");

    // Once it has ended, the link is dead, whatever seats were left.
    let tok = created["token"].as_str().unwrap();
    assert_eq!(
        srv.post(
            &format!("/api/live/sessions/{sid}/end"),
            Some(tok),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    let (s, b) = join(&srv, &code, "Third", "fr").await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::GONE, Some("session_ended"))
    );
}

#[tokio::test]
async fn a_guest_leaving_a_shared_conversation_does_not_end_it_for_the_others() {
    let srv = srv!(Opts::default());
    let (jwt, _) = login(&srv, "Marco").await;
    let (_, created) = create_with(&srv, &jwt, "it", json!(2)).await;
    let code = code_of(&created);
    let sid = created["session_id"].as_str().unwrap();
    let (_, a) = join(&srv, &code, "A", "es").await;
    let (_, b) = join(&srv, &code, "B", "en").await;

    let (s, left) = srv
        .post(
            &format!("/api/live/sessions/{sid}/end"),
            Some(a["token"].as_str().unwrap()),
            json!({}),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        (left["ended"].as_bool(), left["left"].as_bool()),
        (Some(false), Some(true))
    );
    let (s, view) = srv
        .get(
            &format!("/api/live/sessions/{sid}"),
            b["token"].as_str().unwrap(),
        )
        .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "the other guest's seat is still good: {view}"
    );
    assert_eq!(view["session"]["status"], "active");

    // The host's end is the end for everyone.
    let (_, ended) = srv
        .post(
            &format!("/api/live/sessions/{sid}/end"),
            Some(created["token"].as_str().unwrap()),
            json!({}),
        )
        .await;
    assert_eq!(ended["ended"], true);
    let (s, _) = srv
        .get(
            &format!("/api/live/sessions/{sid}"),
            b["token"].as_str().unwrap(),
        )
        .await;
    assert_eq!(s, StatusCode::GONE);
}

#[tokio::test]
async fn a_join_the_engine_cannot_serve_is_refused_busy_instead_of_a_direction_going_silent() {
    // Three upstream sessions in all. A pair needs two; a third language needs four more.
    let srv = srv!(Opts {
        qwen_max_sessions: Some(3),
        ..Opts::default()
    });
    let (jwt, _) = login(&srv, "Marco").await;
    let (_, created) = create_with(&srv, &jwt, "it", json!(3)).await;
    let code = code_of(&created);
    let sid = created["session_id"].as_str().unwrap();

    assert_eq!(join(&srv, &code, "Lucia", "es").await.0, StatusCode::OK);
    let (s, b) = join(&srv, &code, "Ann", "en").await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("live_busy")),
        "a third language would need more sessions than the engine has"
    );
    assert_eq!(
        guest_rows(&srv, sid).await,
        1,
        "the refused join took no seat"
    );

    // The same language as a guest already there needs no new stream: it still fits.
    assert_eq!(join(&srv, &code, "Pablo", "es").await.0, StatusCode::OK);
    assert_eq!(guest_rows(&srv, sid).await, 2);
}

#[tokio::test]
async fn a_business_session_takes_guests_too() {
    let srv = srv!(Opts::default());
    let (_, owner) = login(&srv, "Owner").await;
    let (jwt, member) = login(&srv, "Receptionist").await;
    let org = org_with_member(&srv, owner, Some(member)).await;

    let (s, created) = srv
        .post(
            "/api/live/sessions",
            Some(&jwt),
            json!({ "lang": "it", "org_id": org.id, "project_id": org.project, "max_guests": 2 }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{created}");
    assert_eq!(created["max_guests"], 2);
    let code = code_of(&created);
    assert_eq!(join(&srv, &code, "A", "es").await.0, StatusCode::OK);
    assert_eq!(join(&srv, &code, "B", "en").await.0, StatusCode::OK);
    assert_eq!(join(&srv, &code, "C", "fr").await.0, StatusCode::CONFLICT);
}
