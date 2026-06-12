//! Room glossary (spec 0011) end-to-end: REST CRUD with validation + CSV
//! import over real HTTP, and the `glossary_active` WebSocket badge on join
//! and after live edits.
//!
//! Every test is **DB-gated**: it no-ops when `DATABASE_URL` is unset. Locally,
//! run against the Docker Postgres:
//! `DATABASE_URL=postgres://postgres:postgres@localhost:5433/voxtest cargo test --test glossary`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::Config;
use voxtranslate_server::db::{self, Pool};
use voxtranslate_server::glossary::GlossaryService;
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::transcripts::TranscriptService;
use voxtranslate_server::{app, AppState};

/// Small cap so the "too many entries" 400 is cheap to trigger.
const TEST_MAX_ENTRIES: usize = 5;

struct Server {
    addr: SocketAddr,
    pool: Pool,
    secret: String,
}

async fn setup() -> Option<Server> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let secret = "glossary-secret".to_string();
    let config = Config::test_with_billing(&url, &secret, 2.0);
    let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);
    let mut state = AppState::new(config);
    state.billing = Some(BillingService::new(pool.clone(), min_join));
    state.safety = Some(SafetyService::new(pool.clone()));
    state.transcripts = Some(TranscriptService::new(pool.clone()));
    state.glossary = Some(GlossaryService::new(pool.clone(), TEST_MAX_ENTRIES));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    Some(Server { addr, pool, secret })
}

async fn login(srv: &Server, name: &str) -> (Uuid, String) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: name.into(),
        avatar_url: None,
    };
    let user = upsert_google_user(&srv.pool, &identity, rust_decimal::Decimal::new(200, 2))
        .await
        .unwrap();
    let jwt = issue_jwt(&srv.secret, &user.id, &user.email, &user.name, 168).unwrap();
    (user.id, jwt)
}

fn entry(sl: &str, tl: &str, st: &str, tt: &str) -> serde_json::Value {
    serde_json::json!({
        "source_lang": sl, "target_lang": tl,
        "source_term": st, "target_term": tt,
    })
}

