//! What the dashboard reads about Live Connect (spec 0121, S14): the session list, one session
//! with what was said, and usage by desk / project / day.
//!
//! The rules under test: an admin sees the whole organisation, a plain member only the
//! conversations they sat in, a stranger nothing; a conversation you may not see answers exactly
//! like one that does not exist; the original is never replaced by a translation; money is for
//! admins and counts only the org's own metered ticks.
//!
//! DB-gated like the other integration suites. Run against the local pgvector Postgres:
//! `DATABASE_URL=postgres://vox:vox_local_dev@127.0.0.1:55432/voxtranslate_voip_test \
//!   cargo test --test live_dashboard`. A run of a few milliseconds skipped everything.

use std::net::SocketAddr;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use reqwest::StatusCode;
use serde_json::{json, Value};
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::Config;
use voxtranslate_server::db::{self, Pool};
use voxtranslate_server::live::config::LiveConfig;
use voxtranslate_server::live::LiveState;
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::{app, AppState};

const SECRET: &str = "live-dashboard-secret";
const LIVE_KEY: &str = "0123456789abcdef0123456789abcdef-live-dashboard";

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
    async fn get(&self, path: &str, bearer: &str) -> (StatusCode, Value) {
        let res = self
            .http
            .get(format!("http://{}{path}", self.addr))
            .bearer_auth(bearer)
            .send()
            .await
            .unwrap();
        let status = res.status();
        let text = res.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
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
    (
        issue_jwt(SECRET, &user.id, &user.email, &user.name, 168).unwrap(),
        user.id,
    )
}

struct World {
    org: Uuid,
    owner: (String, Uuid),
    member_a: (String, Uuid),
    member_b: (String, Uuid),
    project_a: Uuid,
    project_b: Uuid,
    desk1: Uuid,
    desk2: Uuid,
}

async fn world(srv: &Srv) -> World {
    let owner = login(srv, "Owner").await;
    let member_a = login(srv, "Anna").await;
    let member_b = login(srv, "Bruno").await;
    let org: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id, credits_balance) VALUES ('Hotel', $1, $2, 777) RETURNING id",
    )
    .bind(format!("slug-{}", Uuid::new_v4().simple()))
    .bind(owner.1)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    for (user, role) in [
        (owner.1, "owner"),
        (member_a.1, "member"),
        (member_b.1, "member"),
    ] {
        sqlx::query("INSERT INTO organization_members (org_id, user_id, role) VALUES ($1, $2, $3)")
            .bind(org)
            .bind(user)
            .bind(role)
            .execute(&srv.pool)
            .await
            .unwrap();
    }
    let mut ids = Vec::new();
    for name in ["Reception", "Spa"] {
        ids.push(
            sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO projects (org_id, name, created_by) VALUES ($1, $2, $3) RETURNING id",
            )
            .bind(org)
            .bind(name)
            .bind(owner.1)
            .fetch_one(&srv.pool)
            .await
            .unwrap(),
        );
    }
    let mut desks = Vec::new();
    for (i, name) in ["Front desk", "Spa counter"].iter().enumerate() {
        desks.push(
            sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO live_desks (org_id, project_id, name, default_lang, public_id) VALUES ($1, $2, $3, 'it', $4) RETURNING id",
            )
            .bind(org)
            .bind(ids[i])
            .bind(name)
            .bind(format!("d{}", &Uuid::new_v4().simple().to_string()[..11]))
            .fetch_one(&srv.pool)
            .await
            .unwrap(),
        );
    }
    World {
        org,
        owner,
        member_a,
        member_b,
        project_a: ids[0],
        project_b: ids[1],
        desk1: desks[0],
        desk2: desks[1],
    }
}

struct Seeded {
    session: Uuid,
    call: Uuid,
    guest_peer: Uuid,
    staff_peer: Option<Uuid>,
}

