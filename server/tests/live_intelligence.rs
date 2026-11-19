//! Live Connect conversation intelligence (spec 0121, S12/S13/S17): a finished conversation
//! becomes a searchable record, and the content of a conversation can be erased — on request,
//! on a schedule, and with the account of the person who owns it.
//!
//! Most conversations here are written straight into the tables the socket would have written
//! (`call_sessions`, `transcript_events`, `session_participants`, `live_*`): what is under test is
//! what happens AFTER the conversation, and that is a function of those rows. Each way a
//! conversation can end is then driven for real, to prove it leads to finalisation.
//!
//! Embeddings cannot be produced without the provider, so the search tests stop one step short of
//! it: finalisation is real, the vectors are hand-made, and the SQL under test (scope, filters) is
//! the SQL the endpoint runs.
//!
//! DB-gated like the other live suites. Run against the local pgvector Postgres:
//! `DATABASE_URL=postgres://vox:vox_local_dev@127.0.0.1:55432/voxtranslate_voip_test \
//!   cargo test --test live_intelligence`. A run of a few milliseconds skipped everything.

use std::collections::BTreeMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use reqwest::StatusCode;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::business::search::{self, Filters, Visible};
use voxtranslate_server::business::transcripts::Segment;
use voxtranslate_server::config::{Config, QwenConfig};
use voxtranslate_server::db::{self, Pool};
use voxtranslate_server::engine::realtime_mock::{Dialect, RealtimeMock, Reply};
use voxtranslate_server::live::config::LiveConfig;
use voxtranslate_server::live::finalize::{self, Outcome};
use voxtranslate_server::live::sweeper::{run_maintenance, sweep_sessions};
use voxtranslate_server::live::tokens::PeerRole;
use voxtranslate_server::live::{erase, sessions, LiveState};
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::transcripts::TranscriptService;
use voxtranslate_server::{app, AppState};

const SECRET: &str = "live-intelligence-secret";
const LIVE_KEY: &str = "0123456789abcdef0123456789abcdef-live-intel";

struct Srv {
    addr: SocketAddr,
    pool: Pool,
    http: reqwest::Client,
    state: AppState,
    _mock: RealtimeMock,
}

/// `env` adds Live Connect settings. The grace before finalising is 0 here, so a test does not
/// sit through the production 5 s; the reconnect grace is the shortest the config allows.
async fn setup(env: &'static [(&'static str, &'static str)]) -> Option<Srv> {
    let url = db::test_database_url()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(vec![
        Reply::Transcript {
            original: "ciao a tutti".into(),
            translated: "hola a todos".into(),
        },
        Reply::Audio(vec![7u8; 960]),
    ]);
    let mut config = Config::test_with_billing(&url, SECRET, 5.0);
    config.qwen = QwenConfig {
        api_key: "test-key".into(),
        endpoint: mock.base_url(),
        ..Default::default()
    };
    config.standard_enabled = true;
    // `@cost_per_minute` prices the engine for the test that needs a conversation to run out of
    // credit within seconds; it is not a Live Connect setting.
    if let Some((_, v)) = env.iter().find(|(k, _)| *k == "@cost_per_minute") {
        config.qwen.cost_per_minute = v.parse().unwrap();
    }
    {
        let pricing = &mut config.billing.as_mut().unwrap().pricing;
        pricing.usage_update_interval = 1;
    }
    let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);
    let mut state = AppState::new(config);
    state.billing = Some(BillingService::new(pool.clone(), min_join));
    state.safety = Some(SafetyService::new(pool.clone()));
    state.transcripts = Some(TranscriptService::new(pool.clone()));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);
    state.live = Arc::new(LiveState::new(
        LiveConfig::load(|k| {
            if let Some((_, v)) = env.iter().find(|(name, _)| *name == k) {
                return Some((*v).to_string());
            }
            match k {
                "LIVE_CONNECT_ENABLED" => Some("1".into()),
                "LIVE_TOKEN_KEY" => Some(LIVE_KEY.into()),
                "LIVE_FINALIZE_GRACE_SECS" => Some("0".into()),
                "LIVE_RECONNECT_GRACE_SECS" => Some("5".into()),
                _ => None,
            }
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
        _mock: mock,
    })
}

macro_rules! srv {
    () => {
        srv!(&[])
    };
    ($env:expr) => {
        match setup($env).await {
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
            "DELETE" => self.http.delete(url),
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
    async fn delete(&self, path: &str, bearer: Option<&str>) -> (StatusCode, Value) {
        self.call("DELETE", path, bearer, None).await
    }
    async fn get(&self, path: &str, bearer: &str) -> (StatusCode, Value) {
        self.call("GET", path, Some(bearer), None).await
    }
}

/// Poll until `f` yields a value (the recorder and the finaliser run in the background).
async fn eventually<T, Fut: Future<Output = Option<T>>>(
    what: &str,
    mut f: impl FnMut() -> Fut,
) -> T {
    for _ in 0..80 {
        if let Some(v) = f().await {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("timed out waiting for {what}");
}

// ---- people and organisations ----------------------------------------------------------

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
    owner: Uuid,
    member_jwt: String,
    member: Uuid,
}

/// An org with credit, one project, an owner (admin rank) and a member (the person at the desk).
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
        owner,
        member_jwt,
        member,
    }
}

/// A second member of `org` who never created or took part in anything.
async fn bystander(srv: &Srv, org: &Org) -> (String, Uuid) {
    let (jwt, id) = login(srv, "Bystander").await;
    sqlx::query(
        "INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, 'member')",
    )
    .bind(org.id)
    .bind(id)
    .execute(&srv.pool)
    .await
    .unwrap();
    (jwt, id)
}

// ---- seeded conversations ---------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Who {
    Staff,
    Guest,
}

struct Speech {
    who: Who,
    text: &'static str,
    lang: &'static str,
    /// Seconds after the conversation started.
    at: i64,
}

fn say(who: Who, text: &'static str, lang: &'static str, at: i64) -> Speech {
    Speech {
        who,
        text,
        lang,
        at,
    }
}

enum Owner<'a> {
    Org(&'a Org),
    User(Uuid),
}

struct Conv {
    live: Uuid,
    staff_peer: Uuid,
    guest_peer: Uuid,
    /// Oldest first; the last one is what `live_sessions.call_session_id` points at.
    calls: Vec<Uuid>,
}

const STAFF_NAME: &str = "Receptionist";
const GUEST_NAME: &str = "Anna Guest";

/// Write a conversation the way the socket does: one `call_sessions` row per room the
/// conversation lived through (recorded as `room_session` events, the live row pointing at the
/// last), participants, and one `speech` event per utterance with its translation beside it.
/// `ended_days_ago` positions the end of an ended conversation; an active one is "now".
async fn seed(
    srv: &Srv,
    owner: Owner<'_>,
    status: &str,
    ended_days_ago: i32,
    rooms: &[Vec<Speech>],
) -> Conv {
    let pool = &srv.pool;
    let (org_id, project_id, owner_user, staff_user) = match &owner {
        Owner::Org(o) => (Some(o.id), Some(o.project), None, o.member),
        Owner::User(u) => (None, None, Some(*u), *u),
    };
    let live: Uuid = sqlx::query_scalar(
        "INSERT INTO live_sessions
             (org_id, project_id, owner_user_id, status, host_lang, save_transcript, expires_at,
              created_at, started_at, ended_at, end_reason)
         SELECT $1, $2, $3, $4, 'it', TRUE, now(),
                r - interval '10 minutes', r - interval '5 minutes',
                CASE WHEN $4 IN ('ended', 'expired') THEN r END,
                CASE WHEN $4 IN ('ended', 'expired') THEN 'ended_by_staff' END
         FROM (SELECT now() - make_interval(days => $5) AS r) t
         RETURNING id",
    )
    .bind(org_id)
    .bind(project_id)
    .bind(owner_user)
    .bind(status)
    .bind(ended_days_ago)
    .fetch_one(pool)
    .await
    .unwrap();

    let owner_role = if org_id.is_some() { "staff" } else { "host" };
    let staff_peer: Uuid = sqlx::query_scalar(
        "INSERT INTO live_participants (session_id, role, user_id, display_name, lang, token_hash, consent_at)
         VALUES ($1, $2, $3, $4, 'it', '\\xdeadbeef', now()) RETURNING peer_id",
    )
    .bind(live)
    .bind(owner_role)
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

    let mut calls = Vec::new();
    for room in rooms {
        let call = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO call_sessions (id, room, kind, org_id, project_id, started_at)
             SELECT $1, $2, 'live_connect', $3, $4, started_at FROM live_sessions WHERE id = $5",
        )
        .bind(call)
        .bind(format!("live-{live}"))
        .bind(org_id)
        .bind(project_id)
        .bind(live)
        .execute(pool)
        .await
        .unwrap();
        for (peer, user, name, lang) in [
            (staff_peer, Some(staff_user), STAFF_NAME, "it"),
            (guest_peer, None, GUEST_NAME, "es"),
        ] {
            sqlx::query(
                "INSERT INTO session_participants (session_id, peer_id, user_id, name, lang)
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(call)
            .bind(peer.to_string())
            .bind(user)
            .bind(name)
            .bind(lang)
            .execute(pool)
            .await
            .unwrap();
        }
        for sp in room {
            let (peer, user, name) = match sp.who {
                Who::Staff => (staff_peer, Some(staff_user), STAFF_NAME),
                Who::Guest => (guest_peer, None, GUEST_NAME),
            };
            let other = if sp.lang == "es" { "it" } else { "es" };
            sqlx::query(
                "INSERT INTO transcript_events
                     (session_id, event_type, speaker_peer_id, speaker_user_id, speaker_name,
                      original_text, original_lang, translations, ts)
                 SELECT $1, 'speech', $2, $3, $4, $5, $6, $7, started_at + make_interval(secs => $8)
                 FROM live_sessions WHERE id = $9",
            )
            .bind(call)
            .bind(peer.to_string())
            .bind(user)
            .bind(name)
            .bind(sp.text)
            .bind(sp.lang)
            .bind(json!({ other: format!("[{other}] {}", sp.text) }))
            .bind(sp.at as f64)
            .bind(live)
            .execute(pool)
            .await
            .unwrap();
        }
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
        calls.push(call);
    }

    // Billing facts: two metered ticks and an org ledger row tied to the conversation.
    for seq in [1i64, 2] {
        sqlx::query(
            "INSERT INTO live_usage_ticks (session_id, peer_id, tick_seq, payer_kind, amount)
             VALUES ($1, $2, $3, $4, 2)",
        )
        .bind(live)
        .bind(staff_peer)
        .bind(seq)
        .bind(if org_id.is_some() { "org" } else { "user" })
        .execute(pool)
        .await
        .unwrap();
    }
    if let (Some(org_id), Some(call)) = (org_id, calls.last()) {
        sqlx::query(
            "INSERT INTO organization_credits_transactions (org_id, amount, type, description, session_id)
             VALUES ($1, -2, 'translation', $2, $3)",
        )
        .bind(org_id)
        .bind(format!("seed {live}"))
        .bind(call)
        .execute(pool)
        .await
        .unwrap();
    }
    Conv {
        live,
        staff_peer,
        guest_peer,
        calls,
    }
}

/// A short exchange: Italian from the desk, Spanish from the guest, Italian again.
fn exchange() -> Vec<Speech> {
    vec![
        say(Who::Staff, "Buongiorno, come posso aiutarla?", "it", 5),
        say(
            Who::Guest,
            "Quiero reservar una habitación para dos noches",
            "es",
            12,
        ),
        say(Who::Staff, "Certo, per quali date?", "it", 20),
    ]
}

/// Row counts for everything a conversation can leave behind, keyed by what the test knows.
async fn counts(srv: &Srv, c: &Conv, org: Option<Uuid>) -> BTreeMap<&'static str, i64> {
    let mut out = BTreeMap::new();
    let by_calls = [
        (
            "call_sessions",
            "SELECT count(*) FROM call_sessions WHERE id = ANY($1)",
        ),
        (
            "session_participants",
            "SELECT count(*) FROM session_participants WHERE session_id = ANY($1)",
        ),
        (
            "transcript_events",
            "SELECT count(*) FROM transcript_events WHERE session_id = ANY($1)",
        ),
        (
            "transcripts",
            "SELECT count(*) FROM transcripts WHERE session_id = ANY($1)",
        ),
        (
            "transcript_embeddings",
            "SELECT count(*) FROM transcript_embeddings WHERE session_id = ANY($1)",
        ),
    ];
    for (name, sql) in by_calls {
        let n: i64 = sqlx::query_scalar(sql)
            .bind(&c.calls)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
        out.insert(name, n);
    }
    let by_live = [
        (
            "live_sessions",
            "SELECT count(*) FROM live_sessions WHERE id = $1",
        ),
        (
            "live_participants",
            "SELECT count(*) FROM live_participants WHERE session_id = $1",
        ),
        (
            "live_usage_ticks",
            "SELECT count(*) FROM live_usage_ticks WHERE session_id = $1",
        ),
        (
            "live_session_events",
            "SELECT count(*) FROM live_session_events WHERE session_id = $1",
        ),
    ];
    for (name, sql) in by_live {
        let n: i64 = sqlx::query_scalar(sql)
            .bind(c.live)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
        out.insert(name, n);
    }
    if let Some(org) = org {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM organization_credits_transactions WHERE org_id = $1 AND description = $2",
        )
        .bind(org)
        .bind(format!("seed {}", c.live))
        .fetch_one(&srv.pool)
        .await
        .unwrap();
        out.insert("ledger", n);
    }
    out
}

