//! The host's STT ingest socket for a webinar, end to end.
//!
//! A broadcast does NOT use the call path's per-listener shape by accident: it opens
//! one Qwen session per distinct VIEWER language and reconciles that set every
//! second, so a viewer who arrives mid-talk is translated for and a language nobody
//! is watching stops costing money. None of that could be tested without a provider;
//! `QwenConfig::endpoint` now points at a stand-in, so it is.
//!
//! DB-gated: skipped without `DATABASE_URL`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt as _, StreamExt as _};
use reqwest::Client;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::{Config, QwenConfig, WebinarConfig};
use voxtranslate_server::db::{self, Pool};
use voxtranslate_server::engine::realtime_mock::{Dialect, RealtimeMock, Reply};
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::{app, AppState};

const SECRET: &str = "webinar-stt-secret";

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Server {
    addr: SocketAddr,
    pool: Pool,
}

fn base(srv: &Server) -> String {
    format!("http://{}", srv.addr)
}

async fn setup(mock: &RealtimeMock) -> Option<Server> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let mut config = Config::test_with_billing(&url, SECRET, 0.0);
    config.webinar = Some(WebinarConfig::test_default());
    config.qwen = QwenConfig {
        api_key: "test-key".into(),
        endpoint: mock.base_url(),
        ..Default::default()
    };
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

macro_rules! srv {
    ($mock:expr) => {
        match setup($mock).await {
            Some(s) => s,
            None => {
                eprintln!("skipping — no DATABASE_URL");
                return;
            }
        }
    };
}

async fn user(srv: &Server) -> (Uuid, String) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: "Host".into(),
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
         VALUES ('Acme', $1, $2, 'active', now() + interval '30 days') RETURNING id",
    )
    .bind(format!("org-{}", Uuid::new_v4().simple()))
    .bind(owner)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO organization_members (org_id, user_id, role) VALUES ($1,$2,'owner')")
        .bind(org_id)
        .bind(owner)
        .execute(&srv.pool)
        .await
        .unwrap();
    org_id
}

