//! The AI summary of a finished Live Connect conversation (spec 0121, S12).
//!
//! The rules under test: the summary is written from what was SAID, in order, not from its
//! translations; whoever may read a conversation may ask for its summary and nobody else learns
//! the conversation exists; the organisation pays exactly once per (conversation, language) — not
//! twice for a repeat, not twice for concurrent requests — and gets its money back when the
//! provider fails; a conversation with nothing said or still open costs nothing; a long
//! conversation loses its middle, never its end; and the summary is erased with the
//! conversation while the billing facts stay. The model is a local stand-in.
//!
//! DB-gated like the other integration suites. Run against the local pgvector Postgres:
//! `DATABASE_URL=postgres://vox:vox_local_dev@127.0.0.1:55432/voxtranslate_voip_test \
//!   cargo test --test live_summary`. A run of a few milliseconds skipped everything.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode as AxStatus;
use axum::routing::post;
use axum::{Json, Router};
use chrono::{DateTime, Duration as Span, Utc};
use reqwest::StatusCode;
use serde_json::{json, Value};
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::Config;
use voxtranslate_server::db::{self, Pool};
use voxtranslate_server::live::config::LiveConfig;
use voxtranslate_server::live::{erase, LiveState};
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::{app, AppState};

const SECRET: &str = "live-summary-secret";
const LIVE_KEY: &str = "0123456789abcdef0123456789abcdef-live-summary";
const SUMMARY_TEXT: &str = "## Key points\n- a double room was booked";

// ---- the model stand-in ----------------------------------------------------------------

struct MockGroq {
    calls: AtomicUsize,
    /// 200 answers with `reply`; anything else answers that status echoing the request.
    status: AtomicU16,
    delay_ms: AtomicU64,
    bodies: Mutex<Vec<Value>>,
}

impl MockGroq {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    fn fail(&self) {
        self.status.store(400, Ordering::SeqCst);
    }
    fn recover(&self) {
        self.status.store(200, Ordering::SeqCst);
    }
    fn delay(&self, ms: u64) {
        self.delay_ms.store(ms, Ordering::SeqCst);
    }
    /// What the model was last sent as the user message.
    fn last_user(&self) -> String {
        let bodies = self.bodies.lock().unwrap();
        bodies.last().expect("the model was called")["messages"][1]["content"]
            .as_str()
            .unwrap()
            .to_string()
    }
    fn last_system(&self) -> String {
        let bodies = self.bodies.lock().unwrap();
        bodies.last().expect("the model was called")["messages"][0]["content"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

async fn chat(State(m): State<Arc<MockGroq>>, Json(body): Json<Value>) -> (AxStatus, Json<Value>) {
    m.calls.fetch_add(1, Ordering::SeqCst);
    let user = body["messages"][1]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    m.bodies.lock().unwrap().push(body);
    let wait = m.delay_ms.load(Ordering::SeqCst);
    if wait > 0 {
        tokio::time::sleep(Duration::from_millis(wait)).await;
    }
    match m.status.load(Ordering::SeqCst) {
        200 => (
            AxStatus::OK,
            Json(json!({ "choices": [{ "message": { "content": SUMMARY_TEXT } }] })),
        ),
        // A provider error that quotes the request, as real ones sometimes do.
        status => (
            AxStatus::from_u16(status).unwrap(),
            Json(json!({ "error": { "message": format!("invalid request near: {user}") } })),
        ),
    }
}

async fn mock_groq() -> (String, Arc<MockGroq>) {
    let mock = Arc::new(MockGroq {
        calls: AtomicUsize::new(0),
        status: AtomicU16::new(200),
        delay_ms: AtomicU64::new(0),
        bodies: Mutex::new(Vec::new()),
    });
    let router = Router::new()
        .route("/chat/completions", post(chat))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{addr}/chat/completions"), mock)
}

// ---- the server ------------------------------------------------------------------------

struct Srv {
    addr: SocketAddr,
    pool: Pool,
    http: reqwest::Client,
    state: AppState,
    groq: Arc<MockGroq>,
}

async fn setup() -> Option<Srv> {
    let url = db::test_database_url()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let (groq_url, groq) = mock_groq().await;
    let mut config = Config::test_with_billing(&url, SECRET, 5.0);
    config.groq_base_url = Some(groq_url);
    let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);
    let mut state = AppState::new(config);
    state.billing = Some(BillingService::new(pool.clone(), min_join));
    state.safety = Some(SafetyService::new(pool.clone()));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);
    state.live = Arc::new(LiveState::new(
        LiveConfig::load(|k| match k {
            "LIVE_CONNECT_ENABLED" => Some("1".into()),
            "LIVE_TOKEN_KEY" => Some(LIVE_KEY.into()),
            _ => None,
        })
        .unwrap(),
    ));
    let served = state.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(served)).await;
    });
    Some(Srv {
        addr,
        pool,
        http: reqwest::Client::new(),
        state,
        groq,
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
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        bearer: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = self
            .http
            .request(method, format!("http://{}{path}", self.addr))
            .bearer_auth(bearer);
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
    async fn post(&self, path: &str, bearer: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.send(reqwest::Method::POST, path, bearer, body).await
    }
    async fn get(&self, path: &str, bearer: &str) -> (StatusCode, Value) {
        self.send(reqwest::Method::GET, path, bearer, None).await
    }
    async fn delete(&self, path: &str, bearer: &str) -> (StatusCode, Value) {
        self.send(reqwest::Method::DELETE, path, bearer, None).await
    }
    async fn balance(&self, org: Uuid) -> i32 {
        sqlx::query_scalar("SELECT credits_balance FROM organizations WHERE id = $1")
            .bind(org)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
    async fn set_balance(&self, org: Uuid, credits: i32) {
        sqlx::query("UPDATE organizations SET credits_balance = $2 WHERE id = $1")
            .bind(org)
            .bind(credits)
            .execute(&self.pool)
            .await
            .unwrap();
    }
    /// The org's `live_summary` ledger, oldest first: (amount, session_id, actor_id).
    async fn ledger(&self, org: Uuid) -> Vec<(i32, Option<Uuid>, Option<Uuid>)> {
        sqlx::query_as(
            "SELECT amount, session_id, actor_id FROM organization_credits_transactions
             WHERE org_id = $1 AND type = 'live_summary' ORDER BY created_at, amount",
        )
        .bind(org)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }
    async fn summaries(&self, session: Uuid) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM live_session_summaries WHERE session_id = $1")
            .bind(session)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
}

fn path(org: Uuid, session: Uuid) -> String {
    format!("/api/business/organizations/{org}/live/sessions/{session}/summary")
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
    (
        issue_jwt(SECRET, &user.id, &user.email, &user.name, 168).unwrap(),
        user.id,
    )
}

struct World {
    org: Uuid,
    owner: (String, Uuid),
    admin: (String, Uuid),
    /// A plain member who sat in the conversation, speaking French.
    sat: (String, Uuid),
    /// A plain member who did not.
    other: (String, Uuid),
    /// An admin of ANOTHER organisation.
    outsider: (String, Uuid),
    outsider_org: Uuid,
    /// Signed in, member of nothing.
    stranger: (String, Uuid),
}

async fn make_org(srv: &Srv, owner: Uuid, credits: i32) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id, credits_balance) VALUES ('Hotel', $1, $2, $3) RETURNING id",
    )
    .bind(format!("slug-{}", Uuid::new_v4().simple()))
    .bind(owner)
    .bind(credits)
    .fetch_one(&srv.pool)
    .await
    .unwrap()
}