/// A conversation as the live path leaves it: the session, its call-session row, a guest and
/// (optionally) a staff member.
#[allow(clippy::too_many_arguments)]
async fn seed(
    srv: &Srv,
    w: &World,
    desk: Uuid,
    project: Uuid,
    status: &str,
    guest: &str,
    staff: Option<(Uuid, &str)>,
    created: DateTime<Utc>,
) -> Seeded {
    let session = Uuid::new_v4();
    let call = Uuid::new_v4();
    sqlx::query("INSERT INTO call_sessions (id, room, kind, org_id, project_id) VALUES ($1, $2, 'live_connect', $3, $4)")
        .bind(call)
        .bind(format!("live-{session}"))
        .bind(w.org)
        .bind(project)
        .execute(&srv.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO live_sessions (id, call_session_id, org_id, project_id, desk_id, status, host_lang, expires_at, created_at, started_at, ended_at, end_reason)
         VALUES ($1, $2, $3, $4, $5, $6, 'it', now() + interval '1 hour', $7,
                 CASE WHEN $6 IN ('active', 'ended') THEN $7 END,
                 CASE WHEN $6 = 'ended' THEN $7 + interval '90 seconds' END,
                 CASE WHEN $6 = 'ended' THEN 'ended_by_guest' END)",
    )
    .bind(session)
    .bind(call)
    .bind(w.org)
    .bind(project)
    .bind(desk)
    .bind(status)
    .bind(created)
    .execute(&srv.pool)
    .await
    .unwrap();
    let guest_peer: Uuid = sqlx::query_scalar(
        "INSERT INTO live_participants (session_id, role, display_name, lang) VALUES ($1, 'guest', $2, 'es') RETURNING peer_id",
    )
    .bind(session)
    .bind(guest)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    let staff_peer = match staff {
        Some((user, name)) => Some(
            sqlx::query_scalar(
                "INSERT INTO live_participants (session_id, role, user_id, display_name, lang) VALUES ($1, 'staff', $2, $3, 'it') RETURNING peer_id",
            )
            .bind(session)
            .bind(user)
            .bind(name)
            .fetch_one(&srv.pool)
            .await
            .unwrap(),
        ),
        None => None,
    };
    Seeded {
        session,
        call,
        guest_peer,
        staff_peer,
    }
}