/// A webinar in `status`, created through the API so it carries whatever defaults
/// the product actually gives it.
async fn webinar(
    http: &Client,
    srv: &Server,
    jwt: &str,
    org_id: Uuid,
    status: &str,
) -> (Uuid, String) {
    let r = http
        .post(format!("{}/api/webinars", base(srv)))
        .bearer_auth(jwt)
        .json(&json!({ "org_id": org_id, "title": "Launch", "source_language": "en" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "create webinar");
    let body: Value = r.json().await.unwrap();
    let id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let code = body["code"].as_str().unwrap().to_string();
    sqlx::query("UPDATE webinars SET status = $2 WHERE id = $1")
        .bind(id)
        .bind(status)
        .execute(&srv.pool)
        .await
        .unwrap();
    (id, code)
}

/// Open the host ingest socket, returning the HTTP status when it is refused.
async fn ingest(srv: &Server, id: Uuid, query: &str) -> Result<Ws, u16> {
    let url = format!("ws://{}/api/webinars/{id}/stt?{query}", srv.addr);
    match tokio_tungstenite::connect_async(url).await {
        Ok((ws, _)) => Ok(ws),
        Err(tokio_tungstenite::tungstenite::Error::Http(r)) => Err(r.status().as_u16()),
        Err(e) => panic!("unexpected transport failure: {e}"),
    }
}

/// A viewer watching in `lang`. Their presence is what decides the fan-out.
async fn viewer(srv: &Server, code: &str, lang: &str) -> Ws {
    let url = format!(
        "ws://{}/api/w/{code}/presence?lang={lang}&guest_id={}",
        srv.addr,
        Uuid::new_v4()
    );
    let (ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("the presence channel accepted the viewer");
    ws
}

/// Stream audio for up to `ms`, giving the reconcile loop (1 s) room to run.
async fn talk(ws: &mut Ws, ms: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    while tokio::time::Instant::now() < deadline {
        if ws.send(Message::binary(vec![0u8; 4800])).await.is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ---------------------------------------------------------------------------
// The gates, before the upgrade
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_socket_without_a_token_is_refused_with_a_status_not_a_half_open_stream() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let http = Client::new();
    let (uid, jwt) = user(&srv).await;
    let o = org(&srv, uid).await;
    let (id, _) = webinar(&http, &srv, &jwt, o, "live").await;

    // Everything is authorized BEFORE the upgrade, so the caller gets a clean HTTP
    // status it can act on rather than a socket that silently does nothing.
    assert_eq!(ingest(&srv, id, "").await.err(), Some(401));
    assert_eq!(ingest(&srv, id, "token=not-a-jwt").await.err(), Some(401));
}

#[tokio::test]
async fn another_orgs_member_is_told_the_webinar_does_not_exist() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let http = Client::new();
    let (host, host_jwt) = user(&srv).await;
    let o = org(&srv, host).await;
    let (id, _) = webinar(&http, &srv, &host_jwt, o, "live").await;

    let (outsider, outsider_jwt) = user(&srv).await;
    org(&srv, outsider).await; // their own org, not this one

    // Cross-tenant is a 404, not a 403: confirming the id exists would leak which
    // webinars another company is running.
    assert_eq!(
        ingest(&srv, id, &format!("token={outsider_jwt}"))
            .await
            .err(),
        Some(404)
    );
}

#[tokio::test]
async fn a_webinar_that_is_over_cannot_be_streamed_into() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let http = Client::new();
    let (uid, jwt) = user(&srv).await;
    let o = org(&srv, uid).await;

    for status in ["ended", "cancelled"] {
        let (id, _) = webinar(&http, &srv, &jwt, o, status).await;
        assert_eq!(
            ingest(&srv, id, &format!("token={jwt}")).await.err(),
            Some(409),
            "{status}"
        );
    }
}

#[tokio::test]
async fn a_scheduled_webinar_accepts_the_host_warming_the_microphone() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let http = Client::new();
    let (uid, jwt) = user(&srv).await;
    let o = org(&srv, uid).await;
    let (id, _) = webinar(&http, &srv, &jwt, o, "scheduled").await;

    // Rejecting this would mean the host discovers their microphone is broken in
    // front of the audience.
    assert!(ingest(&srv, id, &format!("token={jwt}")).await.is_ok());
}

#[tokio::test]
async fn only_one_stream_at_a_time_may_feed_a_webinar() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let http = Client::new();
    let (uid, jwt) = user(&srv).await;
    let o = org(&srv, uid).await;
    let (id, _) = webinar(&http, &srv, &jwt, o, "live").await;

    let first = ingest(&srv, id, &format!("token={jwt}"))
        .await
        .expect("first");
    // A reconnect race — or a buggy client opening sockets in a loop — would
    // otherwise multiply the Qwen bill and double every subtitle.
    assert_eq!(
        ingest(&srv, id, &format!("token={jwt}")).await.err(),
        Some(409)
    );

    // The slot is held by the socket for its whole life and freed on drop, not by
    // a timer — so a host who reloads the page can start again immediately.
    drop(first);
    let mut freed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(3000);
    while tokio::time::Instant::now() < deadline {
        if ingest(&srv, id, &format!("token={jwt}")).await.is_ok() {
            freed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(freed, "the single-flight slot was never released");
}

// ---------------------------------------------------------------------------
// The fan-out
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_host_talking_to_an_empty_room_opens_no_upstream_session() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let http = Client::new();
    let (uid, jwt) = user(&srv).await;
    let o = org(&srv, uid).await;
    let (id, _) = webinar(&http, &srv, &jwt, o, "live").await;

    let mut host = ingest(&srv, id, &format!("token={jwt}"))
        .await
        .expect("host");
    talk(&mut host, 1500).await;

    // Nobody is watching, so there is no language to translate into — and a
    // broadcast bills per language.
    assert_eq!(mock.connections(), 0, "an empty room cost money");
}

#[tokio::test]
async fn a_viewer_in_another_language_is_translated_for_within_a_reconcile() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let http = Client::new();
    let (uid, jwt) = user(&srv).await;
    let o = org(&srv, uid).await;
    let (id, code) = webinar(&http, &srv, &jwt, o, "live").await;

    let _watcher = viewer(&srv, &code, "it").await;
    let mut host = ingest(&srv, id, &format!("token={jwt}"))
        .await
        .expect("host");
    talk(&mut host, 2500).await;

    assert!(
        mock.connections() >= 1,
        "a watching viewer was never translated for"
    );
}