struct TranscriptRow {
    session_id: Uuid,
    org_id: Uuid,
    source_language: String,
    segments: Vec<Segment>,
    translations: Value,
    duration_seconds: Option<i32>,
    word_count: Option<i32>,
    processed: bool,
}

#[allow(clippy::type_complexity)] // one SELECT row, read once
async fn transcripts_of(srv: &Srv, calls: &[Uuid]) -> Vec<TranscriptRow> {
    let rows: Vec<(
        Uuid,
        Uuid,
        String,
        Value,
        Value,
        Option<i32>,
        Option<i32>,
        bool,
    )> = sqlx::query_as(
        "SELECT session_id, org_id, source_language, segments, translations,
                    duration_seconds, word_count, processed_at IS NOT NULL
             FROM transcripts WHERE session_id = ANY($1)",
    )
    .bind(calls)
    .fetch_all(&srv.pool)
    .await
    .unwrap();
    rows.into_iter()
        .map(|r| TranscriptRow {
            session_id: r.0,
            org_id: r.1,
            source_language: r.2,
            segments: serde_json::from_value(r.3).unwrap(),
            translations: r.4,
            duration_seconds: r.5,
            word_count: r.6,
            processed: r.7,
        })
        .collect()
}

async fn status_of(srv: &Srv, call: Uuid) -> String {
    sqlx::query_scalar("SELECT transcript_status FROM call_sessions WHERE id = $1")
        .bind(call)
        .fetch_one(&srv.pool)
        .await
        .unwrap()
}

fn dummy_vector() -> Vec<f32> {
    vec![0.1f32; 1536]
}

/// What the embedding step stores, minus the provider: chunk the transcript exactly as
/// `finalize` does and give every chunk the same vector.
async fn index_without_provider(srv: &Srv, org: &Org, call: Uuid) -> usize {
    let (transcript_id, segments): (Uuid, Value) =
        sqlx::query_as("SELECT id, segments FROM transcripts WHERE session_id = $1")
            .bind(call)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    let segments: Vec<Segment> = serde_json::from_value(segments).unwrap();
    let chunks = finalize::turn_chunks(&segments);
    let vectors = chunks.iter().map(|_| dummy_vector()).collect();
    finalize::store_chunks(
        &srv.pool,
        transcript_id,
        call,
        org.id,
        Some(org.project),
        &chunks,
        vectors,
    )
    .await
    .unwrap()
}

fn org_session_path(org: &Org, live: Uuid) -> String {
    format!(
        "/api/business/organizations/{}/live/sessions/{live}",
        org.id
    )
}

fn token_for(
    srv: &Srv,
    live: Uuid,
    peer: Uuid,
    role: PeerRole,
    created: chrono::DateTime<chrono::Utc>,
) -> String {
    sessions::issue_token(&srv.state.live.config, live, peer, role, created).unwrap()
}

async fn created_at(srv: &Srv, live: Uuid) -> chrono::DateTime<chrono::Utc> {
    sqlx::query_scalar("SELECT created_at FROM live_sessions WHERE id = $1")
        .bind(live)
        .fetch_one(&srv.pool)
        .await
        .unwrap()
}

// ===== S12: a finished conversation becomes one transcript ===============================

#[tokio::test]
async fn finalising_builds_one_transcript_with_both_speakers_kept_apart_and_the_originals_intact() {
    let srv = srv!();
    let org = org(&srv).await;
    let c = seed(&srv, Owner::Org(&org), "ended", 0, &[exchange()]).await;
    let call = c.calls[0];

    let out = finalize::finalize(&srv.state, c.live).await.unwrap();
    assert!(
        matches!(out, Outcome::Finalized { segments: 3, .. }),
        "{out:?}"
    );

    let rows = transcripts_of(&srv, &c.calls).await;
    assert_eq!(rows.len(), 1);
    let t = &rows[0];
    assert_eq!((t.session_id, t.org_id), (call, org.id));
    assert!(t.processed, "processed_at is set");
    assert_eq!(
        t.source_language, "it",
        "the language spoken most often in the conversation"
    );
    assert_eq!(t.translations, json!({}), "translations stay on the events");
    assert_eq!(t.word_count, Some(4 + 7 + 4), "words of the ORIGINAL texts");
    // Ended 5 minutes after it started: that is the session duration, not the speech span.
    assert_eq!(t.duration_seconds, Some(300));

    // Both speakers, told apart by the participant id and name the room used, in order of speech.
    let who: Vec<(&str, &str, &str)> = t
        .segments
        .iter()
        .map(|s| {
            (
                s.speaker_id.as_str(),
                s.speaker_name.as_str(),
                s.text.as_str(),
            )
        })
        .collect();
    assert_eq!(
        who,
        vec![
            (
                c.staff_peer.to_string().as_str(),
                STAFF_NAME,
                "Buongiorno, come posso aiutarla?"
            ),
            (
                c.guest_peer.to_string().as_str(),
                GUEST_NAME,
                "Quiero reservar una habitación para dos noches"
            ),
            (
                c.staff_peer.to_string().as_str(),
                STAFF_NAME,
                "Certo, per quali date?"
            ),
        ]
    );
    // Timestamps are relative to the first utterance; each segment has a positive length.
    assert_eq!(t.segments[0].start_ms, 0);
    assert_eq!(t.segments[1].start_ms, 7_000);
    assert_eq!(t.segments[2].start_ms, 15_000);
    assert!(t.segments.iter().all(|s| s.end_ms > s.start_ms));

    assert_eq!(status_of(&srv, call).await, "ready");

    // The events are untouched: original and translation still sit in their own columns.
    let ev: Vec<(String, String, Value)> = sqlx::query_as(
        "SELECT original_text, original_lang, translations FROM transcript_events
         WHERE session_id = $1 ORDER BY ts",
    )
    .bind(call)
    .fetch_all(&srv.pool)
    .await
    .unwrap();
    assert_eq!(ev.len(), 3);
    assert_eq!(ev[1].0, "Quiero reservar una habitación para dos noches");
    assert_eq!(ev[1].1, "es");
    assert_eq!(
        ev[1].2,
        json!({ "it": "[it] Quiero reservar una habitación para dos noches" })
    );
}