async fn tick(srv: &Srv, s: &Seeded, seq: i64, payer: &str, credits: i32, ts: DateTime<Utc>) {
    sqlx::query("INSERT INTO live_usage_ticks (session_id, peer_id, tick_seq, payer_kind, amount, ts) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(s.session)
        .bind(s.guest_peer)
        .bind(seq)
        .bind(payer)
        .bind(rust_decimal::Decimal::from(credits))
        .bind(ts)
        .execute(&srv.pool)
        .await
        .unwrap();
}

async fn says(
    srv: &Srv,
    s: &Seeded,
    peer: Uuid,
    name: &str,
    lang: &str,
    text: &str,
    tr: Value,
    at: DateTime<Utc>,
) {
    sqlx::query(
        "INSERT INTO transcript_events (session_id, event_type, speaker_peer_id, speaker_name, original_text, original_lang, translations, ts)
         VALUES ($1, 'speech', $2, $3, $4, $5, $6, $7)",
    )
    .bind(s.call)
    .bind(peer.to_string())
    .bind(name)
    .bind(text)
    .bind(lang)
    .bind(tr)
    .bind(at)
    .execute(&srv.pool)
    .await
    .unwrap();
}

fn ids(list: &Value) -> Vec<String> {
    let mut v: Vec<String> = list["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

fn sorted(ids: &[Uuid]) -> Vec<String> {
    let mut v: Vec<String> = ids.iter().map(Uuid::to_string).collect();
    v.sort();
    v
}

// ---- the list --------------------------------------------------------------------------

#[tokio::test]
async fn an_admin_sees_every_conversation_a_member_only_their_own_and_a_stranger_none() {
    let srv = srv!();
    let w = world(&srv).await;
    let now = Utc::now();
    let s1 = seed(
        &srv,
        &w,
        w.desk1,
        w.project_a,
        "ended",
        "Lucia",
        Some((w.member_a.1, "Anna")),
        now - Duration::days(2),
    )
    .await;
    let s2 = seed(
        &srv,
        &w,
        w.desk2,
        w.project_b,
        "active",
        "Pedro",
        Some((w.owner.1, "Owner")),
        now - Duration::hours(1),
    )
    .await;
    let s3 = seed(
        &srv,
        &w,
        w.desk1,
        w.project_a,
        "expired",
        "Nobody",
        None,
        now - Duration::days(10),
    )
    .await;
    let list = format!("/api/business/organizations/{}/live/sessions", w.org);

    let (s, all) = srv.get(&list, &w.owner.0).await;
    assert_eq!(s, StatusCode::OK, "{all}");
    assert_eq!(ids(&all), sorted(&[s1.session, s2.session, s3.session]));
    assert_eq!(all["total"], 3);

    // A member sees only the conversation they sat in; another member sees none.
    let (_, mine) = srv.get(&list, &w.member_a.0).await;
    assert_eq!(ids(&mine), sorted(&[s1.session]));
    let (_, none) = srv.get(&list, &w.member_b.0).await;
    assert_eq!(none["total"], 0);

    // Someone outside the org cannot even tell it exists.
    let stranger = login(&srv, "Stranger").await;
    assert_eq!(srv.get(&list, &stranger.0).await.0, StatusCode::NOT_FOUND);

    // What a row says about the conversation.
    let row = all["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == json!(s1.session))
        .unwrap();
    assert_eq!(row["status"], "ended");
    assert_eq!(row["end_reason"], "ended_by_guest");
    assert_eq!(row["duration_secs"], 90);
    assert_eq!(row["desk"]["name"], "Front desk");
    assert_eq!(row["project"]["name"], "Reception");
    assert_eq!(row["guest"]["name"], "Lucia");
    assert_eq!(row["guest"]["lang"], "es");
    assert_eq!(row["staff_name"], "Anna");
    assert_eq!(row["has_transcript"], false);
}

#[tokio::test]
async fn the_list_filters_by_status_project_desk_and_date_and_pages() {
    let srv = srv!();
    let w = world(&srv).await;
    let now = Utc::now();
    let s1 = seed(
        &srv,
        &w,
        w.desk1,
        w.project_a,
        "ended",
        "A",
        None,
        now - Duration::days(2),
    )
    .await;
    let s2 = seed(
        &srv,
        &w,
        w.desk2,
        w.project_b,
        "active",
        "B",
        None,
        now - Duration::hours(1),
    )
    .await;
    let s3 = seed(
        &srv,
        &w,
        w.desk1,
        w.project_a,
        "expired",
        "C",
        None,
        now - Duration::days(10),
    )
    .await;
    let base = format!("/api/business/organizations/{}/live/sessions", w.org);
    let get = |q: String| {
        let (srv, tok, base) = (&srv, w.owner.0.clone(), base.clone());
        async move { srv.get(&format!("{base}?{q}"), &tok).await }
    };

    assert_eq!(
        ids(&get("status=active".into()).await.1),
        sorted(&[s2.session])
    );
    assert_eq!(
        ids(&get(format!("project_id={}", w.project_a)).await.1),
        sorted(&[s1.session, s3.session])
    );
    assert_eq!(
        ids(&get(format!("desk_id={}", w.desk2)).await.1),
        sorted(&[s2.session])
    );
    let from = (now - Duration::days(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    assert_eq!(
        ids(&get(format!("from={from}")).await.1),
        sorted(&[s1.session, s2.session])
    );
    let to = (now - Duration::days(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    assert_eq!(ids(&get(format!("to={to}")).await.1), sorted(&[s3.session]));
    assert_eq!(
        ids(&get(format!("status=ended&desk_id={}", w.desk2)).await.1),
        Vec::<String>::new()
    );

    // Bad input is refused; paging is clamped and counts the filtered total.
    let (s, b) = get("status=weird".into()).await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("bad_status"))
    );
    let (_, page) = get("limit=1&offset=1".into()).await;
    assert_eq!(
        (
            page["sessions"].as_array().unwrap().len(),
            page["total"].as_i64(),
            page["limit"].as_i64()
        ),
        (1, Some(3), Some(1))
    );
    let (_, huge) = get("limit=100000".into()).await;
    assert_eq!(huge["limit"], 100);
}

// ---- one conversation ------------------------------------------------------------------

#[tokio::test]
async fn a_conversation_shows_what_was_said_with_the_original_beside_the_translation() {
    let srv = srv!();
    let w = world(&srv).await;
    let t0 = Utc::now() - Duration::minutes(10);
    let s = seed(
        &srv,
        &w,
        w.desk1,
        w.project_a,
        "ended",
        "Lucia",
        Some((w.member_a.1, "Anna")),
        t0,
    )
    .await;
    let staff = s.staff_peer.unwrap();
    says(
        &srv,
        &s,
        s.guest_peer,
        "Lucia",
        "es",
        "¿Dónde está el parking?",
        json!({ "it": "Dov'è il parcheggio?" }),
        t0,
    )
    .await;
    says(
        &srv,
        &s,
        staff,
        "Anna",
        "it",
        "Al piano meno uno.",
        json!({ "es": "En el sótano." }),
        t0 + Duration::seconds(5),
    )
    .await;
    let path = |id: Uuid| format!("/api/business/organizations/{}/live/sessions/{id}", w.org);

    // The staff member who took part reads it, in their language.
    let (st, body) = srv
        .get(&format!("{}?lang=it", path(s.session)), &w.member_a.0)
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let lines = body["utterances"].as_array().unwrap();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["speaker_name"], "Lucia");
    assert_eq!(
        lines[0]["original"], "¿Dónde está el parking?",
        "the original is what was said, never replaced"
    );
    assert_eq!(lines[0]["original_lang"], "es");
    assert_eq!(lines[0]["translation"], "Dov'è il parcheggio?");
    assert_eq!(
        lines[1]["translation"],
        Value::Null,
        "no translation into `it` of an utterance that was already Italian"
    );
    assert_eq!(lines[1]["original"], "Al piano meno uno.");
    assert_eq!(lines[1]["offset_ms"], 5000);
    // Asking for no language still returns every translation that was captured.
    let (_, raw) = srv.get(&path(s.session), &w.member_a.0).await;
    assert_eq!(
        raw["utterances"][0]["translations"]["it"],
        "Dov'è il parcheggio?"
    );
    assert_eq!(raw["utterances"][0]["translation"], Value::Null);
    assert_eq!(body["participants"].as_array().unwrap().len(), 2);

    // How the call went is for administrators.
    assert!(
        body.get("events").is_none(),
        "a member must not see the diagnostic timeline"
    );
    sqlx::query(
        "INSERT INTO live_session_events (session_id, kind, detail) VALUES ($1, 'connected', '{}')",
    )
    .bind(s.session)
    .execute(&srv.pool)
    .await
    .unwrap();
    let (_, admin) = srv.get(&path(s.session), &w.owner.0).await;
    assert_eq!(admin["events"][0]["kind"], "connected");

    // A member who was not in it, and one conversation of another org, both look like nothing.
    let (s_other, missing) = srv.get(&path(s.session), &w.member_b.0).await;
    let (s_none, unknown) = srv.get(&path(Uuid::new_v4()), &w.member_b.0).await;
    assert_eq!(
        (s_other, s_none),
        (StatusCode::NOT_FOUND, StatusCode::NOT_FOUND)
    );
    assert_eq!(missing, unknown, "no oracle for which conversations exist");
    let w2 = world(&srv).await;
    let (s_cross, _) = srv
        .get(
            &format!(
                "/api/business/organizations/{}/live/sessions/{}",
                w2.org, s.session
            ),
            &w2.owner.0,
        )
        .await;
    assert_eq!(
        s_cross,
        StatusCode::NOT_FOUND,
        "an admin of another org cannot read it through their own org"
    );
}

// ---- usage -----------------------------------------------------------------------------

#[tokio::test]
async fn usage_is_for_admins_and_counts_only_the_orgs_own_metered_ticks() {
    let srv = srv!();
    let w = world(&srv).await;
    let now = Utc::now();
    let day = |d: i64| now - Duration::days(d);
    let s1 = seed(&srv, &w, w.desk1, w.project_a, "ended", "A", None, day(2)).await;
    let s2 = seed(&srv, &w, w.desk2, w.project_b, "active", "B", None, day(0)).await;
    tick(&srv, &s1, 1, "org", 5, day(2)).await;
    tick(&srv, &s1, 2, "org", 3, day(2)).await;
    tick(&srv, &s2, 1, "org", 4, day(0)).await;
    // A consumer-paid tick (not this org's money) is not counted.
    tick(&srv, &s2, 2, "user", 99, day(0)).await;
    // Another org's usage never appears.
    let other = world(&srv).await;
    let foreign = seed(
        &srv,
        &other,
        other.desk1,
        other.project_a,
        "ended",
        "X",
        None,
        day(1),
    )
    .await;
    tick(&srv, &foreign, 1, "org", 1000, day(1)).await;

    let usage = |q: &str| format!("/api/business/organizations/{}/live/usage{q}", w.org);
    let (s, by_desk) = srv.get(&usage(""), &w.owner.0).await;
    assert_eq!(s, StatusCode::OK, "{by_desk}");
    let rows = by_desk["rows"].as_array().unwrap();
    let row = |label: &str| {
        rows.iter()
            .find(|r| r["label"] == label)
            .unwrap_or_else(|| panic!("no row {label}: {by_desk}"))
    };
    assert_eq!(
        (
            row("Front desk")["credits"].as_i64(),
            row("Front desk")["sessions"].as_i64()
        ),
        (Some(8), Some(1))
    );
    assert_eq!(row("Spa counter")["credits"], 4);
    assert_eq!(by_desk["totals"], json!({ "sessions": 2, "credits": 12 }));
    assert_eq!(
        by_desk["balance"], 777,
        "the wallet balance comes from the backend"
    );
    // The conversation still open, with what it has cost so far.
    let active = by_desk["active"].as_array().unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(
        (
            active[0]["session_id"].clone(),
            active[0]["credits"].as_i64()
        ),
        (json!(s2.session), Some(4))
    );

    let (_, by_project) = srv.get(&usage("?group=project"), &w.owner.0).await;
    assert_eq!(
        by_project["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["label"] == "Reception")
            .unwrap()["credits"],
        8
    );
    let (_, by_day) = srv.get(&usage("?group=day"), &w.owner.0).await;
    assert_eq!(
        by_day["rows"].as_array().unwrap().len(),
        2,
        "two different days: {by_day}"
    );

    // A date range narrows it; bad input is refused.
    let from = (now - Duration::days(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let (_, narrow) = srv.get(&usage(&format!("?from={from}")), &w.owner.0).await;
    assert_eq!(narrow["totals"]["credits"], 4);
    let (s, b) = srv.get(&usage("?group=ssn"), &w.owner.0).await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("bad_group"))
    );
    let (s, b) = srv
        .get(&usage(&format!("?from={from}&to={from}")), &w.owner.0)
        .await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("bad_range"))
    );

    // Money is for admins; a stranger cannot tell the org exists.
    assert_eq!(
        srv.get(&usage(""), &w.member_a.0).await.0,
        StatusCode::FORBIDDEN
    );
    let stranger = login(&srv, "Stranger").await;
    assert_eq!(
        srv.get(&usage(""), &stranger.0).await.0,
        StatusCode::NOT_FOUND
    );
}