#[tokio::test]
async fn a_viewer_watching_in_the_hosts_own_language_needs_no_translation() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let http = Client::new();
    let (uid, jwt) = user(&srv).await;
    let o = org(&srv, uid).await;
    let (id, code) = webinar(&http, &srv, &jwt, o, "live").await;

    // The webinar is in English and so is the viewer.
    let _watcher = viewer(&srv, &code, "en").await;
    let mut host = ingest(&srv, id, &format!("token={jwt}"))
        .await
        .expect("host");
    talk(&mut host, 2500).await;

    assert_eq!(
        mock.connections(),
        0,
        "the host was translated into the language they were already speaking"
    );
}

#[tokio::test]
async fn two_viewer_languages_are_two_sessions_and_a_third_viewer_is_not() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let http = Client::new();
    let (uid, jwt) = user(&srv).await;
    let o = org(&srv, uid).await;
    let (id, code) = webinar(&http, &srv, &jwt, o, "live").await;

    let _a = viewer(&srv, &code, "it").await;
    let _b = viewer(&srv, &code, "fr").await;
    let _c = viewer(&srv, &code, "it").await; // same language as _a
    let mut host = ingest(&srv, id, &format!("token={jwt}"))
        .await
        .expect("host");
    talk(&mut host, 2500).await;

    // The cost scales with DISTINCT languages, not with the audience — which is
    // the whole reason a broadcast reconciles a set rather than a list of viewers.
    assert!(
        mock.connections() >= 2,
        "two languages did not open two sessions"
    );
}

#[tokio::test]
async fn a_transcript_reaches_the_viewer_watching_that_language() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    mock.reply_with(vec![Reply::Transcript {
        original: "welcome everybody".into(),
        translated: "benvenuti a tutti".into(),
    }]);
    let srv = srv!(&mock);
    let http = Client::new();
    let (uid, jwt) = user(&srv).await;
    let o = org(&srv, uid).await;
    let (id, code) = webinar(&http, &srv, &jwt, o, "live").await;

    let mut watcher = viewer(&srv, &code, "it").await;
    let mut host = ingest(&srv, id, &format!("token={jwt}"))
        .await
        .expect("host");

    let talking = tokio::spawn(async move {
        talk(&mut host, 4000).await;
        host
    });

    let mut saw_subtitle = false;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(6000);
    while tokio::time::Instant::now() < deadline {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, watcher.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    if v["type"].as_str().is_some_and(|k| k.contains("subtitle")) {
                        saw_subtitle = true;
                        break;
                    }
                }
            }
            Ok(Some(Ok(_))) => {}
            _ => break,
        }
    }
    let _ = talking.await;
    assert!(saw_subtitle, "the viewer never received a subtitle");
}

#[tokio::test]
async fn the_host_hanging_up_ends_every_language_session() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let http = Client::new();
    let (uid, jwt) = user(&srv).await;
    let o = org(&srv, uid).await;
    let (id, code) = webinar(&http, &srv, &jwt, o, "live").await;

    let _watcher = viewer(&srv, &code, "it").await;
    let mut host = ingest(&srv, id, &format!("token={jwt}"))
        .await
        .expect("host");
    talk(&mut host, 2500).await;
    let opened = mock.connections();
    assert!(opened >= 1);

    let _ = host.close(None).await;
    drop(host);
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // Dropping the host's channel closes every language feed. If it did not, each
    // one would sit there reconnecting to Qwen and billing for silence.
    assert_eq!(
        mock.connections(),
        opened,
        "a language session reconnected after the host left"
    );
}

#[tokio::test]
async fn text_frames_on_the_ingest_socket_are_ignored() {
    let mock = RealtimeMock::start(Dialect::Qwen).await;
    let srv = srv!(&mock);
    let http = Client::new();
    let (uid, jwt) = user(&srv).await;
    let o = org(&srv, uid).await;
    let (id, code) = webinar(&http, &srv, &jwt, o, "live").await;

    let _watcher = viewer(&srv, &code, "it").await;
    let mut host = ingest(&srv, id, &format!("token={jwt}"))
        .await
        .expect("host");
    // There is no start/stop protocol here: the stream itself is the signal, so a
    // client that invents one must not be able to break the session.
    let _ = host
        .send(Message::text(json!({ "type": "start" }).to_string()))
        .await;
    let _ = host.send(Message::text("{}".to_string())).await;
    talk(&mut host, 2500).await;

    assert!(mock.connections() >= 1, "a text frame killed the session");
}