async fn join(srv: &Srv, org: Uuid, user: Uuid, role: &str) {
    sqlx::query("INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, $3)")
        .bind(org)
        .bind(user)
        .bind(role)
        .execute(&srv.pool)
        .await
        .unwrap();
}

async fn world(srv: &Srv, credits: i32) -> World {
    let owner = login(srv, "Owner").await;
    let admin = login(srv, "Ada").await;
    let sat = login(srv, "Anna").await;
    let other = login(srv, "Bruno").await;
    let outsider = login(srv, "Olga").await;
    let stranger = login(srv, "Sam").await;
    let org = make_org(srv, owner.1, credits).await;
    for (u, role) in [
        (owner.1, "owner"),
        (admin.1, "admin"),
        (sat.1, "member"),
        (other.1, "member"),
    ] {
        join(srv, org, u, role).await;
    }
    let outsider_org = make_org(srv, outsider.1, 100).await;
    join(srv, outsider_org, outsider.1, "admin").await;
    World {
        org,
        owner,
        admin,
        sat,
        other,
        outsider,
        outsider_org,
        stranger,
    }
}

struct Seeded {
    session: Uuid,
    call: Uuid,
    guest_peer: Uuid,
    staff_peer: Uuid,
}

/// One business conversation as the live path leaves it, host language Italian, the guest
/// Spanish, the member of staff (`w.sat`) French.
async fn seed(srv: &Srv, w: &World, status: &str) -> Seeded {
    let session = Uuid::new_v4();
    let call = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO call_sessions (id, room, kind, org_id) VALUES ($1, $2, 'live_connect', $3)",
    )
    .bind(call)
    .bind(format!("live-{session}"))
    .bind(w.org)
    .execute(&srv.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO live_sessions (id, call_session_id, org_id, status, host_lang, expires_at, created_at, started_at, ended_at)
         VALUES ($1, $2, $3, $4, 'it', now() + interval '1 hour', now() - interval '10 minutes',
                 now() - interval '10 minutes',
                 CASE WHEN $4 = 'ended' THEN now() - interval '5 minutes' END)",
    )
    .bind(session)
    .bind(call)
    .bind(w.org)
    .bind(status)
    .execute(&srv.pool)
    .await
    .unwrap();
    let guest_peer: Uuid = sqlx::query_scalar(
        "INSERT INTO live_participants (session_id, role, display_name, lang) VALUES ($1, 'guest', 'Lucía', 'es') RETURNING peer_id",
    )
    .bind(session)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    let staff_peer: Uuid = sqlx::query_scalar(
        "INSERT INTO live_participants (session_id, role, user_id, display_name, lang) VALUES ($1, 'staff', $2, 'Anna', 'fr') RETURNING peer_id",
    )
    .bind(session)
    .bind(w.sat.1)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    Seeded {
        session,
        call,
        guest_peer,
        staff_peer,
    }
}