#[tokio::test]
async fn finalising_twice_or_at_the_same_time_leaves_exactly_one_transcript() {
    let srv = srv!();
    let org = org(&srv).await;
    let c = seed(&srv, Owner::Org(&org), "ended", 0, &[exchange()]).await;

    // Four finalisers at once (every ending path spawns one): one wins, the rest see it done.
    let results = futures::future::join_all((0..4).map(|_| finalize::finalize(&srv.state, c.live)))
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect::<Vec<_>>();
    let won = results
        .iter()
        .filter(|o| matches!(o, Outcome::Finalized { .. }))
        .count();
    assert_eq!(won, 1, "{results:?}");
    assert!(results
        .iter()
        .all(|o| matches!(o, Outcome::Finalized { .. } | Outcome::AlreadyDone)));

    // And again later.
    assert_eq!(
        finalize::finalize(&srv.state, c.live).await.unwrap(),
        Outcome::AlreadyDone
    );
    assert_eq!(counts(&srv, &c, None).await["transcripts"], 1);
}

#[tokio::test]
async fn a_consumer_conversation_gets_no_transcript_row_and_keeps_its_events() {
    let srv = srv!();
    let (_, owner) = login(&srv, "Marco").await;
    let c = seed(&srv, Owner::User(owner), "ended", 0, &[exchange()]).await;

    let out = finalize::finalize(&srv.state, c.live).await.unwrap();
    assert_eq!(
        out,
        Outcome::NotApplicable,
        "a consumer transcript has no organisation to belong to"
    );
    let n = counts(&srv, &c, None).await;
    assert_eq!(n["transcripts"], 0);
    assert_eq!(
        n["transcript_events"], 3,
        "the opted-in transcript stays readable"
    );
    assert_eq!(status_of(&srv, c.calls[0]).await, "none");
}

#[tokio::test]
async fn a_conversation_that_lived_through_two_rooms_is_gathered_whole() {
    let srv = srv!();
    let org = org(&srv).await;
    // Both phones dropped once, so the second half lives in another meeting-side row.
    let first = vec![
        say(Who::Staff, "Buongiorno", "it", 3),
        say(Who::Guest, "Hola, buenos días", "es", 9),
    ];
    let second = vec![
        say(Who::Guest, "Me he cortado, perdone", "es", 40),
        say(Who::Staff, "Nessun problema", "it", 47),
    ];
    let c = seed(&srv, Owner::Org(&org), "ended", 0, &[first, second]).await;
    assert_eq!(c.calls.len(), 2);

    let out = finalize::finalize(&srv.state, c.live).await.unwrap();
    assert!(
        matches!(out, Outcome::Finalized { segments: 4, .. }),
        "{out:?}"
    );

    let rows = transcripts_of(&srv, &c.calls).await;
    assert_eq!(rows.len(), 1, "one transcript for the conversation");
    assert_eq!(rows[0].session_id, c.calls[1], "on the latest room session");
    let texts: Vec<&str> = rows[0].segments.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(
        texts,
        [
            "Buongiorno",
            "Hola, buenos días",
            "Me he cortado, perdone",
            "Nessun problema"
        ],
        "both rooms, in the order it was said"
    );
    assert_eq!(rows[0].segments[2].start_ms, 37_000);
    assert_eq!(
        rows[0].segments[1].speaker_id, rows[0].segments[2].speaker_id,
        "the guest is the same speaker in both rooms"
    );
    assert_eq!(status_of(&srv, c.calls[1]).await, "ready");
    assert_eq!(room_session_ids(&srv, c.live).await.len(), 2);
}

async fn room_session_ids(srv: &Srv, live: Uuid) -> Vec<Uuid> {
    finalize::room_session_ids(&srv.pool, live).await.unwrap()
}

#[tokio::test]
async fn nothing_is_finalised_while_open_or_when_nothing_was_said_or_after_retention_expired_it() {
    let srv = srv!();
    let org = org(&srv).await;

    let open = seed(&srv, Owner::Org(&org), "active", 0, &[exchange()]).await;
    assert_eq!(
        finalize::finalize(&srv.state, open.live).await.unwrap(),
        Outcome::NotApplicable,
        "a transcript of a conversation still going would be half of it"
    );
    assert_eq!(counts(&srv, &open, None).await["transcripts"], 0);

    let silent = seed(&srv, Owner::Org(&org), "ended", 0, &[vec![]]).await;
    assert_eq!(
        finalize::finalize(&srv.state, silent.live).await.unwrap(),
        Outcome::NoContent
    );
    assert_eq!(counts(&srv, &silent, None).await["transcripts"], 0);
    assert_eq!(status_of(&srv, silent.calls[0]).await, "none");

    // Retention already took the text out; the surviving events must not resurrect it.
    let expired = seed(&srv, Owner::Org(&org), "ended", 0, &[exchange()]).await;
    sqlx::query("UPDATE call_sessions SET transcript_status = 'expired' WHERE id = $1")
        .bind(expired.calls[0])
        .execute(&srv.pool)
        .await
        .unwrap();
    assert_eq!(
        finalize::finalize(&srv.state, expired.live).await.unwrap(),
        Outcome::AlreadyDone
    );
    assert_eq!(counts(&srv, &expired, None).await["transcripts"], 0);

    // A session that does not exist is not an error either.
    assert_eq!(
        finalize::finalize(&srv.state, Uuid::new_v4())
            .await
            .unwrap(),
        Outcome::NotApplicable
    );
}

#[tokio::test]
async fn indexing_keeps_each_speaker_in_chunks_of_their_own() {
    let srv = srv!();
    let org = org(&srv).await;
    let c = seed(&srv, Owner::Org(&org), "ended", 0, &[exchange()]).await;
    finalize::finalize(&srv.state, c.live).await.unwrap();
    assert_eq!(index_without_provider(&srv, &org, c.calls[0]).await, 3);

    let rows: Vec<(String, String, Option<i64>)> = sqlx::query_as(
        "SELECT speaker_name, content, start_ms FROM transcript_embeddings
         WHERE session_id = $1 ORDER BY chunk_index",
    )
    .bind(c.calls[0])
    .fetch_all(&srv.pool)
    .await
    .unwrap();
    // Three short turns by alternating speakers stay three snippets, each under its speaker, not
    // one blob labelled with whoever spoke first.
    assert_eq!(
        rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
        [STAFF_NAME, GUEST_NAME, STAFF_NAME]
    );
    assert_eq!(rows[1].1, "Quiero reservar una habitación para dos noches");
    assert_eq!(rows[1].2, Some(7_000));
}

// ===== every way a conversation ends leads to finalisation ================================

async fn wait_for_transcript(srv: &Srv, c: &Conv) -> TranscriptRow {
    eventually("the transcript", || async {
        transcripts_of(srv, &c.calls).await.into_iter().next()
    })
    .await
}

#[tokio::test]
async fn ending_over_rest_finalises_the_conversation() {
    let srv = srv!();
    let org = org(&srv).await;
    let c = seed(&srv, Owner::Org(&org), "active", 0, &[exchange()]).await;
    let token = token_for(
        &srv,
        c.live,
        c.staff_peer,
        PeerRole::Staff,
        created_at(&srv, c.live).await,
    );

    let (s, body) = srv
        .call(
            "POST",
            &format!("/api/live/sessions/{}/end", c.live),
            Some(&token),
            Some(json!({})),
        )
        .await;
    assert_eq!((s, &body["ended"]), (StatusCode::OK, &json!(true)));

    let t = wait_for_transcript(&srv, &c).await;
    assert_eq!(t.segments.len(), 3);
}

#[tokio::test]
async fn the_sweeper_finalises_an_abandoned_conversation() {
    let srv = srv!();
    let org = org(&srv).await;
    let c = seed(&srv, Owner::Org(&org), "active", 0, &[exchange()]).await;

    // Nobody is in the room. The first pass only notices; the second, after the grace, ends it.
    sweep_sessions(&srv.state, Some(&[c.live])).await.unwrap();
    tokio::time::sleep(Duration::from_millis(5300)).await;
    sweep_sessions(&srv.state, Some(&[c.live])).await.unwrap();
    let reason: Option<String> =
        sqlx::query_scalar("SELECT end_reason FROM live_sessions WHERE id = $1")
            .bind(c.live)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(reason.as_deref(), Some("abandoned"));

    assert_eq!(wait_for_transcript(&srv, &c).await.segments.len(), 3);
}

#[tokio::test]
async fn the_hard_ceiling_finalises_the_conversation_it_closes() {
    let srv = srv!();
    let org = org(&srv).await;
    let c = seed(&srv, Owner::Org(&org), "active", 0, &[exchange()]).await;
    // Past the 120-minute ceiling: `expire_stale` closes it on the next pass.
    sqlx::query(
        "UPDATE live_sessions SET created_at = now() - interval '121 minutes' WHERE id = $1",
    )
    .bind(c.live)
    .execute(&srv.pool)
    .await
    .unwrap();

    sweep_sessions(&srv.state, Some(&[c.live])).await.unwrap();
    let reason: Option<String> =
        sqlx::query_scalar("SELECT end_reason FROM live_sessions WHERE id = $1")
            .bind(c.live)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(reason.as_deref(), Some("max_duration"));

    assert_eq!(wait_for_transcript(&srv, &c).await.segments.len(), 3);
}