#[tokio::test]
async fn glossary_rest_crud_validation_and_import() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let (_uid, jwt) = login(&srv, "Tess").await;
    let room = format!("gl-{}", Uuid::new_v4().simple());
    let base = format!("http://{}/api/rooms/{room}/glossary", srv.addr);
    let http = reqwest::Client::new();

    // No token -> 401 on the mutating verbs. GET is intentionally public (the
    // room code is the access control), so it is not gated here.
    for resp in [
        http.post(&base)
            .json(&serde_json::json!({ "entries": [] }))
            .send()
            .await
            .unwrap(),
        http.delete(&base).send().await.unwrap(),
    ] {
        assert_eq!(resp.status(), 401);
    }
    // GET without a token is allowed (public read).
    let resp = http.get(&base).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    // Fresh room -> empty glossary, advertised cap.
    let resp = http.get(&base).bearer_auth(&jwt).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["name"].is_null());
    assert_eq!(body["entries"].as_array().unwrap().len(), 0);
    assert_eq!(body["max_entries"], TEST_MAX_ENTRIES);

    // Save: trims the name, lowercases langs, dedupes last-wins.
    let resp = http
        .post(&base)
        .bearer_auth(&jwt)
        .json(&serde_json::json!({
            "name": "  Legal  ",
            "entries": [
                entry(" IT ", " EN ", " fattura ", "invoice"),
                entry("en", "it", "deck", "presentazione"),
                entry("it", "en", "fattura", "bill"), // same key — wins
            ],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["name"], "Legal");
    let entries = body["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    // Canonical order is alphabetical by (source_lang, target_lang, term).
    assert_eq!(entries[0]["source_lang"], "en");
    assert_eq!(entries[0]["source_term"], "deck");
    assert_eq!(entries[1]["source_term"], "fattura");
    assert_eq!(entries[1]["target_term"], "bill");
    assert!(entries[0]["id"].is_string(), "entries carry ids");

    // Validation 400s: over the cap, and a bad entry with its 1-based index.
    let too_many: Vec<_> = (0..TEST_MAX_ENTRIES + 1)
        .map(|i| entry("it", "en", &format!("t{i}"), "x"))
        .collect();
    let resp = http
        .post(&base)
        .bearer_auth(&jwt)
        .json(&serde_json::json!({ "entries": too_many }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert!(resp.text().await.unwrap().contains("too many entries"));
    let resp = http
        .post(&base)
        .bearer_auth(&jwt)
        .json(&serde_json::json!({ "entries": [entry("it", "en", "  ", "x")] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert!(resp.text().await.unwrap().contains("entry 1"));

    // CSV import merges into the saved glossary; imported rows override.
    let csv = "source_lang,target_lang,source_term,target_term\n\
               it,en,preventivo,quote\n\
               it,en,fattura,\"final invoice\"\n";
    let resp = http
        .post(format!("{base}/import"))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({ "csv": csv }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["name"], "Legal", "import keeps the name");
    let entries = body["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    let fattura = entries
        .iter()
        .find(|e| e["source_term"] == "fattura")
        .unwrap();
    assert_eq!(fattura["target_term"], "final invoice");

    // Import 400s: malformed line (with its number) and an empty file.
    let resp = http
        .post(format!("{base}/import"))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({ "csv": "it,en,solo-tre" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert!(resp.text().await.unwrap().contains("line 1"));
    let resp = http
        .post(format!("{base}/import"))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({ "csv": "\n\n" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert!(resp.text().await.unwrap().contains("no entries"));

    // Delete is idempotent and leaves an empty glossary behind.
    let resp = http.delete(&base).bearer_auth(&jwt).send().await.unwrap();
    assert_eq!(resp.status(), 204);
    let resp = http.delete(&base).bearer_auth(&jwt).send().await.unwrap();
    assert_eq!(resp.status(), 204);
    let body: serde_json::Value = http
        .get(&base)
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(body["name"].is_null());
    assert_eq!(body["entries"].as_array().unwrap().len(), 0);
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(addr: SocketAddr, params: &str) -> Ws {
    let url = format!("ws://{addr}/ws?{params}");
    let (ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("ws connect");
    ws
}

/// Wait until a frame of the given type arrives on the socket.
async fn wait_for(ws: &mut Ws, frame_type: &str) -> serde_json::Value {
    loop {
        match tokio::time::timeout(Duration::from_secs(3), ws.next()).await {
            Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t)))) => {
                let v: serde_json::Value = serde_json::from_str(t.as_str()).unwrap();
                if v["type"] == frame_type {
                    return v;
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => panic!("no {frame_type} frame"),
        }
    }
}

#[tokio::test]
async fn glossary_badge_on_join_and_live_updates() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let (_uid, jwt) = login(&srv, "Tess").await;
    let room = format!("gl-{}", Uuid::new_v4().simple());
    let base = format!("http://{}/api/rooms/{room}/glossary", srv.addr);
    let http = reqwest::Client::new();

    // Seed a glossary before anyone joins.
    let resp = http
        .post(&base)
        .bearer_auth(&jwt)
        .json(&serde_json::json!({
            "name": "Sales",
            "entries": [entry("it", "en", "fattura", "invoice")],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Authed joiner gets the badge right after room_joined.
    let mut ws = connect(
        srv.addr,
        &format!("room={room}&lang=it&id=tess-peer&token={jwt}"),
    )
    .await;
    wait_for(&mut ws, "room_joined").await;
    let badge = wait_for(&mut ws, "glossary_active").await;
    assert_eq!(badge["name"], "Sales");
    assert_eq!(badge["entries"], 1);

    // A guest in the same (private) room sees the badge too.
    let mut guest = connect(srv.addr, &format!("room={room}&lang=en&id=guest-peer")).await;
    wait_for(&mut guest, "room_joined").await;
    let badge = wait_for(&mut guest, "glossary_active").await;
    assert_eq!(badge["entries"], 1);

    // A live edit re-broadcasts the badge to everyone in the room.
    let resp = http
        .post(&base)
        .bearer_auth(&jwt)
        .json(&serde_json::json!({
            "name": "Sales v2",
            "entries": [
                entry("it", "en", "fattura", "invoice"),
                entry("it", "en", "preventivo", "quote"),
            ],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    for ws in [&mut ws, &mut guest] {
        let badge = wait_for(ws, "glossary_active").await;
        assert_eq!(badge["name"], "Sales v2");
        assert_eq!(badge["entries"], 2);
    }

    // Deleting broadcasts entries: 0 (the client hides the badge).
    assert_eq!(
        http.delete(&base)
            .bearer_auth(&jwt)
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    let badge = wait_for(&mut ws, "glossary_active").await;
    assert_eq!(badge["entries"], 0);
    assert!(badge.get("name").is_none());
}