async fn says(
    srv: &Srv,
    s: &Seeded,
    guest: bool,
    lang: &str,
    text: &str,
    translations: Value,
    at: DateTime<Utc>,
) {
    let (peer, name) = if guest {
        (s.guest_peer, "Lucía")
    } else {
        (s.staff_peer, "Anna")
    };
    sqlx::query(
        "INSERT INTO transcript_events (session_id, event_type, speaker_peer_id, speaker_name, original_text, original_lang, translations, ts)
         VALUES ($1, 'speech', $2, $3, $4, $5, $6, $7)",
    )
    .bind(s.call)
    .bind(peer.to_string())
    .bind(name)
    .bind(text)
    .bind(lang)
    .bind(translations)
    .bind(at)
    .execute(&srv.pool)
    .await
    .unwrap();
}

/// A short exchange, inserted out of order on purpose (the summary reads by time, not by id).
async fn small_talk(srv: &Srv, s: &Seeded) {
    let t0 = Utc::now() - Span::minutes(9);
    says(
        srv,
        s,
        true,
        "es",
        "Necesito una habitación doble",
        json!({"it": "TRADUZIONE-IT-DUE"}),
        t0 + Span::seconds(20),
    )
    .await;
    says(
        srv,
        s,
        false,
        "it",
        "Buongiorno, come posso aiutarla?",
        json!({"es": "TRADUZIONE-ES-UNO"}),
        t0,
    )
    .await;
    says(
        srv,
        s,
        false,
        "it",
        "Certo, per quante notti?",
        json!({"es": "TRADUZIONE-ES-TRE"}),
        t0 + Span::seconds(40),
    )
    .await;
}

// ---- what the summary is made of -------------------------------------------------------

#[tokio::test]
async fn the_summary_is_written_from_the_originals_in_order_and_billed_to_the_org() {
    let srv = srv!();
    let w = world(&srv, 100).await;
    let s = seed(&srv, &w, "ended").await;
    small_talk(&srv, &s).await;

    let (status, body) = srv
        .post(
            &path(w.org, s.session),
            &w.admin.0,
            Some(json!({"lang": "en"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["summary"], SUMMARY_TEXT);
    assert_eq!(body["lang"], "en");
    assert_eq!(body["charged"], 3);
    assert_eq!(body["status"], "ready");

    // What the model read: the originals, by time, with who said them and in which language.
    let sent = srv.groq.last_user();
    let at = |needle: &str| {
        sent.find(needle)
            .unwrap_or_else(|| panic!("{needle:?} missing from {sent}"))
    };
    let (a, b, c) = (
        at("Buongiorno, come posso aiutarla?"),
        at("Necesito una habitación doble"),
        at("Certo, per quante notti?"),
    );
    assert!(a < b && b < c, "in the order they were said");
    assert!(sent.contains("Anna (it)") && sent.contains("Lucía (es)"));
    assert!(
        !sent.contains("TRADUZIONE"),
        "a translation is never the source"
    );
    assert!(srv.groq.last_system().contains("English"));

    // Billed to the org wallet, once, as its own ledger type, against the conversation.
    assert_eq!(srv.balance(w.org).await, 97);
    assert_eq!(
        srv.ledger(w.org).await,
        vec![(-3, Some(s.call), Some(w.admin.1))]
    );
}

#[tokio::test]
async fn without_a_language_it_is_the_callers_own_then_the_staff_sides() {
    let srv = srv!();
    let w = world(&srv, 100).await;
    let s = seed(&srv, &w, "ended").await;
    small_talk(&srv, &s).await;

    // The member who sat in it spoke French; the admin did not sit in it, so the host language.
    let (_, body) = srv.post(&path(w.org, s.session), &w.sat.0, None).await;
    assert_eq!(body["lang"], "fr");
    let (_, body) = srv.post(&path(w.org, s.session), &w.admin.0, None).await;
    assert_eq!(body["lang"], "it");

    let (status, body) = srv
        .post(
            &path(w.org, s.session),
            &w.admin.0,
            Some(json!({"lang": "not a language"})),
        )
        .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("bad_lang"))
    );
    let (status, body) = srv
        .post(
            &path(w.org, s.session),
            &w.admin.0,
            Some(json!({"lang": 5})),
        )
        .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("bad_body"))
    );
}

#[tokio::test]
async fn get_serves_what_was_stored_and_never_charges() {
    let srv = srv!();
    let w = world(&srv, 100).await;
    let s = seed(&srv, &w, "ended").await;
    small_talk(&srv, &s).await;

    let (status, body) = srv.get(&path(w.org, s.session), &w.admin.0).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("summary_not_found"))
    );

    srv.post(
        &path(w.org, s.session),
        &w.admin.0,
        Some(json!({"lang": "en"})),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    srv.post(
        &path(w.org, s.session),
        &w.admin.0,
        Some(json!({"lang": "it"})),
    )
    .await;
    assert_eq!(srv.balance(w.org).await, 94);

    let (status, body) = srv
        .get(&format!("{}?lang=en", path(w.org, s.session)), &w.admin.0)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (
            body["summary"].as_str(),
            body["lang"].as_str(),
            body["charged"].as_i64()
        ),
        (Some(SUMMARY_TEXT), Some("en"), Some(0))
    );
    // No language: the most recent one.
    let (_, body) = srv.get(&path(w.org, s.session), &w.admin.0).await;
    assert_eq!(body["lang"], "it");
    let (status, _) = srv
        .get(&format!("{}?lang=de", path(w.org, s.session)), &w.admin.0)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    assert_eq!(srv.balance(w.org).await, 94, "reading is free");
    assert_eq!(srv.groq.calls(), 2, "reading never calls the model");

    // The conversation detail says which languages exist.
    let (_, detail) = srv
        .get(
            &format!(
                "/api/business/organizations/{}/live/sessions/{}",
                w.org, s.session
            ),
            &w.admin.0,
        )
        .await;
    assert_eq!(detail["summary_langs"], json!(["en", "it"]));
}