async fn open_ws(
    srv: &Srv,
    token: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://{}/ws/live?token={token}", srv.addr))
            .await
            .expect("upgrade");
    loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) if t.contains("room_joined") => return ws,
            Ok(Some(Ok(_))) => continue,
            other => panic!("no room_joined: {other:?}"),
        }
    }
}

#[tokio::test]
async fn the_socket_ceiling_finalises_the_conversation_it_closes() {
    // One-minute ceiling, a conversation 58 s old: the connected phone's own check closes it.
    let srv = srv!(&[("LIVE_MAX_SESSION_MIN", "1")]);
    let org = org(&srv).await;
    let c = seed(&srv, Owner::Org(&org), "active", 0, &[exchange()]).await;
    sqlx::query(
        "UPDATE live_sessions SET created_at = now() - interval '58 seconds' WHERE id = $1",
    )
    .bind(c.live)
    .execute(&srv.pool)
    .await
    .unwrap();
    let token = token_for(
        &srv,
        c.live,
        c.staff_peer,
        PeerRole::Staff,
        created_at(&srv, c.live).await,
    );
    let mut ws = open_ws(&srv, &token).await;

    let reason = eventually("the ceiling to close it", || async {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT end_reason FROM live_sessions WHERE id = $1",
        )
        .bind(c.live)
        .fetch_one(&srv.pool)
        .await
        .unwrap()
    })
    .await;
    assert_eq!(reason, "max_duration");

    // Connecting made a room of its own (a new meeting-side row, with nothing said in it); the
    // conversation is still gathered whole, across the seeded room and this one.
    let t = wait_for_transcript(
        &srv,
        &Conv {
            calls: all_calls(&srv, c.live).await,
            ..c
        },
    )
    .await;
    assert_eq!(t.segments.len(), 3);
    let _ = ws.close(None).await;
}

async fn all_calls(srv: &Srv, live: Uuid) -> Vec<Uuid> {
    room_session_ids(srv, live).await
}