// ---- who may ask -----------------------------------------------------------------------

#[tokio::test]
async fn a_conversation_you_may_not_see_cannot_be_summarised_and_looks_like_one_that_does_not_exist(
) {
    let srv = srv!();
    let w = world(&srv, 100).await;
    let s = seed(&srv, &w, "ended").await;
    small_talk(&srv, &s).await;
    let missing = Uuid::new_v4();

    let (_, absent) = srv.post(&path(w.org, missing), &w.other.0, None).await;
    for (who, name) in [(&w.other, "member who did not sit in it")] {
        let (status, body) = srv.post(&path(w.org, s.session), &who.0, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{name}");
        assert_eq!(
            body, absent,
            "{name}: identical to a conversation that does not exist"
        );
        let (status, body) = srv.get(&path(w.org, s.session), &who.0).await;
        assert_eq!(
            (status, &body),
            (StatusCode::NOT_FOUND, &absent),
            "{name}: GET"
        );
    }
    // Another organisation's admin, and someone in no organisation: the org itself is hidden.
    for who in [&w.outsider, &w.stranger] {
        let (status, _) = srv.post(&path(w.org, s.session), &who.0, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = srv.get(&path(w.org, s.session), &who.0).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    // The conversation's id under the OUTSIDER's own organisation is not found either.
    let (status, body) = srv
        .post(&path(w.outsider_org, s.session), &w.outsider.0, None)
        .await;
    assert_eq!((status, &body), (StatusCode::NOT_FOUND, &absent));
    let (status, _) = srv.post(&path(w.org, s.session), "not-a-token", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    assert_eq!(srv.groq.calls(), 0, "the model was never called");
    assert_eq!(srv.balance(w.org).await, 100, "nobody was charged");
    assert_eq!(srv.balance(w.outsider_org).await, 100);
    assert_eq!(srv.summaries(s.session).await, 0);
    assert!(srv.ledger(w.org).await.is_empty());

    // Whoever may read it, may summarise it: owner, admin, and the member who sat in it.
    for (who, lang) in [(&w.owner, "en"), (&w.admin, "it"), (&w.sat, "fr")] {
        let (status, body) = srv
            .post(&path(w.org, s.session), &who.0, Some(json!({"lang": lang})))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    // Once it exists, a member who did not sit in it still cannot read it.
    let (status, body) = srv.get(&path(w.org, s.session), &w.other.0).await;
    assert_eq!((status, &body), (StatusCode::NOT_FOUND, &absent));
}

// ---- money -----------------------------------------------------------------------------

#[tokio::test]
async fn a_second_request_for_the_same_language_returns_the_stored_summary_free() {
    let srv = srv!();
    let w = world(&srv, 100).await;
    let s = seed(&srv, &w, "ended").await;
    small_talk(&srv, &s).await;

    let (_, first) = srv
        .post(
            &path(w.org, s.session),
            &w.admin.0,
            Some(json!({"lang": "en"})),
        )
        .await;
    assert_eq!(first["charged"], 3);
    let (status, again) = srv
        .post(
            &path(w.org, s.session),
            &w.sat.0,
            Some(json!({"lang": "EN"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["summary"], first["summary"]);
    assert_eq!(
        again["charged"], 0,
        "a repeat is free (and the code is case-insensitive)"
    );
    assert_eq!(srv.groq.calls(), 1);
    assert_eq!(srv.balance(w.org).await, 97);
    assert_eq!(srv.ledger(w.org).await.len(), 1);

    // Another language is another summary, charged on its own.
    let (_, other) = srv
        .post(
            &path(w.org, s.session),
            &w.admin.0,
            Some(json!({"lang": "de"})),
        )
        .await;
    assert_eq!(other["charged"], 3);
    assert_eq!(srv.balance(w.org).await, 94);
    assert_eq!(srv.ledger(w.org).await.len(), 2);
    assert_eq!(srv.summaries(s.session).await, 2);
}

#[tokio::test]
async fn concurrent_requests_for_the_same_pair_charge_once_and_all_get_the_summary() {
    let srv = srv!();
    let w = world(&srv, 100).await;
    let s = seed(&srv, &w, "ended").await;
    small_talk(&srv, &s).await;
    srv.groq.delay(600);

    let mut jobs = Vec::new();
    for i in 0..6 {
        let http = srv.http.clone();
        let url = format!("http://{}{}", srv.addr, path(w.org, s.session));
        let token = if i % 2 == 0 {
            w.admin.0.clone()
        } else {
            w.owner.0.clone()
        };
        jobs.push(tokio::spawn(async move {
            let res = http
                .post(url)
                .bearer_auth(token)
                .json(&json!({"lang": "en"}))
                .send()
                .await
                .unwrap();
            let status = res.status();
            (status, res.json::<Value>().await.unwrap())
        }));
    }
    let mut charged = 0;
    for job in jobs {
        let (status, body) = job.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["summary"], SUMMARY_TEXT);
        charged += body["charged"].as_i64().unwrap();
    }
    assert_eq!(charged, 3, "exactly one request paid");
    assert_eq!(srv.groq.calls(), 1, "the model was asked once");
    assert_eq!(srv.balance(w.org).await, 97);
    assert_eq!(srv.ledger(w.org).await.len(), 1);
    assert_eq!(srv.summaries(s.session).await, 1);
}

#[tokio::test]
async fn a_provider_failure_refunds_stores_nothing_and_the_next_try_charges_once() {
    let srv = srv!();
    let w = world(&srv, 100).await;
    let s = seed(&srv, &w, "ended").await;
    small_talk(&srv, &s).await;

    srv.groq.fail();
    let (status, body) = srv
        .post(
            &path(w.org, s.session),
            &w.admin.0,
            Some(json!({"lang": "en"})),
        )
        .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::BAD_GATEWAY, Some("summary_unavailable"))
    );
    assert_eq!(srv.balance(w.org).await, 100, "refunded");
    let ledger = srv.ledger(w.org).await;
    assert_eq!(ledger.iter().map(|l| l.0).sum::<i32>(), 0, "{ledger:?}");
    assert_eq!(
        ledger.len(),
        2,
        "the charge and its refund are both on the books"
    );
    assert!(
        ledger.iter().all(|l| l.1 == Some(s.call)),
        "both against the conversation"
    );
    assert_eq!(
        srv.summaries(s.session).await,
        0,
        "no half-written claim is left behind"
    );
    assert_eq!(srv.groq.calls(), 2, "report model, then the fallback");

    srv.groq.recover();
    let (status, body) = srv
        .post(
            &path(w.org, s.session),
            &w.admin.0,
            Some(json!({"lang": "en"})),
        )
        .await;
    assert_eq!(
        (status, body["charged"].as_i64()),
        (StatusCode::OK, Some(3))
    );
    assert_eq!(srv.balance(w.org).await, 97);
}

#[tokio::test]
async fn an_org_that_cannot_pay_gets_402_and_nothing_is_stored_but_what_exists_stays_readable() {
    let srv = srv!();
    let w = world(&srv, 2).await;
    let s = seed(&srv, &w, "ended").await;
    small_talk(&srv, &s).await;

    let (status, body) = srv
        .post(
            &path(w.org, s.session),
            &w.admin.0,
            Some(json!({"lang": "en"})),
        )
        .await;
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(body["code"], "insufficient_credits");
    assert_eq!(
        (body["balance"].as_i64(), body["required"].as_i64()),
        (Some(2), Some(3))
    );
    assert_eq!(srv.groq.calls(), 0);
    assert_eq!(
        srv.summaries(s.session).await,
        0,
        "the claim was rolled back with the charge"
    );
    assert!(srv.ledger(w.org).await.is_empty());
    assert_eq!(srv.balance(w.org).await, 2);

    srv.set_balance(w.org, 3).await;
    let (status, _) = srv
        .post(
            &path(w.org, s.session),
            &w.admin.0,
            Some(json!({"lang": "en"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(srv.balance(w.org).await, 0);

    // Broke now: what was already paid for is still served, another language is not.
    let (status, body) = srv
        .post(
            &path(w.org, s.session),
            &w.admin.0,
            Some(json!({"lang": "en"})),
        )
        .await;
    assert_eq!(
        (status, body["charged"].as_i64()),
        (StatusCode::OK, Some(0))
    );
    let (status, _) = srv
        .post(
            &path(w.org, s.session),
            &w.admin.0,
            Some(json!({"lang": "de"})),
        )
        .await;
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
}

// ---- when there is nothing to summarise ------------------------------------------------

#[tokio::test]
async fn nothing_said_and_still_open_are_refused_without_a_charge() {
    let srv = srv!();
    let w = world(&srv, 100).await;

    let silent = seed(&srv, &w, "ended").await;
    // A typed chat message is not speech.
    sqlx::query("INSERT INTO transcript_events (session_id, event_type, speaker_peer_id, speaker_name, original_text, original_lang, ts) VALUES ($1, 'chat', 'p', 'Anna', 'a typed message, not speech', 'it', now())")
        .bind(silent.call)
        .execute(&srv.pool)
        .await
        .unwrap();
    let (status, body) = srv
        .post(&path(w.org, silent.session), &w.admin.0, None)
        .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::CONFLICT, Some("nothing_to_summarize"))
    );

    for open in ["waiting", "active"] {
        let s = seed(&srv, &w, open).await;
        small_talk(&srv, &s).await;
        let (status, body) = srv.post(&path(w.org, s.session), &w.admin.0, None).await;
        assert_eq!(
            (status, body["code"].as_str()),
            (StatusCode::CONFLICT, Some("session_open")),
            "{open}"
        );
    }
    assert_eq!(srv.groq.calls(), 0);
    assert_eq!(srv.balance(w.org).await, 100);
    assert!(srv.ledger(w.org).await.is_empty());
    assert_eq!(srv.summaries(silent.session).await, 0);
}

#[tokio::test]
async fn a_long_conversation_is_cut_in_the_middle_never_at_the_end() {
    let srv = srv!();
    let w = world(&srv, 100).await;
    let s = seed(&srv, &w, "ended").await;
    let t0 = Utc::now() - Span::hours(2);
    for i in 0..700 {
        let text = match i {
            0 => "FIRST-WORD-OF-THE-CALL".to_string(),
            699 => "LAST-WORD-OF-THE-CALL, see you tomorrow at nine".to_string(),
            _ => format!("MIDDLE-{i:04} {}", "blah ".repeat(20)),
        };
        says(
            &srv,
            &s,
            i % 2 == 0,
            "it",
            &text,
            json!({}),
            t0 + Span::seconds(i),
        )
        .await;
    }
    let (status, _) = srv
        .post(
            &path(w.org, s.session),
            &w.admin.0,
            Some(json!({"lang": "en"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let sent = srv.groq.last_user();
    assert!(sent.contains("FIRST-WORD-OF-THE-CALL"));
    assert!(sent.contains("LAST-WORD-OF-THE-CALL, see you tomorrow at nine"));
    assert!(sent.contains("utterances omitted from the middle"));
    assert!(!sent.contains("MIDDLE-0350"));
    assert!(
        sent.chars().count() < 31_000,
        "bounded: {}",
        sent.chars().count()
    );
}

// ---- erasure ---------------------------------------------------------------------------

async fn summarised(srv: &Srv, w: &World) -> Seeded {
    let s = seed(srv, w, "ended").await;
    small_talk(srv, &s).await;
    let (status, _) = srv
        .post(
            &path(w.org, s.session),
            &w.admin.0,
            Some(json!({"lang": "en"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(srv.summaries(s.session).await, 1);
    s
}

async fn session_rows(srv: &Srv, session: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM live_sessions WHERE id = $1")
        .bind(session)
        .fetch_one(&srv.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn deleting_a_conversation_deletes_its_summaries_and_keeps_the_billing_facts() {
    let srv = srv!();
    let w = world(&srv, 100).await;
    let s = summarised(&srv, &w).await;
    // A claim still pending goes too.
    sqlx::query("INSERT INTO live_session_summaries (session_id, lang, status, credits) VALUES ($1, 'de', 'pending', 3)")
        .bind(s.session)
        .execute(&srv.pool)
        .await
        .unwrap();

    let (status, _) = srv
        .delete(
            &format!(
                "/api/business/organizations/{}/live/sessions/{}",
                w.org, s.session
            ),
            &w.admin.0,
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(srv.summaries(s.session).await, 0);
    let (status, _) = srv.get(&path(w.org, s.session), &w.admin.0).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        session_rows(&srv, s.session).await,
        1,
        "the session row stays"
    );
    assert_eq!(
        srv.ledger(w.org).await.len(),
        1,
        "what the org paid stays on its books"
    );
    assert_eq!(srv.balance(w.org).await, 97);
    // Asking again finds nothing to summarise and charges nothing.
    let (status, body) = srv
        .post(
            &path(w.org, s.session),
            &w.admin.0,
            Some(json!({"lang": "en"})),
        )
        .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::CONFLICT, Some("nothing_to_summarize"))
    );
    assert_eq!(srv.balance(w.org).await, 97);
}

#[tokio::test]
async fn erase_content_removes_summaries_even_when_the_meeting_side_is_already_gone() {
    let srv = srv!();
    let w = world(&srv, 100).await;
    let s = summarised(&srv, &w).await;
    // The meeting-side rows vanished some other way; only the summary is left to find.
    sqlx::query("DELETE FROM call_sessions WHERE id = $1")
        .bind(s.call)
        .execute(&srv.pool)
        .await
        .unwrap();
    assert_eq!(srv.summaries(s.session).await, 1);

    let out = erase::erase_content(&srv.pool, s.session).await.unwrap();
    assert_eq!(out.call_sessions, 0);
    assert_eq!(srv.summaries(s.session).await, 0);
    let erased: i64 = sqlx::query_scalar("SELECT count(*) FROM live_session_events WHERE session_id = $1 AND kind = 'content_erased'")
        .bind(s.session)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert_eq!(
        erased, 1,
        "erasing a summary is recorded even when it is all there was"
    );
}

#[tokio::test]
async fn retention_erases_summaries_with_the_conversation_and_finds_orphaned_ones() {
    let srv = srv!();
    let w = world(&srv, 100).await;
    sqlx::query(
        "UPDATE organizations SET settings = jsonb_build_object('retention_days', 1) WHERE id = $1",
    )
    .bind(w.org)
    .execute(&srv.pool)
    .await
    .unwrap();
    let old = summarised(&srv, &w).await;
    let fresh = summarised(&srv, &w).await;
    let orphan = summarised(&srv, &w).await;
    for s in [&old, &orphan] {
        sqlx::query("UPDATE live_sessions SET ended_at = now() - interval '3 days' WHERE id = $1")
            .bind(s.session)
            .execute(&srv.pool)
            .await
            .unwrap();
    }
    // The meeting side is gone and the guest's name was already scrubbed: the summary is all
    // that is left of what was said.
    sqlx::query("UPDATE live_participants SET display_name = 'Erased', token_hash = NULL WHERE session_id = $1")
        .bind(orphan.session)
        .execute(&srv.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM call_sessions WHERE id = $1")
        .bind(orphan.call)
        .execute(&srv.pool)
        .await
        .unwrap();

    let n = erase::retention_pass(
        &srv.state,
        Some(&[old.session, fresh.session, orphan.session]),
    )
    .await
    .unwrap();
    assert_eq!(n, 2);
    assert_eq!(srv.summaries(old.session).await, 0);
    assert_eq!(
        srv.summaries(orphan.session).await,
        0,
        "found by its summary alone"
    );
    assert_eq!(srv.summaries(fresh.session).await, 1, "inside its window");
    assert_eq!(srv.ledger(w.org).await.len(), 3, "billing facts untouched");
}

#[tokio::test]
async fn deleting_an_account_that_owned_a_conversation_erases_its_summaries_and_keeps_the_ledger() {
    let srv = srv!();
    let w = world(&srv, 100).await;
    let s = summarised(&srv, &w).await;
    sqlx::query("UPDATE live_sessions SET owner_user_id = $2 WHERE id = $1")
        .bind(s.session)
        .bind(w.sat.1)
        .execute(&srv.pool)
        .await
        .unwrap();

    srv.state
        .safety
        .as_ref()
        .unwrap()
        .delete_user(w.sat.1)
        .await
        .unwrap();
    assert_eq!(srv.summaries(s.session).await, 0);
    assert_eq!(srv.ledger(w.org).await.len(), 1, "the org's ledger stays");
}

#[tokio::test]
async fn deleting_the_account_that_asked_for_a_summary_keeps_the_summary_of_the_org() {
    let srv = srv!();
    let w = world(&srv, 100).await;
    let s = summarised(&srv, &w).await;

    srv.state
        .safety
        .as_ref()
        .unwrap()
        .delete_user(w.admin.1)
        .await
        .unwrap();
    assert_eq!(
        srv.summaries(s.session).await,
        1,
        "the conversation is the org's, not the member's"
    );
    let by: Option<Uuid> =
        sqlx::query_scalar("SELECT created_by FROM live_session_summaries WHERE session_id = $1")
            .bind(s.session)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(by, None);
    assert_eq!(srv.ledger(w.org).await.len(), 1);
}

// ---- crash recovery --------------------------------------------------------------------

#[tokio::test]
async fn a_claim_abandoned_by_a_crashed_request_is_taken_over_without_charging_again() {
    let srv = srv!();
    let w = world(&srv, 100).await;
    let s = seed(&srv, &w, "ended").await;
    small_talk(&srv, &s).await;
    // Charged and claimed, then the process died: a pending row and a debit, no summary.
    sqlx::query("INSERT INTO live_session_summaries (session_id, lang, status, credits, created_by, created_at) VALUES ($1, 'en', 'pending', 3, $2, now() - interval '20 minutes')")
        .bind(s.session)
        .bind(w.admin.1)
        .execute(&srv.pool)
        .await
        .unwrap();
    srv.set_balance(w.org, 97).await;

    let (status, body) = srv
        .post(
            &path(w.org, s.session),
            &w.admin.0,
            Some(json!({"lang": "en"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["charged"], 0, "the debit is already on the books");
    assert_eq!(srv.balance(w.org).await, 97);
    assert_eq!(srv.groq.calls(), 1);
    let (_, got) = srv.get(&path(w.org, s.session), &w.admin.0).await;
    assert_eq!(got["summary"], SUMMARY_TEXT);
}

#[tokio::test]
async fn a_fresh_claim_is_waited_for_not_stolen() {
    let srv = srv!();
    let w = world(&srv, 100).await;
    let s = seed(&srv, &w, "ended").await;
    small_talk(&srv, &s).await;
    sqlx::query("INSERT INTO live_session_summaries (session_id, lang, status, credits, created_by) VALUES ($1, 'en', 'pending', 3, $2)")
        .bind(s.session)
        .bind(w.admin.1)
        .execute(&srv.pool)
        .await
        .unwrap();

    // The writer "finishes" half a second later.
    let pool = srv.pool.clone();
    let session = s.session;
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        sqlx::query("UPDATE live_session_summaries SET status = 'ready', summary = 'written by the other request' WHERE session_id = $1")
            .bind(session)
            .execute(&pool)
            .await
            .unwrap();
    });
    let (status, body) = srv
        .post(
            &path(w.org, s.session),
            &w.owner.0,
            Some(json!({"lang": "en"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["summary"], "written by the other request");
    assert_eq!(body["charged"], 0);
    assert_eq!(srv.groq.calls(), 0, "the model was not asked a second time");

    // While it is pending, GET says so.
    sqlx::query("UPDATE live_session_summaries SET status = 'pending' WHERE session_id = $1")
        .bind(s.session)
        .execute(&srv.pool)
        .await
        .unwrap();
    let (status, body) = srv
        .get(&format!("{}?lang=en", path(w.org, s.session)), &w.admin.0)
        .await;
    assert_eq!(
        (status, body["status"].as_str()),
        (StatusCode::ACCEPTED, Some("pending"))
    );
}

// ---- what is recorded and what is logged -----------------------------------------------

async fn audit_rows(srv: &Srv, session: Uuid) -> Vec<Value> {
    for _ in 0..50 {
        let rows: Vec<Value> = sqlx::query_scalar(
            "SELECT metadata FROM audit_logs WHERE resource_id = $1 AND action = 'live.summary'",
        )
        .bind(session)
        .fetch_all(&srv.pool)
        .await
        .unwrap();
        if !rows.is_empty() {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Vec::new()
}

#[tokio::test]
async fn generating_is_audited_with_the_shape_of_the_request_and_none_of_the_content() {
    let srv = srv!();
    let w = world(&srv, 100).await;
    let s = seed(&srv, &w, "ended").await;
    small_talk(&srv, &s).await;
    srv.post(
        &path(w.org, s.session),
        &w.admin.0,
        Some(json!({"lang": "en"})),
    )
    .await;

    let rows = audit_rows(&srv, s.session).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0],
        json!({"lang": "en", "credits": 3, "utterances": 3, "omitted": 0})
    );
    let text = rows[0].to_string();
    for said in ["habitación", "Buongiorno", "notti", "double room"] {
        assert!(
            !text.contains(said),
            "the audit entry quotes the conversation: {text}"
        );
    }
    // A repeat reads the store and is not a second generation.
    srv.post(
        &path(w.org, s.session),
        &w.admin.0,
        Some(json!({"lang": "en"})),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(audit_rows(&srv, s.session).await.len(), 1);
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Everything this test binary logs, at DEBUG, kept in memory. A process-wide subscriber, not a
/// scoped one: tests run in parallel threads, and tracing caches per call site whether anybody is
/// listening, so a scoped subscriber can miss events a parallel test registered first.
fn logs() -> &'static Capture {
    static LOGS: std::sync::OnceLock<Capture> = std::sync::OnceLock::new();
    LOGS.get_or_init(|| {
        let sink = Capture::default();
        let writer = sink.clone();
        let _ = tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::DEBUG)
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish(),
        );
        sink
    })
}

#[tokio::test]
async fn the_conversation_never_reaches_the_logs_even_when_the_provider_quotes_it() {
    let sink = logs();
    let srv = srv!();
    let w = world(&srv, 100).await;
    let s = seed(&srv, &w, "ended").await;
    small_talk(&srv, &s).await;

    srv.groq.fail();
    let (status, _) = srv
        .post(
            &path(w.org, s.session),
            &w.admin.0,
            Some(json!({"lang": "en"})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    srv.groq.recover();
    let (status, _) = srv
        .post(
            &path(w.org, s.session),
            &w.admin.0,
            Some(json!({"lang": "en"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let logs = String::from_utf8_lossy(&sink.0.lock().unwrap()).into_owned();
    assert!(
        logs.lines()
            .any(|l| l.contains("provider failed") && l.contains(&s.session.to_string())),
        "the failure itself is logged"
    );
    for said in [
        "habitación",
        "Buongiorno",
        "notti",
        "TRADUZIONE",
        "invalid request near",
    ] {
        assert!(
            !logs.contains(said),
            "{said:?} leaked into the logs:\n{logs}"
        );
    }
}

// ---- several guests in one conversation (spec 0121, S1) --------------------------------

#[tokio::test]
async fn a_summary_of_a_conversation_with_several_guests_reads_every_speaker_in_their_language() {
    let srv = srv!();
    let w = world(&srv, 100).await;
    let s = seed(&srv, &w, "ended").await;
    small_talk(&srv, &s).await;
    // Two more guests, one of them in a language already there.
    let t0 = Utc::now() - Span::minutes(9);
    for (i, (name, lang, text)) in [
        ("Ben", "en", "Is breakfast included?"),
        ("Pablo", "es", "Y el aparcamiento?"),
    ]
    .into_iter()
    .enumerate()
    {
        let peer: Uuid = sqlx::query_scalar(
            "INSERT INTO live_participants (session_id, role, display_name, lang, joined_at)
             VALUES ($1, 'guest', $2, $3, now() + make_interval(mins => $4)) RETURNING peer_id",
        )
        .bind(s.session)
        .bind(name)
        .bind(lang)
        .bind(i as i32 + 1)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO transcript_events (session_id, event_type, speaker_peer_id, speaker_name, original_text, original_lang, translations, ts)
             VALUES ($1, 'speech', $2, $3, $4, $5, '{}'::jsonb, $6)",
        )
        .bind(s.call)
        .bind(peer.to_string())
        .bind(name)
        .bind(text)
        .bind(lang)
        .bind(t0 + Span::seconds(25 + i as i64 * 5))
        .execute(&srv.pool)
        .await
        .unwrap();
    }

    // The conversation is readable (a scalar subquery over "the" guest would have failed here)
    // and everything each of the four said reaches the model, with who and in which language.
    let (status, body) = srv
        .post(
            &path(w.org, s.session),
            &w.admin.0,
            Some(json!({"lang": "en"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let sent = srv.groq.last_user();
    for needle in [
        "Anna (it)",
        "Lucía (es)",
        "Ben (en)",
        "Pablo (es)",
        "Is breakfast included?",
        "Y el aparcamiento?",
    ] {
        assert!(sent.contains(needle), "{needle:?} missing from {sent}");
    }
    // The member who sat in it still gets a summary in their own language.
    let (_, mine) = srv.post(&path(w.org, s.session), &w.sat.0, None).await;
    assert_eq!(mine["lang"], "fr");
}