#[tokio::test]
async fn a_conversation_that_runs_out_of_credit_is_finalised_with_what_was_really_said() {
    // $60 a minute: every one-second tick costs far more than the org has left once drained.
    // A short real grace, as in production: the engines' last finals land while it runs.
    let srv = srv!(&[
        ("@cost_per_minute", "60"),
        ("LIVE_FINALIZE_GRACE_SECS", "3")
    ]);
    let org = org(&srv).await;
    let (s, created) = srv
        .call(
            "POST",
            "/api/live/sessions",
            Some(&org.member_jwt),
            Some(json!({ "lang": "it", "org_id": org.id, "project_id": org.project })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{created}");
    let code = created["invite_url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_string();
    let (s, joined) = srv
        .call(
            "POST",
            "/api/live/join",
            None,
            Some(json!({ "code": code, "name": "Customer", "lang": "es", "consent": true })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{joined}");
    let live: Uuid = created["session_id"].as_str().unwrap().parse().unwrap();
    let mut host = open_ws(&srv, created["token"].as_str().unwrap()).await;
    let mut guest = open_ws(&srv, joined["token"].as_str().unwrap()).await;
    use futures::SinkExt as _;
    for ws in [&mut host, &mut guest] {
        ws.send(Message::text(json!({ "type": "start" }).to_string()))
            .await
            .unwrap();
    }
    // Let both starts pass their credit gate before the wallet is emptied.
    tokio::time::sleep(Duration::from_millis(800)).await;
    // Both talk while the wallet is drained: the first tick cannot be paid and the
    // conversation ends from inside the socket.
    sqlx::query("UPDATE organizations SET credits_balance = 0 WHERE id = $1")
        .bind(org.id)
        .execute(&srv.pool)
        .await
        .unwrap();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
    let reason = loop {
        tokio::select! {
            _ = tick.tick() => {
                let _ = host.send(Message::binary(vec![0u8; 640])).await;
                let _ = guest.send(Message::binary(vec![0u8; 640])).await;
                let r: Option<String> = sqlx::query_scalar("SELECT end_reason FROM live_sessions WHERE id = $1")
                    .bind(live).fetch_one(&srv.pool).await.unwrap();
                if r.is_some() { break r; }
            }
            Some(_) = host.next() => {}
            Some(_) = guest.next() => {}
            _ = tokio::time::sleep_until(deadline) => break None,
        }
    };
    assert_eq!(reason.as_deref(), Some("balance_exhausted"));

    // The transcript is built from what the recorder really stored for both phones.
    let calls = room_session_ids(&srv, live).await;
    let t = eventually("the transcript", || async {
        transcripts_of(&srv, &calls).await.into_iter().next()
    })
    .await;
    let peers: std::collections::HashSet<&str> =
        t.segments.iter().map(|s| s.speaker_id.as_str()).collect();
    assert_eq!(
        peers.len(),
        2,
        "both phones are in it: {:?}",
        t.segments
            .iter()
            .map(|s| (&s.speaker_name, &s.text))
            .collect::<Vec<_>>()
    );
    assert!(t.segments.iter().any(|s| s.text == "ciao a tutti"));
}

#[tokio::test]
async fn a_conversation_missed_by_every_trigger_is_picked_up_by_the_hourly_pass() {
    // A restart between "ended" and "finalised" loses the in-memory timer; the hourly pass
    // finds what was left behind.
    let srv = srv!();
    let org = org(&srv).await;
    let missed = seed(&srv, Owner::Org(&org), "ended", 0, &[exchange()]).await;
    let silent = seed(&srv, Owner::Org(&org), "ended", 0, &[vec![]]).await;
    let open = seed(&srv, Owner::Org(&org), "active", 0, &[exchange()]).await;
    let done = seed(&srv, Owner::Org(&org), "ended", 0, &[exchange()]).await;
    finalize::finalize(&srv.state, done.live).await.unwrap();
    let all = [missed.live, silent.live, open.live, done.live];

    let n = finalize::finalize_pending(&srv.state, Some(&all), 100)
        .await
        .unwrap();
    assert_eq!(n, 1, "only the one that was missed");
    assert_eq!(counts(&srv, &missed, None).await["transcripts"], 1);
    assert_eq!(counts(&srv, &silent, None).await["transcripts"], 0);
    assert_eq!(counts(&srv, &open, None).await["transcripts"], 0);
    assert_eq!(counts(&srv, &done, None).await["transcripts"], 1);
    assert_eq!(
        finalize::finalize_pending(&srv.state, Some(&all), 100)
            .await
            .unwrap(),
        0,
        "and a second pass has nothing left to do"
    );
}

// ===== S13: search filters and scope ======================================================

/// Two finalised, indexed conversations in one project, plus a plain meeting, a voice message
/// and a phone call, so the `kind` filter has something to tell apart.
struct Corpus {
    org: Org,
    a: Conv, // it/es, ~10 days ago
    b: Conv, // it/en, today
    plain_call: Uuid,
}

async fn corpus(srv: &Srv) -> Corpus {
    let org = org(srv).await;
    let a = seed(srv, Owner::Org(&org), "ended", 10, &[exchange()]).await;
    let b = seed(
        srv,
        Owner::Org(&org),
        "ended",
        0,
        &[vec![
            say(
                Who::Staff,
                "Il parcheggio è gratuito per gli ospiti",
                "it",
                4,
            ),
            say(Who::Guest, "Is there a charge for the shuttle?", "en", 11),
        ]],
    )
    .await;
    // The guest of B declared English.
    sqlx::query(
        "UPDATE live_participants SET lang = 'en' WHERE session_id = $1 AND role = 'guest'",
    )
    .bind(b.live)
    .execute(&srv.pool)
    .await
    .unwrap();
    for c in [&a, &b] {
        finalize::finalize(&srv.state, c.live).await.unwrap();
        index_without_provider(srv, &org, c.calls[0]).await;
    }
    // Other kinds, indexed the way they are today.
    let plain_call = Uuid::new_v4();
    for (id, kind) in [
        (plain_call, "call"),
        (Uuid::new_v4(), "voice_message"),
        (Uuid::new_v4(), "phone"),
    ] {
        sqlx::query("INSERT INTO call_sessions (id, room, org_id, project_id, kind, transcript_status) VALUES ($1, $2, $3, $4, $5, 'ready')")
            .bind(id).bind(format!("room-{id}")).bind(org.id).bind(org.project).bind(kind)
            .execute(&srv.pool).await.unwrap();
        let tid: Uuid = sqlx::query_scalar("INSERT INTO transcripts (session_id, org_id, source_language) VALUES ($1, $2, 'en') RETURNING id")
            .bind(id).bind(org.id).fetch_one(&srv.pool).await.unwrap();
        sqlx::query("INSERT INTO transcript_embeddings (transcript_id, session_id, org_id, project_id, chunk_index, speaker_name, content, embedding) VALUES ($1, $2, $3, $4, 0, 'Bob', $5, $6)")
            .bind(tid).bind(id).bind(org.id).bind(org.project).bind(format!("{kind} content")).bind(pgvector::Vector::from(dummy_vector()))
            .execute(&srv.pool).await.unwrap();
    }
    Corpus {
        org,
        a,
        b,
        plain_call,
    }
}

async fn run(
    srv: &Srv,
    org: &Org,
    user: Uuid,
    visible: &Visible,
    project: Option<Uuid>,
    filters: Filters,
) -> Vec<search::ResultRow> {
    search::knn(
        &srv.pool,
        org.id,
        user,
        visible,
        project,
        &filters,
        pgvector::Vector::from(dummy_vector()),
        50,
    )
    .await
    .unwrap()
}

fn sessions_of(rows: &[search::ResultRow]) -> std::collections::BTreeSet<Uuid> {
    rows.iter().filter_map(|r| r.session_id).collect()
}

#[tokio::test]
async fn a_live_hit_carries_what_opens_the_conversation() {
    let srv = srv!();
    let k = corpus(&srv).await;
    let rows = run(
        &srv,
        &k.org,
        k.org.owner,
        &Visible::Everything,
        None,
        Filters::default(),
    )
    .await;
    let hit = rows
        .iter()
        .find(|r| {
            r.session_id == Some(k.b.calls[0]) && r.speaker_name.as_deref() == Some(STAFF_NAME)
        })
        .expect("the live conversation is searchable");
    assert_eq!(hit.project_id, Some(k.org.project));
    assert_eq!(hit.start_ms, Some(0), "timestamp inside the conversation");
    assert!(hit.content.contains("parcheggio"));

    let json = search::result_json(hit);
    assert_eq!(
        json["kind"], "call",
        "the hit's existing shape is unchanged"
    );
    assert_eq!(json["session_kind"], "live_connect");
    assert_eq!(json["live_session_id"], json!(k.b.live));
    assert_eq!(json["session_id"], json!(k.b.calls[0]));
    assert_eq!(json["project_id"], json!(k.org.project));

    // A hit from an ordinary meeting is exactly what it was: no live fields at all.
    let plain = rows
        .iter()
        .find(|r| r.session_id == Some(k.plain_call))
        .unwrap();
    let json = search::result_json(plain);
    assert!(
        json.get("session_kind").is_none() && json.get("live_session_id").is_none(),
        "{json}"
    );
}

#[tokio::test]
async fn every_filter_narrows_the_search_and_no_filter_changes_nothing() {
    let srv = srv!();
    let k = corpus(&srv).await;
    let all = |f: Filters| run(&srv, &k.org, k.org.owner, &Visible::Everything, None, f);
    let (a, b) = (k.a.calls[0], k.b.calls[0]);

    let unfiltered = all(Filters::default()).await;
    // Two live conversations x their chunks, plus the three other kinds.
    assert_eq!(unfiltered.len(), 3 + 2 + 3);

    // kind
    let live = sessions_of(
        &all(Filters {
            kind: Some("live_connect".into()),
            ..Default::default()
        })
        .await,
    );
    assert_eq!(live, [a, b].into_iter().collect());
    let call = sessions_of(
        &all(Filters {
            kind: Some("call".into()),
            ..Default::default()
        })
        .await,
    );
    assert_eq!(call, [k.plain_call].into_iter().collect());
    assert!(
        all(Filters {
            kind: Some("phone".into()),
            ..Default::default()
        })
        .await
        .len()
            == 1
    );

    // lang: either side of a live conversation matches; the plain call is English.
    let es = sessions_of(
        &all(Filters {
            lang: Some("es".into()),
            ..Default::default()
        })
        .await,
    );
    assert_eq!(es, [a].into_iter().collect());
    let en = sessions_of(
        &all(Filters {
            lang: Some("en".into()),
            ..Default::default()
        })
        .await,
    );
    assert!(en.contains(&b) && !en.contains(&a), "{en:?}");
    let it = sessions_of(
        &all(Filters {
            lang: Some("it".into()),
            ..Default::default()
        })
        .await,
    );
    assert_eq!(it, [a, b].into_iter().collect());

    // date range, on when the conversation started
    let now = chrono::Utc::now();
    let recent = sessions_of(
        &all(Filters {
            from: Some(now - chrono::Duration::days(5)),
            kind: Some("live_connect".into()),
            ..Default::default()
        })
        .await,
    );
    assert_eq!(recent, [b].into_iter().collect());
    let old = sessions_of(
        &all(Filters {
            to: Some(now - chrono::Duration::days(5)),
            kind: Some("live_connect".into()),
            ..Default::default()
        })
        .await,
    );
    assert_eq!(old, [a].into_iter().collect());
    let window = all(Filters {
        from: Some(now - chrono::Duration::days(11)),
        to: Some(now - chrono::Duration::days(9)),
        kind: Some("live_connect".into()),
        ..Default::default()
    })
    .await;
    assert_eq!(sessions_of(&window), [a].into_iter().collect());

    // session: by the call-session id a hit carries, or by the live session id
    for id in [a, k.a.live] {
        let rows = all(Filters {
            session_id: Some(id),
            ..Default::default()
        })
        .await;
        assert_eq!(sessions_of(&rows), [a].into_iter().collect(), "{id}");
        assert_eq!(rows.len(), 3);
    }

    // speaker: case-insensitive substring, and `%`/`_` are text, not patterns
    let anna = all(Filters {
        speaker_like: Some("anna".into()),
        ..Default::default()
    })
    .await;
    assert_eq!(anna.len(), 1 /* A */ + 1 /* B */);
    assert!(anna
        .iter()
        .all(|r| r.speaker_name.as_deref() == Some(GUEST_NAME)));
    assert!(all(Filters {
        speaker_like: Some("\\%".into()),
        ..Default::default()
    })
    .await
    .is_empty());
    assert!(all(Filters {
        speaker_like: Some("\\_nna".into()),
        ..Default::default()
    })
    .await
    .is_empty());

    // project narrowing still works with a filter on top
    let other_project = run(
        &srv,
        &k.org,
        k.org.owner,
        &Visible::Everything,
        Some(Uuid::new_v4()),
        Filters {
            kind: Some("live_connect".into()),
            ..Default::default()
        },
    )
    .await;
    assert!(other_project.is_empty());
}

#[tokio::test]
async fn filters_are_validated_before_anything_is_embedded() {
    let srv = srv!();
    let org = org(&srv).await;
    let path = |qs: &str| {
        format!(
            "/api/business/organizations/{}/search?q=parking&{qs}",
            org.id
        )
    };
    for qs in [
        "kind=sms",
        "lang=not%20a%20language",
        "lang=abcdefghijk",
        "from=yesterday",
        "to=2026-13-45T00:00:00Z",
        "session_id=not-a-uuid",
    ] {
        let (s, _) = srv.get(&path(qs), &org.owner_jwt).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{qs}");
    }
    // Valid filters pass validation (and stop at the missing embedding provider).
    let (s, _) = srv
        .get(
            &path("kind=live_connect&lang=es&from=2026-01-01T00:00:00Z&speaker=anna"),
            &org.owner_jwt,
        )
        .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn a_live_conversation_is_only_found_by_who_may_see_its_project_and_org() {
    let srv = srv!();
    let k = corpus(&srv).await;
    let (stranger_jwt, _) = login(&srv, "Stranger").await;
    let (bystander_jwt, bystander_id) = bystander(&srv, &k.org).await;
    let _ = bystander_jwt;
    let live_only = || Filters {
        kind: Some("live_connect".into()),
        ..Default::default()
    };

    // The member who staffed it took part, so the project is theirs; the owner is an admin.
    for (user, is_admin) in [(k.org.member, false), (k.org.owner, true)] {
        let vis = search::resolve_visibility(&srv.pool, k.org.id, user, is_admin)
            .await
            .unwrap();
        let hits = run(&srv, &k.org, user, &vis, None, live_only()).await;
        assert_eq!(sessions_of(&hits).len(), 2, "user {user}");
    }
    // A member who neither created nor took part in the project finds nothing.
    let vis = search::resolve_visibility(&srv.pool, k.org.id, bystander_id, false)
        .await
        .unwrap();
    assert_eq!(vis, Visible::Nothing);
    assert!(run(&srv, &k.org, bystander_id, &vis, None, live_only())
        .await
        .is_empty());
    // Even a member who can see SOME project cannot reach another project's conversation.
    let elsewhere = Visible::Projects(vec![Uuid::new_v4()]);
    assert!(
        run(&srv, &k.org, bystander_id, &elsewhere, None, live_only())
            .await
            .is_empty()
    );

    // Another organisation's admin, searching their own org, reaches none of this org's rows.
    let other = self::org(&srv).await;
    let vis = search::resolve_visibility(&srv.pool, other.id, other.owner, true)
        .await
        .unwrap();
    assert!(
        run(&srv, &other, other.owner, &vis, None, Filters::default())
            .await
            .is_empty()
    );
    // ...and over HTTP a non-member cannot even tell the organisation exists.
    let url = format!("/api/business/organizations/{}/search?q=parking", k.org.id);
    assert_eq!(srv.get(&url, &stranger_jwt).await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        srv.get(&url, &other.owner_jwt).await.0,
        StatusCode::NOT_FOUND
    );
}

/// The dashboard shows a member only the conversations they sat in. Search must say no more:
/// belonging to the project is not belonging to every conversation held under it.
#[tokio::test]
async fn a_member_finds_only_the_live_conversations_they_sat_in_and_an_admin_finds_all() {
    let srv = srv!();
    let k = corpus(&srv).await;
    // `b` was staffed by somebody else: hand its staff seat to the owner.
    sqlx::query(
        "UPDATE live_participants SET user_id = $2 WHERE session_id = $1 AND role = 'staff'",
    )
    .bind(k.b.live)
    .bind(k.org.owner)
    .execute(&srv.pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE session_participants SET user_id = $2 WHERE session_id = $1 AND user_id = $3",
    )
    .bind(k.b.calls[0])
    .bind(k.org.owner)
    .bind(k.org.member)
    .execute(&srv.pool)
    .await
    .unwrap();
    let live_only = || Filters {
        kind: Some("live_connect".into()),
        ..Default::default()
    };

    let vis = search::resolve_visibility(&srv.pool, k.org.id, k.org.member, false)
        .await
        .unwrap();
    assert_ne!(vis, Visible::Nothing, "the project is theirs through `a`");
    let hits = run(&srv, &k.org, k.org.member, &vis, None, live_only()).await;
    assert_eq!(
        sessions_of(&hits),
        [k.a.calls[0]].into(),
        "the member found a conversation they were not in"
    );
    // Naming it does not help, and neither does narrowing by project or session.
    let by_id = Filters {
        session_id: Some(k.b.live),
        ..Default::default()
    };
    assert!(
        run(&srv, &k.org, k.org.member, &vis, Some(k.org.project), by_id)
            .await
            .is_empty()
    );
    // Everything that is not a live conversation behaves as it always did.
    let all = run(&srv, &k.org, k.org.member, &vis, None, Filters::default()).await;
    assert!(
        sessions_of(&all).contains(&k.plain_call) && !sessions_of(&all).contains(&k.b.calls[0]),
        "{:?}",
        sessions_of(&all)
    );

    // The admin is unchanged: the whole org.
    let vis = search::resolve_visibility(&srv.pool, k.org.id, k.org.owner, true)
        .await
        .unwrap();
    let hits = run(&srv, &k.org, k.org.owner, &vis, None, live_only()).await;
    assert_eq!(sessions_of(&hits).len(), 2);
}

// ===== S17 / D4: erasing the content of a conversation ====================================

#[tokio::test]
async fn erasing_deletes_the_content_of_every_room_session_and_keeps_the_billing_facts() {
    let srv = srv!();
    let org = org(&srv).await;
    let first = vec![
        say(Who::Staff, "Buongiorno", "it", 3),
        say(Who::Guest, "Hola", "es", 9),
    ];
    let second = vec![say(Who::Guest, "Una pregunta más", "es", 40)];
    let c = seed(&srv, Owner::Org(&org), "ended", 0, &[first, second]).await;
    finalize::finalize(&srv.state, c.live).await.unwrap();
    index_without_provider(&srv, &org, c.calls[1]).await;

    // Bystanders that must survive: a neighbouring conversation, and an ordinary meeting in the
    // same org and project.
    let neighbour = seed(&srv, Owner::Org(&org), "ended", 0, &[exchange()]).await;
    let meeting = Uuid::new_v4();
    sqlx::query("INSERT INTO call_sessions (id, room, org_id, project_id, kind) VALUES ($1, 'standup', $2, $3, 'call')")
        .bind(meeting).bind(org.id).bind(org.project).execute(&srv.pool).await.unwrap();
    // A forged pointer: an event naming the meeting as one of this conversation's rooms.
    sqlx::query("INSERT INTO live_session_events (session_id, kind, detail) VALUES ($1, 'room_session', $2)")
        .bind(c.live).bind(json!({ "call_session_id": meeting })).execute(&srv.pool).await.unwrap();

    let before = counts(&srv, &c, Some(org.id)).await;
    assert_eq!(before["call_sessions"], 2);
    assert_eq!(before["session_participants"], 4);
    assert_eq!(before["transcript_events"], 3);
    assert_eq!(before["transcripts"], 1);
    assert!(before["transcript_embeddings"] >= 1);
    assert_eq!((before["live_usage_ticks"], before["ledger"]), (2, 1));
    let neighbour_before = counts(&srv, &neighbour, Some(org.id)).await;

    let out = erase::erase_content(&srv.pool, c.live).await.unwrap();
    assert_eq!(out.call_sessions, 2);
    assert_eq!(out.participants_scrubbed, 2);

    let after = counts(&srv, &c, Some(org.id)).await;
    // Content: gone, in BOTH room sessions, with everything that hangs off them.
    for table in [
        "call_sessions",
        "session_participants",
        "transcript_events",
        "transcripts",
        "transcript_embeddings",
    ] {
        assert_eq!(after[table], 0, "{table}");
    }
    // Billing: the conversation row, its ticks, the ledger row and the diagnostics all stay.
    assert_eq!(after["live_sessions"], 1);
    assert_eq!(after["live_participants"], 2);
    assert_eq!((after["live_usage_ticks"], after["ledger"]), (2, 1));
    assert!(after["live_session_events"] >= before["live_session_events"]);
    let (st, end): (String, Option<String>) =
        sqlx::query_as("SELECT status, end_reason FROM live_sessions WHERE id = $1")
            .bind(c.live)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(
        (st.as_str(), end.as_deref()),
        ("ended", Some("ended_by_staff"))
    );
    let call_ptr: Option<Uuid> =
        sqlx::query_scalar("SELECT call_session_id FROM live_sessions WHERE id = $1")
            .bind(c.live)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(call_ptr, None);

    // The PII that survives on the participants is gone: names scrubbed, token hashes dropped.
    let people: Vec<(String, Option<Vec<u8>>)> = sqlx::query_as(
        "SELECT display_name, token_hash FROM live_participants WHERE session_id = $1",
    )
    .bind(c.live)
    .fetch_all(&srv.pool)
    .await
    .unwrap();
    assert_eq!(people.len(), 2);
    assert!(
        people
            .iter()
            .all(|(n, h)| n == erase::ERASED_NAME && h.is_none()),
        "{people:?}"
    );

    // Nothing else was touched: the neighbour is identical and the meeting is still there.
    assert_eq!(
        counts(&srv, &neighbour, Some(org.id)).await,
        neighbour_before
    );
    let meeting_left: i64 = sqlx::query_scalar("SELECT count(*) FROM call_sessions WHERE id = $1")
        .bind(meeting)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
    assert_eq!(
        meeting_left, 1,
        "a forged room_session event cannot delete another session"
    );
    let hits = run(
        &srv,
        &org,
        org.owner,
        &Visible::Everything,
        None,
        Filters {
            session_id: Some(c.live),
            ..Default::default()
        },
    )
    .await;
    assert!(hits.is_empty(), "and it is no longer searchable");

    // Erasing again is a no-op, not an error.
    let again = erase::erase_content(&srv.pool, c.live).await.unwrap();
    assert_eq!((again.call_sessions, again.participants_scrubbed), (0, 0));
    assert_eq!(counts(&srv, &c, Some(org.id)).await, after);
}

#[tokio::test]
async fn only_an_org_admin_may_delete_a_business_conversation_and_never_an_open_one() {
    let srv = srv!();
    let org = org(&srv).await;
    let other = self::org(&srv).await;
    let (stranger_jwt, _) = login(&srv, "Stranger").await;
    let c = seed(&srv, Owner::Org(&org), "ended", 0, &[exchange()]).await;
    let path = org_session_path(&org, c.live);
    let intact = counts(&srv, &c, Some(org.id)).await;

    // Not signed in; a stranger (404: the org is not theirs to know about); another org's admin
    // (the same); a plain member (403).
    assert_eq!(srv.delete(&path, None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        srv.delete(&path, Some(&stranger_jwt)).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        srv.delete(&path, Some(&other.owner_jwt)).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        srv.delete(&path, Some(&org.member_jwt)).await.0,
        StatusCode::FORBIDDEN
    );
    // An admin of the WRONG org path cannot reach this org's conversation through theirs.
    assert_eq!(
        srv.delete(&org_session_path(&other, c.live), Some(&other.owner_jwt))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        srv.delete(
            &org_session_path(&org, Uuid::new_v4()),
            Some(&org.owner_jwt)
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        counts(&srv, &c, Some(org.id)).await,
        intact,
        "every refusal left the data alone"
    );

    // An open conversation must be ended first.
    let open = seed(&srv, Owner::Org(&org), "active", 0, &[exchange()]).await;
    let open_intact = counts(&srv, &open, Some(org.id)).await;
    let (s, b) = srv
        .delete(&org_session_path(&org, open.live), Some(&org.owner_jwt))
        .await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::CONFLICT, Some("session_active"))
    );
    assert_eq!(counts(&srv, &open, Some(org.id)).await, open_intact);

    // The admin can; the content goes, the billing facts stay, and the action is audited.
    let (s, b) = srv.delete(&path, Some(&org.owner_jwt)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["erased"], true);
    let after = counts(&srv, &c, Some(org.id)).await;
    assert_eq!(
        (
            after["call_sessions"],
            after["transcript_events"],
            after["session_participants"]
        ),
        (0, 0, 0)
    );
    assert_eq!(
        (
            after["live_sessions"],
            after["live_usage_ticks"],
            after["ledger"]
        ),
        (1, 2, 1)
    );
    let audit: (Uuid, String) = eventually("the audit entry", || async {
        sqlx::query_as("SELECT actor_id, resource_type FROM audit_logs WHERE org_id = $1 AND action = 'live_session.delete' AND resource_id = $2")
            .bind(org.id).bind(c.live).fetch_optional(&srv.pool).await.unwrap()
    })
    .await;
    assert_eq!(audit, (org.owner, "live_session".to_string()));
    // Deleting twice is fine.
    assert_eq!(
        srv.delete(&path, Some(&org.owner_jwt)).await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn the_consumer_owner_deletes_their_own_conversation_and_nobody_else_can() {
    let srv = srv!();
    let (owner_jwt, owner) = login(&srv, "Marco").await;
    let (other_jwt, _) = login(&srv, "Luca").await;
    let org = org(&srv).await;
    let c = seed(&srv, Owner::User(owner), "ended", 0, &[exchange()]).await;
    let path = format!("/api/live/sessions/{}", c.live);
    let intact = counts(&srv, &c, None).await;
    assert_eq!(intact["transcript_events"], 3);

    // No credential; somebody else's account; a participant's live token (not an account); an
    // org admin (this is not an org conversation).
    assert_eq!(srv.delete(&path, None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        srv.delete(&path, Some(&other_jwt)).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        srv.delete(&path, Some(&org.owner_jwt)).await.0,
        StatusCode::NOT_FOUND
    );
    let guest_token = token_for(
        &srv,
        c.live,
        c.guest_peer,
        PeerRole::Guest,
        created_at(&srv, c.live).await,
    );
    assert_eq!(
        srv.delete(&path, Some(&guest_token)).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(counts(&srv, &c, None).await, intact);

    // An org conversation is not reachable here, whoever asks.
    let b2b = seed(&srv, Owner::Org(&org), "ended", 0, &[exchange()]).await;
    assert_eq!(
        srv.delete(
            &format!("/api/live/sessions/{}", b2b.live),
            Some(&org.owner_jwt)
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        counts(&srv, &b2b, Some(org.id)).await["transcript_events"],
        3
    );

    // An open one is refused.
    let open = seed(&srv, Owner::User(owner), "active", 0, &[exchange()]).await;
    let (s, b) = srv
        .delete(
            &format!("/api/live/sessions/{}", open.live),
            Some(&owner_jwt),
        )
        .await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::CONFLICT, Some("session_active"))
    );
    assert_eq!(counts(&srv, &open, None).await["transcript_events"], 3);

    // The owner can: content gone, the usage facts stay.
    let (s, b) = srv.delete(&path, Some(&owner_jwt)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let after = counts(&srv, &c, None).await;
    assert_eq!(
        (
            after["call_sessions"],
            after["transcript_events"],
            after["session_participants"]
        ),
        (0, 0, 0)
    );
    assert_eq!((after["live_sessions"], after["live_usage_ticks"]), (1, 2));
}

// ---- retention ---------------------------------------------------------------------------

async fn set_retention(srv: &Srv, org: &Org, settings: Value) {
    sqlx::query("UPDATE organizations SET settings = $2 WHERE id = $1")
        .bind(org.id)
        .bind(settings)
        .execute(&srv.pool)
        .await
        .unwrap();
}

async fn content_left(srv: &Srv, c: &Conv) -> i64 {
    counts(srv, c, None).await["transcript_events"]
}

#[tokio::test]
async fn retention_erases_only_ended_conversations_older_than_the_orgs_window() {
    let srv = srv!();
    let org = org(&srv).await;
    set_retention(&srv, &org, json!({ "retention_days": 30 })).await;

    let old = seed(&srv, Owner::Org(&org), "ended", 31, &[exchange()]).await;
    let recent = seed(&srv, Owner::Org(&org), "ended", 29, &[exchange()]).await;
    let expired_old = seed(&srv, Owner::Org(&org), "expired", 40, &[vec![]]).await;
    // Old but still open: never touched, however old it looks.
    let open_old = seed(&srv, Owner::Org(&org), "active", 0, &[exchange()]).await;
    sqlx::query("UPDATE live_sessions SET created_at = now() - interval '60 days' WHERE id = $1")
        .bind(open_old.live)
        .execute(&srv.pool)
        .await
        .unwrap();
    let all = [old.live, recent.live, expired_old.live, open_old.live];

    let n = erase::retention_pass(&srv.state, Some(&all)).await.unwrap();
    assert_eq!(
        n, 2,
        "the old ended one and the old expired one (its guest's name)"
    );
    assert_eq!(content_left(&srv, &old).await, 0);
    assert_eq!(counts(&srv, &old, Some(org.id)).await["call_sessions"], 0);
    assert_eq!(
        counts(&srv, &old, Some(org.id)).await["live_usage_ticks"],
        2,
        "billing history stays"
    );
    assert_eq!(content_left(&srv, &recent).await, 3);
    assert_eq!(content_left(&srv, &open_old).await, 3);
    let guest: String = sqlx::query_scalar(
        "SELECT display_name FROM live_participants WHERE session_id = $1 AND role = 'guest'",
    )
    .bind(expired_old.live)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(guest, erase::ERASED_NAME);

    // Nothing is left to do on a second pass.
    assert_eq!(
        erase::retention_pass(&srv.state, Some(&all)).await.unwrap(),
        0
    );
}

#[tokio::test]
async fn an_org_that_keeps_everything_is_never_swept_and_a_window_of_its_own_beats_the_default() {
    let srv = srv!();
    let forever = org(&srv).await;
    set_retention(&srv, &forever, json!({ "retention_days": 0 })).await;
    let keep = seed(&srv, Owner::Org(&forever), "ended", 400, &[exchange()]).await;

    let short = org(&srv).await;
    set_retention(&srv, &short, json!({ "retention_days": 7 })).await;
    let short_old = seed(&srv, Owner::Org(&short), "ended", 8, &[exchange()]).await;

    let long = org(&srv).await;
    set_retention(&srv, &long, json!({ "retention_days": 365 })).await;
    let long_mid = seed(&srv, Owner::Org(&long), "ended", 200, &[exchange()]).await;

    // A setting the organisation DID make but that we cannot read keeps its conversations: erasing
    // on a default it never chose would be the irreversible way to be wrong. (An org that set
    // nothing gets the default — see the next test.)
    let junk = org(&srv).await;
    set_retention(&srv, &junk, json!({ "retention_days": "soon" })).await;
    let junk_old = seed(&srv, Owner::Org(&junk), "ended", 100, &[exchange()]).await;
    let junk_new = seed(&srv, Owner::Org(&junk), "ended", 60, &[exchange()]).await;

    let all = [
        keep.live,
        short_old.live,
        long_mid.live,
        junk_old.live,
        junk_new.live,
    ];
    assert_eq!(
        erase::retention_pass(&srv.state, Some(&all)).await.unwrap(),
        1
    );
    assert_eq!(content_left(&srv, &keep).await, 3, "0 means keep");
    assert_eq!(
        content_left(&srv, &short_old).await,
        0,
        "the org's own 7 days"
    );
    assert_eq!(
        content_left(&srv, &long_mid).await,
        3,
        "the org's own 365 days"
    );
    assert_eq!(
        content_left(&srv, &junk_old).await,
        3,
        "an unreadable setting keeps everything, even past the default 90"
    );
    assert_eq!(content_left(&srv, &junk_new).await, 3);
}

#[tokio::test]
async fn a_default_of_zero_switches_off_retention_for_orgs_without_a_window() {
    let srv = srv!(&[("LIVE_RETENTION_DEFAULT_DAYS", "0")]);
    let none = org(&srv).await;
    set_retention(&srv, &none, json!({})).await;
    let kept = seed(&srv, Owner::Org(&none), "ended", 500, &[exchange()]).await;
    let own = org(&srv).await;
    set_retention(&srv, &own, json!({ "retention_days": 30 })).await;
    let erased = seed(&srv, Owner::Org(&own), "ended", 500, &[exchange()]).await;

    let n = erase::retention_pass(&srv.state, Some(&[kept.live, erased.live]))
        .await
        .unwrap();
    assert_eq!(n, 1, "an explicit window of the org's own still applies");
    assert_eq!(content_left(&srv, &kept).await, 3);
    assert_eq!(content_left(&srv, &erased).await, 0);
}

#[tokio::test]
async fn consumer_transcripts_are_erased_after_their_own_window_and_only_when_opted_in() {
    let srv = srv!();
    let (_, owner) = login(&srv, "Marco").await;
    let old = seed(&srv, Owner::User(owner), "ended", 31, &[exchange()]).await;
    let recent = seed(&srv, Owner::User(owner), "ended", 29, &[exchange()]).await;
    // Not opted in: no transcript was stored, and this pass is about transcripts.
    let not_saved = seed(&srv, Owner::User(owner), "ended", 90, &[vec![]]).await;
    sqlx::query("UPDATE live_sessions SET save_transcript = FALSE WHERE id = $1")
        .bind(not_saved.live)
        .execute(&srv.pool)
        .await
        .unwrap();
    let all = [old.live, recent.live, not_saved.live];

    assert_eq!(
        erase::retention_pass(&srv.state, Some(&all)).await.unwrap(),
        1
    );
    assert_eq!(content_left(&srv, &old).await, 0);
    assert_eq!(counts(&srv, &old, None).await["live_usage_ticks"], 2);
    assert_eq!(content_left(&srv, &recent).await, 3);
    let name: String = sqlx::query_scalar(
        "SELECT display_name FROM live_participants WHERE session_id = $1 AND role = 'guest'",
    )
    .bind(not_saved.live)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(name, GUEST_NAME, "outside this pass");
}

#[tokio::test]
async fn consumer_retention_of_zero_keeps_transcripts_until_the_owner_deletes_them() {
    let srv = srv!(&[("LIVE_B2C_RETENTION_DAYS", "0")]);
    let (_, owner) = login(&srv, "Marco").await;
    let old = seed(&srv, Owner::User(owner), "ended", 400, &[exchange()]).await;
    assert_eq!(
        erase::retention_pass(&srv.state, Some(&[old.live]))
            .await
            .unwrap(),
        0
    );
    assert_eq!(content_left(&srv, &old).await, 3);
}

#[tokio::test]
async fn the_hourly_maintenance_finalises_stragglers_and_applies_retention() {
    let srv = srv!();
    let org = org(&srv).await;
    set_retention(&srv, &org, json!({ "retention_days": 30 })).await;
    let missed = seed(&srv, Owner::Org(&org), "ended", 1, &[exchange()]).await;
    let old = seed(&srv, Owner::Org(&org), "ended", 45, &[exchange()]).await;

    let (finalised, erased) = run_maintenance(&srv.state, Some(&[missed.live, old.live])).await;
    assert_eq!((finalised, erased), (1, 1));
    assert_eq!(counts(&srv, &missed, None).await["transcripts"], 1);
    assert_eq!(content_left(&srv, &old).await, 0);
    assert_eq!(
        counts(&srv, &old, None).await["transcripts"],
        0,
        "an old conversation is erased, not first finalised"
    );
}

#[tokio::test]
async fn the_hourly_maintenance_refreshes_the_per_desk_usage_rollup_directory_charts_read() {
    let srv = srv!();
    let org = org(&srv).await;
    let desk: Uuid = sqlx::query_scalar(
        "INSERT INTO live_desks (org_id, name, public_id) VALUES ($1, 'Front', $2) RETURNING id",
    )
    .bind(org.id)
    .bind(format!("d{}", &Uuid::new_v4().simple().to_string()[..11]))
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    let session: Uuid = sqlx::query_scalar(
        "INSERT INTO live_sessions (org_id, desk_id, host_lang, status, expires_at) VALUES ($1, $2, 'it', 'ended', now() + interval '1 hour') RETURNING id",
    )
    .bind(org.id)
    .bind(desk)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    let peer = Uuid::new_v4();
    sqlx::query("INSERT INTO live_participants (session_id, peer_id, role, display_name, lang, token_hash) VALUES ($1, $2, 'guest', 'G', 'es', $3)")
        .bind(session)
        .bind(peer)
        .bind(vec![0u8; 32])
        .execute(&srv.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO live_usage_ticks (session_id, peer_id, tick_seq, payer_kind, amount, ts) VALUES ($1, $2, 1, 'org', 6, now())")
        .bind(session)
        .bind(peer)
        .execute(&srv.pool)
        .await
        .unwrap();
    let rollup = || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(SUM(credits), 0)::bigint FROM live_desk_usage_daily WHERE desk_id = $1",
        )
        .bind(desk)
        .fetch_one(&srv.pool)
        .await
        .unwrap()
    };
    assert_eq!(rollup().await, 0, "nothing refreshed it yet");

    run_maintenance(&srv.state, Some(&[])).await;
    assert_eq!(
        rollup().await,
        6,
        "the maintenance pass keeps the rollup current"
    );
}

// ---- account deletion (defect D4) ----------------------------------------------------------

#[tokio::test]
async fn deleting_an_account_erases_the_conversations_it_owned_including_the_guests_words() {
    let srv = srv!();
    let (_, owner) = login(&srv, "Marco").await;
    let first = vec![
        say(Who::Staff, "Ciao", "it", 2),
        say(Who::Guest, "Hola, me llamo Lucía", "es", 8),
    ];
    let second = vec![say(Who::Guest, "Otra vez", "es", 30)];
    let mine = seed(&srv, Owner::User(owner), "ended", 0, &[first, second]).await;

    // Somebody else's conversation, and a business conversation the same person merely staffed.
    let (_, stranger) = login(&srv, "Luca").await;
    let theirs = seed(&srv, Owner::User(stranger), "ended", 0, &[exchange()]).await;
    let org = org(&srv).await;
    let org_conv = seed(&srv, Owner::Org(&org), "ended", 0, &[exchange()]).await;

    let before = counts(&srv, &mine, None).await;
    assert_eq!(
        (before["call_sessions"], before["transcript_events"]),
        (2, 3)
    );
    assert_eq!(before["session_participants"], 4);
    let guest_name: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM session_participants WHERE session_id = ANY($1) AND name = $2",
    )
    .bind(&mine.calls)
    .bind(GUEST_NAME)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(guest_name, 2);
    let theirs_before = counts(&srv, &theirs, None).await;
    let org_before = counts(&srv, &org_conv, Some(org.id)).await;

    srv.state
        .safety
        .as_ref()
        .unwrap()
        .delete_user(owner)
        .await
        .unwrap();

    // Before the fix these survived: the meeting-side rows hold the guest's name and words and
    // nothing pointed at them from the user.
    let after = counts(&srv, &mine, None).await;
    for table in [
        "call_sessions",
        "session_participants",
        "transcript_events",
        "transcripts",
        "transcript_embeddings",
    ] {
        assert_eq!(after[table], 0, "{table} survived the account");
    }
    let guest_left: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM session_participants WHERE name = $1 AND session_id = ANY($2)",
    )
    .bind(GUEST_NAME)
    .bind(&mine.calls)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(guest_left, 0);
    assert_eq!(
        after["live_sessions"], 0,
        "the owner's own usage rows go with the owner"
    );

    // Nobody else's data moved.
    assert_eq!(counts(&srv, &theirs, None).await, theirs_before);
    assert_eq!(counts(&srv, &org_conv, Some(org.id)).await, org_before);
}

#[tokio::test]
async fn an_account_that_staffed_a_business_conversation_does_not_take_the_orgs_records_with_it() {
    let srv = srv!();
    let org = org(&srv).await;
    let c = seed(&srv, Owner::Org(&org), "ended", 0, &[exchange()]).await;
    finalize::finalize(&srv.state, c.live).await.unwrap();

    srv.state
        .safety
        .as_ref()
        .unwrap()
        .delete_user(org.member)
        .await
        .unwrap();

    let after = counts(&srv, &c, Some(org.id)).await;
    // The org owns the conversation: its record, the guest's words and the org ledger stay.
    assert_eq!(
        (
            after["call_sessions"],
            after["transcripts"],
            after["ledger"]
        ),
        (1, 1, 1)
    );
    assert_eq!(after["live_sessions"], 1);
    // The staff member's own utterances are the account's data and go (existing behaviour).
    let events: Vec<String> =
        sqlx::query_scalar("SELECT speaker_name FROM transcript_events WHERE session_id = ANY($1)")
            .bind(&c.calls)
            .fetch_all(&srv.pool)
            .await
            .unwrap();
    assert_eq!(events, vec![GUEST_NAME.to_string()]);
}

// ---- findings of the independent verification of T8 ---------------------------------------

#[tokio::test]
async fn a_forged_room_pointer_cannot_reach_another_conversations_room() {
    let srv = srv!();
    let org = org(&srv).await;
    let mine = seed(&srv, Owner::Org(&org), "ended", 0, &[exchange()]).await;
    let theirs = seed(&srv, Owner::Org(&org), "ended", 0, &[exchange()]).await;

    // A participant who can write events of their own conversation names the OTHER conversation's
    // room. It is a real `live_connect` row, so only the room-name check stands in the way.
    sqlx::query("INSERT INTO live_session_events (session_id, kind, detail) VALUES ($1, 'room_session', jsonb_build_object('call_session_id', $2::text))")
        .bind(mine.live)
        .bind(theirs.calls[0].to_string())
        .execute(&srv.pool)
        .await
        .unwrap();
    let before = counts(&srv, &theirs, Some(org.id)).await;
    erase::erase_content(&srv.pool, mine.live).await.unwrap();
    assert_eq!(
        counts(&srv, &theirs, Some(org.id)).await,
        before,
        "another conversation was reached through a forged pointer"
    );
}

#[tokio::test]
async fn a_retention_setting_we_do_not_understand_keeps_the_conversation() {
    let srv = srv!();
    // Absent means the default (90 days), so an old conversation goes...
    let org_default = org(&srv).await;
    let old = seed(&srv, Owner::Org(&org_default), "ended", 200, &[exchange()]).await;
    // ...but a value the organisation DID set and we cannot read must never fall back to it.
    for junk in ["99999", "-5", "30.5", "30d", "", "true"] {
        let o = org(&srv).await;
        sqlx::query("UPDATE organizations SET settings = jsonb_set(COALESCE(settings, '{}'::jsonb), '{retention_days}', to_jsonb($2::text)) WHERE id = $1")
            .bind(o.id)
            .bind(junk)
            .execute(&srv.pool)
            .await
            .unwrap();
        let c = seed(&srv, Owner::Org(&o), "ended", 200, &[exchange()]).await;
        erase::retention_pass(&srv.state, Some(&[c.live]))
            .await
            .unwrap();
        assert!(
            counts(&srv, &c, Some(o.id)).await["call_sessions"] > 0,
            "retention_days = {junk:?} erased on the default"
        );
    }
    erase::retention_pass(&srv.state, Some(&[old.live]))
        .await
        .unwrap();
    assert_eq!(
        counts(&srv, &old, Some(org_default.id)).await["call_sessions"],
        0,
        "an organisation that set nothing gets the default"
    );
}

#[tokio::test]
async fn a_personal_conversation_nobody_asked_to_keep_leaves_no_names_behind() {
    let srv = srv!();
    let (_, owner) = login(&srv, "Marco").await;
    let kept = seed(&srv, Owner::User(owner), "ended", 0, &[exchange()]).await;
    let dropped = seed(&srv, Owner::User(owner), "ended", 0, &[exchange()]).await;
    sqlx::query("UPDATE live_sessions SET save_transcript = FALSE WHERE id = $1")
        .bind(dropped.live)
        .execute(&srv.pool)
        .await
        .unwrap();

    finalize::finalize(&srv.state, kept.live).await.unwrap();
    finalize::finalize(&srv.state, dropped.live).await.unwrap();
    let names = |live: Uuid| {
        let pool = srv.pool.clone();
        async move {
            sqlx::query_scalar::<_, String>(
                "SELECT display_name FROM live_participants WHERE session_id = $1 ORDER BY role",
            )
            .bind(live)
            .fetch_all(&pool)
            .await
            .unwrap()
        }
    };
    assert!(
        names(kept.live).await.contains(&GUEST_NAME.to_string()),
        "an opted-in conversation keeps its speakers' names"
    );
    assert_eq!(
        names(dropped.live).await,
        vec![erase::ERASED_NAME.to_string(); 2],
        "nothing was recorded, so the names must not outlive the call"
    );
}

#[tokio::test]
async fn erasing_stays_possible_when_the_feature_is_switched_off() {
    // LIVE_CONNECT_ENABLED unset: the live routes are gone, the means to delete are not.
    let srv = srv!(&[("LIVE_CONNECT_ENABLED", "0")]);
    let org = org(&srv).await;
    let c = seed(&srv, Owner::Org(&org), "ended", 0, &[exchange()]).await;
    let res = srv
        .http
        .delete(format!(
            "http://{}{}",
            srv.addr,
            org_session_path(&org, c.live)
        ))
        .bearer_auth(&org.owner_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        reqwest::StatusCode::OK,
        "{:?}",
        res.text().await
    );
    assert_eq!(counts(&srv, &c, Some(org.id)).await["call_sessions"], 0);
}
