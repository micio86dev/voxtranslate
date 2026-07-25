//! Transcript capture + export end-to-end over real HTTP/WebSocket: an authed
//! user chats in a call, the event is persisted, the session is listed, and the
//! JSON transcript downloads with the right auth gates (401/403/404).
//!
//! Every test is **DB-gated**: it no-ops when `DATABASE_URL` is unset. Locally,
//! run against the Docker Postgres:
//! `DATABASE_URL=postgresql://postgres:test@127.0.0.1:55432/vox_test cargo test --test transcripts`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::{Config, ResendConfig};
use voxtranslate_server::db::{self, Pool};
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::transcripts::{EventKind, TranscriptEvent, TranscriptService};
use voxtranslate_server::{app, AppState};

struct Server {
    addr: SocketAddr,
    pool: Pool,
    secret: String,
}

/// Spawn a billing-mode server with the transcript service wired (unlike
/// `tests/billing.rs`, which predates transcripts and leaves it `None`).
async fn setup() -> Option<Server> {
    setup_opts(false).await
}

/// Like [`setup`], optionally wiring a (dummy-key) Resend config so the email
/// endpoints pass their 503 feature gate. The key is fake: a real send dies at
/// Resend's 401, which is exactly what the failure-path tests want.
async fn setup_opts(with_resend: bool) -> Option<Server> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let secret = "transcripts-secret".to_string();
    let mut config = Config::test_with_billing(&url, &secret, 2.0);
    if with_resend {
        config.resend = Some(ResendConfig {
            api_key: "dummy".into(),
            from_email: "noreply@example.com".into(),
            from_name: "VoxTranslate".into(),
        });
    }
    let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);
    let mut state = AppState::new(config);
    state.billing = Some(BillingService::new(pool.clone(), min_join));
    state.safety = Some(SafetyService::new(pool.clone()));
    state.transcripts = Some(TranscriptService::new(pool.clone()));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    Some(Server { addr, pool, secret })
}

/// Poll a background AI job until it leaves `pending`, returning the final job
/// JSON. The heavy AI POSTs (report / correction / email draft) now return a job
/// handle and the result/failure surfaces here, not on the POST.
async fn poll_ai_job(
    http: &reqwest::Client,
    base: &str,
    session_id: Uuid,
    job_id: &str,
    jwt: &str,
) -> serde_json::Value {
    let url = format!("{base}/api/sessions/{session_id}/ai-job/{job_id}");
    for _ in 0..100 {
        let r = http.get(&url).bearer_auth(jwt).send().await.unwrap();
        assert_eq!(r.status(), 200, "ai-job poll must be readable");
        let job: serde_json::Value = r.json().await.unwrap();
        if job["status"] != "pending" {
            return job;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("ai job {job_id} never left pending");
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Connect and return the first JSON text frame (keeping the socket open).
async fn connect_first(addr: SocketAddr, params: &str) -> (serde_json::Value, Ws) {
    let url = format!("ws://{addr}/ws?{params}");
    let (mut ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("ws connect");
    let frame = loop {
        match tokio::time::timeout(Duration::from_secs(2), ws.next()).await {
            Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t)))) => {
                break serde_json::from_str(t.as_str()).unwrap()
            }
            Ok(Some(Ok(_))) => continue,
            _ => panic!("no frame"),
        }
    };
    (frame, ws)
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

async fn login(srv: &Server, name: &str) -> (Uuid, String) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: name.into(),
        avatar_url: None,
    };
    let (user, _) = upsert_google_user(
        &srv.pool,
        &identity,
        rust_decimal::Decimal::new(200, 2),
        None,
        None,
    )
    .await
    .unwrap();
    // Clear the 18+/ToS consent gate (`authorize` → `has_consented`) so this
    // authed user can join over WS. Real users consent via `POST /api/user/consent`.
    sqlx::query("UPDATE users SET age_confirmed = TRUE, consent_tos_at = now() WHERE id = $1")
        .bind(user.id)
        .execute(&srv.pool)
        .await
        .unwrap();
    let jwt = issue_jwt(&srv.secret, &user.id, &user.email, &user.name, 168).unwrap();
    (user.id, jwt)
}

#[tokio::test]
async fn chat_is_captured_listed_and_downloadable_with_auth_gates() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let (_uid, jwt) = login(&srv, "Tess").await;
    let room = format!("tr-{}", Uuid::new_v4().simple());

    // Authed join: room_joined carries the session id (recording is on).
    // Single-peer room -> chat fan-out has zero target languages -> no Groq.
    let (frame, mut ws) = connect_first(
        srv.addr,
        &format!("room={room}&lang=it&id=tess-peer&token={jwt}"),
    )
    .await;
    assert_eq!(frame["type"], "room_joined");
    let session_id = frame["session_id"].as_str().expect("session_id present");
    Uuid::parse_str(session_id).expect("session_id is a UUID");

    // Chat; the broadcast echoes back to the sender once the event is queued
    // for persistence (record() happens before the broadcast).
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::json!({ "type": "chat", "text": "ciao a tutti" }).to_string(),
    ))
    .await
    .unwrap();
    let chat = wait_for(&mut ws, "chat_message").await;
    assert_eq!(chat["original"], "ciao a tutti");
    drop(ws); // hang up -> participant_left + finalize_session

    let http = reqwest::Client::new();
    let base = format!("http://{}", srv.addr);

    // Poll the listing until the finalize + batch insert land. The listing's
    // own flush() can persist the event before the disconnect path stamps
    // `ended_at`, so wait for both.
    let mut listed = None;
    for _ in 0..30 {
        let rows: serde_json::Value = http
            .get(format!("{base}/api/sessions"))
            .bearer_auth(&jwt)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if let Some(row) = rows.as_array().unwrap().iter().find(|r| {
            r["id"] == session_id
                && r["event_count"].as_i64() >= Some(1)
                && r["ended_at"].is_string()
        }) {
            listed = Some(row.clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let row = listed.expect("session listed, finalized, with the chat event");
    assert_eq!(row["room"], room.as_str());

    // Download the JSON transcript.
    let resp = http
        .get(format!("{base}/api/sessions/{session_id}/transcript.json"))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/json"
    );
    let cd = resp
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .expect("content-disposition")
        .to_string();
    assert!(
        cd.starts_with("attachment; filename=\"voxtranslate-"),
        "{cd}"
    );
    assert!(cd.ends_with(".json\""), "{cd}");

    let body = resp.text().await.unwrap();
    assert!(body.contains("\n  "), "pretty-printed with 2-space indent");
    let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(doc["session"]["id"], session_id);
    assert_eq!(doc["session"]["room_name"], room.as_str());
    assert!(doc["session"]["duration_seconds"].is_number());
    assert_eq!(doc["session"]["participants"][0]["id"], "tess-peer");
    assert_eq!(doc["session"]["participants"][0]["language"], "it");
    assert_eq!(doc["events"][0]["type"], "chat");
    assert_eq!(doc["events"][0]["original"], "ciao a tutti");
    assert_eq!(doc["events"][0]["lang"], "it");
    assert!(doc["events"][0]["translations"].is_object());
    assert!(doc["exported_at"].is_string());

    // Download the PDF transcript (timezone localized).
    let pdf = http
        .get(format!(
            "{base}/api/sessions/{session_id}/transcript.pdf?tz=Europe/Rome"
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(pdf.status(), 200);
    assert_eq!(
        pdf.headers().get("content-type").unwrap(),
        "application/pdf"
    );
    let cd = pdf
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .unwrap()
        .to_string();
    assert!(cd.ends_with(".pdf\""), "{cd}");
    let bytes = pdf.bytes().await.unwrap();
    assert!(bytes.starts_with(b"%PDF-"), "PDF magic bytes");

    // A bogus timezone falls back to UTC — still a 200.
    let bogus_tz = http
        .get(format!(
            "{base}/api/sessions/{session_id}/transcript.pdf?tz=Not/AZone"
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(bogus_tz.status(), 200);

    // Rendering is rate-limited per user: 5/min, and we've spent 2 already.
    let mut last = 0;
    for _ in 0..5 {
        last = http
            .get(format!("{base}/api/sessions/{session_id}/transcript.pdf"))
            .bearer_auth(&jwt)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16();
        if last == 429 {
            break;
        }
    }
    assert_eq!(last, 429, "rapid PDF requests throttle");

    // Gates: a non-participant gets 403, unknown session 404, no token 401.
    let (_eve, eve_jwt) = login(&srv, "Eve").await;
    let forbidden = http
        .get(format!("{base}/api/sessions/{session_id}/transcript.json"))
        .bearer_auth(&eve_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(forbidden.status(), 403);

    let missing = http
        .get(format!(
            "{base}/api/sessions/{}/transcript.json",
            Uuid::new_v4()
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);

    let unauth = http
        .get(format!("{base}/api/sessions/{session_id}/transcript.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), 401);
    let unauth_list = http
        .get(format!("{base}/api/sessions"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth_list.status(), 401);

    // Eve never sees Tess's session in her own listing.
    let eve_rows: serde_json::Value = http
        .get(format!("{base}/api/sessions"))
        .bearer_auth(&eve_jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(eve_rows
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["id"] != session_id));
}

/// SRT/VTT subtitle exports (spec 0012): seeded speech events come back as
/// timed cues in the requested language mode, chat is skipped, and the same
/// auth gates as the JSON export apply.
#[tokio::test]
async fn subtitles_download_as_srt_and_vtt() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let (uid, jwt) = login(&srv, "Tess").await;
    let room = format!("st-{}", Uuid::new_v4().simple());
    let session_id = Uuid::new_v4();

    // Seed a session directly through the service (shares the server's pool).
    let svc = TranscriptService::new(srv.pool.clone());
    svc.session_started(session_id, &room).await.unwrap();
    svc.participant_joined(session_id, "tess-peer", Some(uid), "Tess", "it")
        .await
        .unwrap();
    svc.record(TranscriptEvent {
        session_id,
        kind: EventKind::Speech,
        speaker_peer_id: "tess-peer".into(),
        speaker_user_id: Some(uid),
        speaker_name: "Tess".into(),
        original_text: "Hello world.".into(),
        original_lang: "en".into(),
        translations: HashMap::from([("it".to_string(), "Ciao mondo.".to_string())]),
        ts: chrono::Utc::now(),
    });
    svc.record(TranscriptEvent {
        session_id,
        kind: EventKind::Chat,
        speaker_peer_id: "tess-peer".into(),
        speaker_user_id: Some(uid),
        speaker_name: "Tess".into(),
        original_text: "off the record".into(),
        original_lang: "en".into(),
        translations: HashMap::new(),
        ts: chrono::Utc::now() + chrono::Duration::seconds(5),
    });
    svc.flush().await;

    let http = reqwest::Client::new();
    let base = format!("http://{}", srv.addr);

    // Default mode = translated, default target = requester's lang ("it").
    let srt = http
        .get(format!("{base}/api/sessions/{session_id}/transcript.srt"))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(srt.status(), 200);
    assert_eq!(
        srt.headers().get("content-type").unwrap(),
        "application/x-subrip"
    );
    let cd = srt
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .unwrap()
        .to_string();
    assert!(cd.ends_with(".srt\""), "{cd}");
    let body = srt.text().await.unwrap();
    assert!(body.starts_with("1\n"), "{body}");
    assert!(body.contains("Tess: Ciao mondo."), "{body}");
    assert!(body.contains(" --> "), "{body}");
    assert!(
        !body.contains("off the record"),
        "chat must be skipped: {body}"
    );

    // VTT, original mode: voice tag + original text.
    let vtt = http
        .get(format!(
            "{base}/api/sessions/{session_id}/transcript.vtt?lang=original"
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(vtt.status(), 200);
    assert_eq!(vtt.headers().get("content-type").unwrap(), "text/vtt");
    let body = vtt.text().await.unwrap();
    assert!(body.starts_with("WEBVTT\n\n"), "{body}");
    assert!(body.contains("<v Tess>Hello world."), "{body}");

    // Both mode pairs original + translation.
    let both = http
        .get(format!(
            "{base}/api/sessions/{session_id}/transcript.srt?lang=both&target=it"
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(both.contains("Tess: Hello world.\nCiao mondo."), "{both}");

    // Gates: bad mode 400, stranger 403, no token 401.
    let bad = http
        .get(format!(
            "{base}/api/sessions/{session_id}/transcript.srt?lang=klingon"
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    let (_eve, eve_jwt) = login(&srv, "Eve").await;
    let forbidden = http
        .get(format!("{base}/api/sessions/{session_id}/transcript.vtt"))
        .bearer_auth(&eve_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(forbidden.status(), 403);
    let unauth = http
        .get(format!("{base}/api/sessions/{session_id}/transcript.srt"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), 401);
}

/// Bookmark CRUD (spec 0013): instant pin + later relabel, owner-only
/// mutations, shared visibility across participants, export integration, and
/// the FK cascade when the session goes away.
#[tokio::test]
async fn bookmarks_crud_gates_and_export() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let (tess, tess_jwt) = login(&srv, "Tess").await;
    let (bob, bob_jwt) = login(&srv, "Bob").await;
    let room = format!("bm-{}", Uuid::new_v4().simple());
    let session_id = Uuid::new_v4();

    // Seed a two-participant session directly through the service.
    let svc = TranscriptService::new(srv.pool.clone());
    svc.session_started(session_id, &room).await.unwrap();
    svc.participant_joined(session_id, "tess-peer", Some(tess), "Tess", "it")
        .await
        .unwrap();
    svc.participant_joined(session_id, "bob-peer", Some(bob), "Bob", "en")
        .await
        .unwrap();

    let http = reqwest::Client::new();
    let base = format!("http://{}", srv.addr);
    let bookmarks_url = format!("{base}/api/sessions/{session_id}/bookmarks");

    // Instant pin: empty body -> server stamps "now", no label yet.
    let created = http
        .post(&bookmarks_url)
        .bearer_auth(&tess_jwt)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201);
    let bm: serde_json::Value = created.json().await.unwrap();
    let bid = bm["id"].as_str().expect("bookmark id").to_string();
    assert_eq!(bm["by"], "Tess");
    assert_eq!(bm["mine"], true);
    assert!(bm["label"].is_null());
    assert!(bm["ts"].is_string());

    // Labels are capped at 200 chars.
    let too_long = http
        .post(&bookmarks_url)
        .bearer_auth(&tess_jwt)
        .json(&serde_json::json!({ "label": "x".repeat(201) }))
        .send()
        .await
        .unwrap();
    assert_eq!(too_long.status(), 400);

    // Relabel afterwards (the in-call input PATCHes) — whitespace trimmed.
    let tess_bm_url = format!("{bookmarks_url}/{bid}");
    let patched = http
        .patch(&tess_bm_url)
        .bearer_auth(&tess_jwt)
        .json(&serde_json::json!({ "label": "  decision made  " }))
        .send()
        .await
        .unwrap();
    assert_eq!(patched.status(), 204);

    // Bob pins with an explicit (earlier) ts + label of his own.
    let earlier = chrono::Utc::now() - chrono::Duration::seconds(60);
    let bob_created = http
        .post(&bookmarks_url)
        .bearer_auth(&bob_jwt)
        .json(&serde_json::json!({ "ts": earlier, "label": "Bob's moment" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bob_created.status(), 201);
    let bob_bm: serde_json::Value = bob_created.json().await.unwrap();
    let bob_bid = bob_bm["id"].as_str().unwrap().to_string();

    // Both participants see both pins, chronological, with viewer-relative `mine`.
    let rows: serde_json::Value = http
        .get(&bookmarks_url)
        .bearer_auth(&bob_jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["by"], "Bob", "explicit earlier ts sorts first");
    assert_eq!(rows[0]["mine"], true);
    assert_eq!(rows[1]["by"], "Tess");
    assert_eq!(rows[1]["mine"], false);
    assert_eq!(rows[1]["label"], "decision made");

    // Blank label PATCH clears it.
    let cleared = http
        .patch(format!("{bookmarks_url}/{bob_bid}"))
        .bearer_auth(&bob_jwt)
        .json(&serde_json::json!({ "label": "   " }))
        .send()
        .await
        .unwrap();
    assert_eq!(cleared.status(), 204);

    // Owner-only mutations: Bob can't touch Tess's pin; unknown id is 404.
    let hijack = http
        .patch(&tess_bm_url)
        .bearer_auth(&bob_jwt)
        .json(&serde_json::json!({ "label": "hijack" }))
        .send()
        .await
        .unwrap();
    assert_eq!(hijack.status(), 403);
    let steal = http
        .delete(&tess_bm_url)
        .bearer_auth(&bob_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(steal.status(), 403);
    let missing = http
        .delete(format!("{bookmarks_url}/{}", Uuid::new_v4()))
        .bearer_auth(&tess_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);

    // Gates: stranger 403, no token 401, unknown session 404.
    let (_eve, eve_jwt) = login(&srv, "Eve").await;
    let forbidden = http
        .get(&bookmarks_url)
        .bearer_auth(&eve_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(forbidden.status(), 403);
    let forbidden_post = http
        .post(&bookmarks_url)
        .bearer_auth(&eve_jwt)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(forbidden_post.status(), 403);
    let unauth = http.get(&bookmarks_url).send().await.unwrap();
    assert_eq!(unauth.status(), 401);
    let unknown = http
        .get(format!("{base}/api/sessions/{}/bookmarks", Uuid::new_v4()))
        .bearer_auth(&tess_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404);

    // The JSON transcript embeds the bookmarks chronologically (names only —
    // user ids never leave the server).
    let doc: serde_json::Value = http
        .get(format!("{base}/api/sessions/{session_id}/transcript.json"))
        .bearer_auth(&tess_jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let exported = doc["bookmarks"].as_array().expect("bookmarks array");
    assert_eq!(exported.len(), 2);
    assert_eq!(exported[0]["by"], "Bob");
    assert!(exported[0]["label"].is_null(), "cleared label exports null");
    assert_eq!(exported[1]["label"], "decision made");
    assert!(exported[0].get("id").is_none(), "export carries no ids");

    // Owner delete works...
    let deleted = http
        .delete(&tess_bm_url)
        .bearer_auth(&tess_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), 204);
    // ...and the FK cascade clears the rest when the session is purged.
    sqlx::query("DELETE FROM call_sessions WHERE id = $1")
        .bind(session_id)
        .execute(&srv.pool)
        .await
        .unwrap();
    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM transcript_bookmarks WHERE session_id = $1")
            .bind(session_id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(left, 0, "bookmarks cascade with the session");
}

/// AI session report (spec 0014): request validation, auth gates, the
/// empty-session 422, the 402 pre-check (balance untouched, no ledger row),
/// no-charge-on-Groq-failure (the test config's Groq key is a dummy, so the
/// real POST path dies at generation), and persistence/latest-wins via GET.
#[tokio::test]
async fn ai_report_validation_billing_and_persistence() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let (uid, jwt) = login(&srv, "Tess").await; // $2.00
    let room = format!("rp-{}", Uuid::new_v4().simple());
    let session_id = Uuid::new_v4();

    // Seed a session with one speech event (something to report on).
    let svc = TranscriptService::new(srv.pool.clone());
    svc.session_started(session_id, &room).await.unwrap();
    svc.participant_joined(session_id, "tess-peer", Some(uid), "Tess", "it")
        .await
        .unwrap();
    svc.record(TranscriptEvent {
        session_id,
        kind: EventKind::Speech,
        speaker_peer_id: "tess-peer".into(),
        speaker_user_id: Some(uid),
        speaker_name: "Tess".into(),
        original_text: "We agreed to ship on Friday.".into(),
        original_lang: "en".into(),
        translations: HashMap::from([("it".to_string(), "Venerdì si spedisce.".to_string())]),
        ts: chrono::Utc::now(),
    });
    svc.flush().await;

    let http = reqwest::Client::new();
    let base = format!("http://{}", srv.addr);
    let report_url = format!("{base}/api/sessions/{session_id}/report");

    // Gates: no token 401, stranger 403, unknown session 404, nothing yet 404.
    let unauth = http.get(&report_url).send().await.unwrap();
    assert_eq!(unauth.status(), 401);
    let (_eve, eve_jwt) = login(&srv, "Eve").await;
    let forbidden = http
        .get(&report_url)
        .bearer_auth(&eve_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(forbidden.status(), 403);
    let unknown = http
        .get(format!("{base}/api/sessions/{}/report", Uuid::new_v4()))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404);
    let none_yet = http
        .get(&report_url)
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(none_yet.status(), 200);
    assert!(
        none_yet
            .json::<serde_json::Value>()
            .await
            .unwrap()
            .is_null(),
        "no report yet → 200 null (not 404), to avoid post-call console spam"
    );

    // Validation 400s: bad format, oversized guidelines, garbage lang.
    for bad in [
        serde_json::json!({ "format": "haiku" }),
        serde_json::json!({ "guidelines": "x".repeat(2001) }),
        serde_json::json!({ "lang": "p?q" }),
    ] {
        let resp = http
            .post(&report_url)
            .bearer_auth(&jwt)
            .json(&bad)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "{bad}");
    }

    // An event-less session has nothing to report on -> 422.
    let empty_session = Uuid::new_v4();
    svc.session_started(empty_session, &format!("re-{room}"))
        .await
        .unwrap();
    svc.participant_joined(empty_session, "tess-peer", Some(uid), "Tess", "it")
        .await
        .unwrap();
    let empty = http
        .post(format!("{base}/api/sessions/{empty_session}/report"))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(empty.status(), 422);

    // 402 pre-check: drain the balance below the report cost (base 0.05).
    sqlx::query("UPDATE users SET balance = $2 WHERE id = $1")
        .bind(uid)
        .bind(usd(0.001))
        .execute(&srv.pool)
        .await
        .unwrap();
    let broke = http
        .post(&report_url)
        .bearer_auth(&jwt)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(broke.status(), 402);
    let body: serde_json::Value = broke.json().await.unwrap();
    assert_eq!(body["error"], "insufficient_credits");
    assert_eq!(body["feature"], "ai_report");
    assert!(body["required"].as_f64().unwrap() >= 0.05);
    assert!(body["available"].as_f64().unwrap() < 0.01);

    let balance_of = |uid: Uuid| {
        let pool = srv.pool.clone();
        async move {
            sqlx::query_scalar::<_, rust_decimal::Decimal>(
                "SELECT balance FROM users WHERE id = $1",
            )
            .bind(uid)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(balance_of(uid).await, usd(0.001), "402 never charges");

    // Groq-failure path: with funds restored, the POST now CLAIMS a background
    // job (202) and generation fails inside the task (dummy key). The failure
    // surfaces on the polled job, not the POST, and the balance stays untouched.
    sqlx::query("UPDATE users SET balance = $2 WHERE id = $1")
        .bind(uid)
        .bind(usd(2.0))
        .execute(&srv.pool)
        .await
        .unwrap();
    let accepted = http
        .post(&report_url)
        .bearer_auth(&jwt)
        .json(&serde_json::json!({
            "format": "structured",
            "lang": "it",
            "guidelines": "focus on decisions"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(accepted.status(), 202);
    let job_id = accepted.json::<serde_json::Value>().await.unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    let job = poll_ai_job(&http, &base, session_id, &job_id, &jwt).await;
    assert_eq!(job["status"], "failed", "Groq failure → failed job: {job}");
    assert_eq!(job["error"], "groq");
    assert_eq!(
        balance_of(uid).await,
        usd(2.0),
        "Groq failure never charges"
    );
    let charged: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM credit_transactions WHERE user_id = $1 AND kind = 'ai_report'",
    )
    .bind(uid)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(charged, 0, "no ai_report ledger rows on any failure path");

    // Persistence: store a report row directly (as a successful generation
    // would) and read it back through GET — cost must come out as a JSON
    // number, not rust_decimal's string serialization.
    voxtranslate_server::ai::report::save_report(
        &srv.pool,
        session_id,
        uid,
        "structured",
        "it",
        Some("focus on decisions"),
        "## Executive Summary\n\nShip on Friday.",
        "model-x",
        usd(0.052),
    )
    .await
    .unwrap();
    let got = http
        .get(&report_url)
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(got.status(), 200);
    let report: serde_json::Value = got.json().await.unwrap();
    assert_eq!(report["format"], "structured");
    assert_eq!(report["lang"], "it");
    assert_eq!(report["guidelines"], "focus on decisions");
    assert_eq!(
        report["markdown"],
        "## Executive Summary\n\nShip on Friday."
    );
    assert_eq!(report["model"], "model-x");
    assert!(report["cost"].is_number(), "cost is f64 in JSON: {report}");
    assert!((report["cost"].as_f64().unwrap() - 0.052).abs() < 1e-9);
    assert!(report["created_at"].is_string());

    // Regenerate keeps history; GET returns the newest row.
    voxtranslate_server::ai::report::save_report(
        &srv.pool,
        session_id,
        uid,
        "freeform",
        "en",
        None,
        "A second take.",
        "model-x",
        usd(0.052),
    )
    .await
    .unwrap();
    let latest: serde_json::Value = http
        .get(&report_url)
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(latest["markdown"], "A second take.");
    assert_eq!(latest["format"], "freeform");
    assert!(latest["guidelines"].is_null());
    let kept: i64 =
        sqlx::query_scalar("SELECT count(*) FROM session_reports WHERE session_id = $1")
            .bind(session_id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(kept, 2, "regenerate appends, never overwrites");
}

/// Sentiment analysis (spec 0015): auth gates, the 402 pre-check, the Groq
/// no-charge failure path, and the UNIQUE(session_id) cache contract — once a
/// result exists, every later POST (from any participant) returns it for free.
#[tokio::test]
async fn sentiment_cache_billing_and_gates() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let (tess, tess_jwt) = login(&srv, "Tess").await; // $2.00 each
    let (bob, bob_jwt) = login(&srv, "Bob").await;
    let room = format!("sn-{}", Uuid::new_v4().simple());
    let session_id = Uuid::new_v4();

    let svc = TranscriptService::new(srv.pool.clone());
    svc.session_started(session_id, &room).await.unwrap();
    svc.participant_joined(session_id, "tess-peer", Some(tess), "Tess", "it")
        .await
        .unwrap();
    svc.participant_joined(session_id, "bob-peer", Some(bob), "Bob", "en")
        .await
        .unwrap();
    svc.record(TranscriptEvent {
        session_id,
        kind: EventKind::Speech,
        speaker_peer_id: "tess-peer".into(),
        speaker_user_id: Some(tess),
        speaker_name: "Tess".into(),
        original_text: "I am thrilled with the launch numbers!".into(),
        original_lang: "en".into(),
        translations: HashMap::new(),
        ts: chrono::Utc::now(),
    });
    svc.flush().await;

    let http = reqwest::Client::new();
    let base = format!("http://{}", srv.addr);
    let url = format!("{base}/api/sessions/{session_id}/sentiment");

    // Gates: no token 401, stranger 403, unknown session 404, nothing yet 404.
    assert_eq!(http.get(&url).send().await.unwrap().status(), 401);
    let (_eve, eve_jwt) = login(&srv, "Eve").await;
    let forbidden = http.get(&url).bearer_auth(&eve_jwt).send().await.unwrap();
    assert_eq!(forbidden.status(), 403);
    let unknown = http
        .get(format!("{base}/api/sessions/{}/sentiment", Uuid::new_v4()))
        .bearer_auth(&tess_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404);
    let none_yet = http.get(&url).bearer_auth(&tess_jwt).send().await.unwrap();
    assert_eq!(none_yet.status(), 200);
    assert!(
        none_yet
            .json::<serde_json::Value>()
            .await
            .unwrap()
            .is_null(),
        "no analysis yet → 200 null"
    );

    // 402 pre-check: cost = base 0.05 + 2 × 0.01 + minutes × 0.002 > 0.001.
    sqlx::query("UPDATE users SET balance = $2 WHERE id = $1")
        .bind(tess)
        .bind(usd(0.001))
        .execute(&srv.pool)
        .await
        .unwrap();
    let broke = http.post(&url).bearer_auth(&tess_jwt).send().await.unwrap();
    assert_eq!(broke.status(), 402);
    let body: serde_json::Value = broke.json().await.unwrap();
    assert_eq!(body["error"], "insufficient_credits");
    assert_eq!(body["feature"], "ai_sentiment");
    assert!(body["required"].as_f64().unwrap() >= 0.07, "{body}");

    // Groq-failure path: funds restored, the POST claims a background job (202)
    // and analysis dies inside the task (dummy Groq key). The failure surfaces
    // on the polled job; balance untouched, no ledger row.
    sqlx::query("UPDATE users SET balance = $2 WHERE id = $1")
        .bind(tess)
        .bind(usd(2.0))
        .execute(&srv.pool)
        .await
        .unwrap();
    let accepted = http.post(&url).bearer_auth(&tess_jwt).send().await.unwrap();
    assert_eq!(accepted.status(), 202);
    let job_id = accepted.json::<serde_json::Value>().await.unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    let job = poll_ai_job(&http, &base, session_id, &job_id, &tess_jwt).await;
    assert_eq!(job["status"], "failed", "Groq failure → failed job: {job}");
    assert_eq!(job["error"], "groq");
    let balance: rust_decimal::Decimal =
        sqlx::query_scalar("SELECT balance FROM users WHERE id = $1")
            .bind(tess)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(balance, usd(2.0), "Groq failure never charges");

    // Cache contract: store a result (as a successful run would), then every
    // POST — including another participant's — returns it without charging.
    let result = serde_json::json!({
        "overall": { "score": 0.6, "mood": "positive" },
        "timeline": [ { "t": 0, "score": 0.6 } ],
        "speakers": [
            { "name": "Tess", "talk_pct": 100.0, "score": 0.6, "mood": "positive" },
            { "name": "Bob", "talk_pct": 0.0, "score": null, "mood": null }
        ],
        "key_moments": [],
        "window_secs": 120,
    });
    let saved = voxtranslate_server::ai::sentiment::save_sentiment(
        &srv.pool,
        session_id,
        tess,
        &result,
        "model-x",
        usd(0.072),
    )
    .await
    .unwrap();
    assert!(saved.is_some(), "first insert wins");
    let raced = voxtranslate_server::ai::sentiment::save_sentiment(
        &srv.pool,
        session_id,
        bob,
        &result,
        "model-x",
        usd(0.072),
    )
    .await
    .unwrap();
    assert!(raced.is_none(), "UNIQUE(session_id) rejects a second row");

    for jwt in [&tess_jwt, &bob_jwt] {
        let hit = http.post(&url).bearer_auth(jwt).send().await.unwrap();
        assert_eq!(hit.status(), 200, "cache hit is 200, not 201");
        let v: serde_json::Value = hit.json().await.unwrap();
        assert_eq!(v["cached"], true);
        assert_eq!(v["result"]["overall"]["mood"], "positive");
        assert_eq!(v["model"], "model-x");
        assert!(v["cost"].is_number(), "cost is f64 in JSON: {v}");
        assert!(v.get("balance").is_none(), "no charge -> no balance echo");
    }
    let charged: i64 =
        sqlx::query_scalar("SELECT count(*) FROM credit_transactions WHERE kind = 'ai_sentiment'")
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(
        charged, 0,
        "cache hits and failures never write ledger rows"
    );

    // GET mirrors the cached POST.
    let got: serde_json::Value = http
        .get(&url)
        .bearer_auth(&bob_jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["result"]["timeline"][0]["score"], 0.6);
    assert_eq!(got["cached"], true);
}

/// Follow-up email (spec 0016): the 503 feature gate, auth gates, recipient
/// validation 400s, the 402 pre-check, no-charge-on-Groq-failure, recipient
/// sanitization (user_ids and addresses never leak), owner-scoping, send gates
/// (404/403/409), and the failed-send-keeps-edited-draft contract.
#[tokio::test]
async fn email_draft_send_gates_and_billing() {
    // Without RESEND_* the feature is off: both POSTs 503.
    if let Some(plain) = setup().await {
        let (_uid, jwt) = login(&plain, "Tess").await;
        let http = reqwest::Client::new();
        let base = format!("http://{}", plain.addr);
        let sid = Uuid::new_v4();
        for (url, body) in [
            (
                format!("{base}/api/sessions/{sid}/email-draft"),
                serde_json::json!({ "recipients": [] }),
            ),
            (
                format!("{base}/api/sessions/{sid}/email-send"),
                serde_json::json!({ "email_id": Uuid::new_v4() }),
            ),
        ] {
            let resp = http
                .post(&url)
                .bearer_auth(&jwt)
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 503, "email disabled -> 503 at {url}");
        }
    }

    let Some(srv) = setup_opts(true).await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let (tess, tess_jwt) = login(&srv, "Tess").await; // $2.00 each
    let (bob, bob_jwt) = login(&srv, "Bob").await;
    let room = format!("em-{}", Uuid::new_v4().simple());
    let session_id = Uuid::new_v4();

    // Tess + Bob (accounts) + a guest share the session; one speech event.
    let svc = TranscriptService::new(srv.pool.clone());
    svc.session_started(session_id, &room).await.unwrap();
    svc.participant_joined(session_id, "tess-peer", Some(tess), "Tess", "it")
        .await
        .unwrap();
    svc.participant_joined(session_id, "bob-peer", Some(bob), "Bob", "en")
        .await
        .unwrap();
    svc.participant_joined(session_id, "guest-peer", None, "Gio", "en")
        .await
        .unwrap();
    svc.record(TranscriptEvent {
        session_id,
        kind: EventKind::Speech,
        speaker_peer_id: "tess-peer".into(),
        speaker_user_id: Some(tess),
        speaker_name: "Tess".into(),
        original_text: "Next steps agreed: ship Friday.".into(),
        original_lang: "en".into(),
        translations: HashMap::new(),
        ts: chrono::Utc::now(),
    });
    svc.flush().await;

    let http = reqwest::Client::new();
    let base = format!("http://{}", srv.addr);
    let get_url = format!("{base}/api/sessions/{session_id}/email");
    let draft_url = format!("{base}/api/sessions/{session_id}/email-draft");
    let send_url = format!("{base}/api/sessions/{session_id}/email-send");

    // GET gates: no token 401, stranger 403, unknown session 404, none yet 404.
    assert_eq!(http.get(&get_url).send().await.unwrap().status(), 401);
    let (_eve, eve_jwt) = login(&srv, "Eve").await;
    let forbidden = http
        .get(&get_url)
        .bearer_auth(&eve_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(forbidden.status(), 403);
    let unknown = http
        .get(format!("{base}/api/sessions/{}/email", Uuid::new_v4()))
        .bearer_auth(&tess_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404);
    let none_yet = http
        .get(&get_url)
        .bearer_auth(&tess_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(none_yet.status(), 200);
    assert!(
        none_yet
            .json::<serde_json::Value>()
            .await
            .unwrap()
            .is_null(),
        "no draft yet → 200 null"
    );

    // Draft validation 400s: empty recipients, invalid address, unknown peer,
    // guest peer (no account email), oversized guidelines, bad tone.
    for bad in [
        serde_json::json!({ "recipients": [] }),
        serde_json::json!({ "recipients": [ { "kind": "email", "email": "nope" } ] }),
        serde_json::json!({ "recipients": [ { "kind": "participant", "peer_id": "ghost" } ] }),
        serde_json::json!({ "recipients": [ { "kind": "participant", "peer_id": "guest-peer" } ] }),
        serde_json::json!({
            "recipients": [ { "kind": "participant", "peer_id": "bob-peer" } ],
            "guidelines": "x".repeat(2001),
        }),
        serde_json::json!({
            "recipients": [ { "kind": "participant", "peer_id": "bob-peer" } ],
            "tone": "sarcastic",
        }),
    ] {
        let resp = http
            .post(&draft_url)
            .bearer_auth(&tess_jwt)
            .json(&bad)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "{bad}");
    }

    // An event-less session has nothing to draft from -> 422.
    let empty_session = Uuid::new_v4();
    svc.session_started(empty_session, &format!("ee-{room}"))
        .await
        .unwrap();
    svc.participant_joined(empty_session, "tess-peer", Some(tess), "Tess", "it")
        .await
        .unwrap();
    let empty = http
        .post(format!("{base}/api/sessions/{empty_session}/email-draft"))
        .bearer_auth(&tess_jwt)
        .json(&serde_json::json!({
            "recipients": [ { "kind": "email", "email": "a@x.co" } ],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(empty.status(), 422);

    // 402 pre-check: flat draft cost 0.02 > 0.001.
    sqlx::query("UPDATE users SET balance = $2 WHERE id = $1")
        .bind(tess)
        .bind(usd(0.001))
        .execute(&srv.pool)
        .await
        .unwrap();
    let valid_body = serde_json::json!({
        "recipients": [ { "kind": "participant", "peer_id": "bob-peer" } ],
    });
    let broke = http
        .post(&draft_url)
        .bearer_auth(&tess_jwt)
        .json(&valid_body)
        .send()
        .await
        .unwrap();
    assert_eq!(broke.status(), 402);
    let body: serde_json::Value = broke.json().await.unwrap();
    assert_eq!(body["error"], "insufficient_credits");
    assert_eq!(body["feature"], "ai_email");
    assert!(
        (body["required"].as_f64().unwrap() - 0.02).abs() < 1e-9,
        "{body}"
    );

    // Groq-failure path: funds restored, the POST claims a background job (202)
    // and generation dies inside the task (dummy Groq key). The failure surfaces
    // on the polled job; balance untouched, no ledger row.
    sqlx::query("UPDATE users SET balance = $2 WHERE id = $1")
        .bind(tess)
        .bind(usd(2.0))
        .execute(&srv.pool)
        .await
        .unwrap();
    let accepted = http
        .post(&draft_url)
        .bearer_auth(&tess_jwt)
        .json(&valid_body)
        .send()
        .await
        .unwrap();
    assert_eq!(accepted.status(), 202);
    let job_id = accepted.json::<serde_json::Value>().await.unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    let job = poll_ai_job(&http, &base, session_id, &job_id, &tess_jwt).await;
    assert_eq!(job["status"], "failed", "Groq failure → failed job: {job}");
    assert_eq!(job["error"], "groq");
    let balance: rust_decimal::Decimal =
        sqlx::query_scalar("SELECT balance FROM users WHERE id = $1")
            .bind(tess)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(balance, usd(2.0), "Groq failure never charges");
    let charged: i64 =
        sqlx::query_scalar("SELECT count(*) FROM credit_transactions WHERE kind = 'ai_email'")
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(charged, 0, "no ai_email ledger rows on any failure path");

    // Persistence + sanitization: store a draft directly (as a successful
    // generation would) with a participant ref AND a raw typed address.
    let draft = voxtranslate_server::ai::email_draft::EmailDraft {
        subject: "Recap — ship Friday".into(),
        body_text: "Hi all,\n\nwe ship Friday.".into(),
        body_html: "<p>Hi all,</p>\n<p>we ship Friday.</p>".into(),
    };
    let recipients = serde_json::json!([
        { "kind": "participant", "user_id": bob, "name": "Bob", "cc": false },
        { "kind": "email", "email": "ext@x.com", "cc": true },
    ]);
    let row = voxtranslate_server::ai::email_draft::save_email(
        &srv.pool,
        session_id,
        tess,
        &draft,
        &recipients,
        Some("professional"),
        None,
        "en",
    )
    .await
    .unwrap();

    let got = http
        .get(&get_url)
        .bearer_auth(&tess_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(got.status(), 200);
    let raw = got.text().await.unwrap();
    assert!(
        !raw.contains("user_id"),
        "user ids never leave the server: {raw}"
    );
    assert!(
        !raw.contains("body_html"),
        "html part is server-side only: {raw}"
    );
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(v["status"], "draft");
    assert_eq!(v["subject"], "Recap — ship Friday");
    assert_eq!(v["recipients"][0]["kind"], "participant");
    assert_eq!(v["recipients"][0]["name"], "Bob");
    assert_eq!(
        v["recipients"][1]["email"], "ext@x.com",
        "own typed address echoes"
    );
    assert_eq!(v["tone"], "professional");
    assert!(v["resend_id"].is_null());

    // Owner-scoping: Bob is a participant but the draft isn't his -> GET 404.
    let bobs = http
        .get(&get_url)
        .bearer_auth(&bob_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(bobs.status(), 200);
    assert!(
        bobs.json::<serde_json::Value>().await.unwrap().is_null(),
        "drafts are owner-scoped — Bob sees null (not Tess's draft), now 200 not 404"
    );

    // Send gates: unknown draft 404, stranger 403 (session gate), participant
    // non-owner 403 (ownership).
    let email_id = v["id"].as_str().unwrap();
    let missing = http
        .post(&send_url)
        .bearer_auth(&tess_jwt)
        .json(&serde_json::json!({ "email_id": Uuid::new_v4() }))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
    let eve_send = http
        .post(&send_url)
        .bearer_auth(&eve_jwt)
        .json(&serde_json::json!({ "email_id": email_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(eve_send.status(), 403);
    let bob_send = http
        .post(&send_url)
        .bearer_auth(&bob_jwt)
        .json(&serde_json::json!({ "email_id": email_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        bob_send.status(),
        403,
        "participant but not the draft owner"
    );

    // Failed send (dummy Resend key -> 401 upstream): 502, the draft survives
    // WITH the pre-send edits persisted (subject + re-rendered html).
    let send_edited = http
        .post(&send_url)
        .bearer_auth(&tess_jwt)
        .json(&serde_json::json!({
            "email_id": email_id,
            "subject": "Edited",
            "body_text": "a & b <c>\n\npara two",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(send_edited.status(), 502);
    assert!(send_edited.text().await.unwrap().contains("draft was kept"));
    let (status, subject, body_html): (String, String, String) =
        sqlx::query_as("SELECT status, subject, body_html FROM session_emails WHERE id = $1")
            .bind(row.id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(status, "draft", "failed send never flips the status");
    assert_eq!(subject, "Edited");
    assert!(
        body_html.contains("&amp;") && body_html.contains("&lt;"),
        "{body_html}"
    );

    // Already sent -> 409.
    sqlx::query("UPDATE session_emails SET status = 'sent' WHERE id = $1")
        .bind(row.id)
        .execute(&srv.pool)
        .await
        .unwrap();
    let again = http
        .post(&send_url)
        .bearer_auth(&tess_jwt)
        .json(&serde_json::json!({ "email_id": email_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 409);
}

#[tokio::test]
async fn guest_only_session_is_purged_on_end() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let room = format!("gr-{}", Uuid::new_v4().simple());

    // A guest in a private room still gets a session id (recording is on)...
    let (frame, mut ws) = connect_first(srv.addr, &format!("room={room}&lang=en&id=g1")).await;
    assert_eq!(frame["type"], "room_joined");
    let session_id = Uuid::parse_str(frame["session_id"].as_str().unwrap()).unwrap();

    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::json!({ "type": "chat", "text": "off the record" }).to_string(),
    ))
    .await
    .unwrap();
    wait_for(&mut ws, "chat_message").await;
    drop(ws); // last leave -> finalize -> guest-only purge

    // ...but the whole session (and its events) is purged on end.
    let mut sessions = -1i64;
    for _ in 0..30 {
        sessions = sqlx::query_scalar("SELECT count(*) FROM call_sessions WHERE id = $1")
            .bind(session_id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
        if sessions == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(sessions, 0, "guest-only session purged");
    let events: i64 =
        sqlx::query_scalar("SELECT count(*) FROM transcript_events WHERE session_id = $1")
            .bind(session_id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
    assert_eq!(events, 0, "no orphaned guest events");
}

#[tokio::test]
async fn set_lang_resolves_auto_and_updates_participant_row() {
    // Auto-detect correction path (spec 0012), no Deepgram needed: a peer joins
    // with lang=auto, corrects it via `set_lang`, and everyone gets the
    // `language_detected` broadcast while the participant row is updated.
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let (_uid, jwt) = login(&srv, "Aura").await;
    let room = format!("al-{}", Uuid::new_v4().simple());

    let (frame, mut auto_ws) = connect_first(
        srv.addr,
        &format!("room={room}&lang=auto&id=auto-peer&token={jwt}"),
    )
    .await;
    assert_eq!(frame["type"], "room_joined");
    let session_id = Uuid::parse_str(frame["session_id"].as_str().unwrap()).unwrap();

    // A second (guest, private-room) peer sees the pending state...
    let (frame_b, mut other_ws) =
        connect_first(srv.addr, &format!("room={room}&lang=it&id=other-peer")).await;
    assert_eq!(frame_b["peers"][0]["lang"], "auto");

    // ...and while detection is pending, "auto" is never a fan-out target: the
    // other peer's chat carries only its own-language echo (no Groq call), and
    // crucially no "auto" entry.
    other_ws
        .send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!({ "type": "chat", "text": "ciao" }).to_string(),
        ))
        .await
        .unwrap();
    let chat = wait_for(&mut other_ws, "chat_message").await;
    assert_eq!(chat["original"], "ciao");
    let translations = chat["translations"].as_object().unwrap();
    assert_eq!(translations.len(), 1, "source-lang echo only");
    assert!(translations.contains_key("it") && !translations.contains_key("auto"));

    // Garbage codes are rejected (trim/lowercase happens first).
    for bad in ["auto", "", "x".repeat(9).as_str(), "p?q"] {
        auto_ws
            .send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::json!({ "type": "set_lang", "lang": bad }).to_string(),
            ))
            .await
            .unwrap();
        let err = wait_for(&mut auto_ws, "error").await;
        assert_eq!(err["code"], "bad_lang");
    }

    // Manual correction: both peers get the broadcast, confidence omitted.
    auto_ws
        .send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!({ "type": "set_lang", "lang": " ES " }).to_string(),
        ))
        .await
        .unwrap();
    let det = wait_for(&mut auto_ws, "language_detected").await;
    assert_eq!(det["peer_id"], "auto-peer");
    assert_eq!(det["lang"], "es");
    assert!(det.get("confidence").is_none(), "manual => no confidence");
    let det_other = wait_for(&mut other_ws, "language_detected").await;
    assert_eq!(det_other["lang"], "es");

    // The participant row now reflects what's actually spoken.
    let lang: String = sqlx::query_scalar(
        "SELECT lang FROM session_participants WHERE session_id = $1 AND peer_id = 'auto-peer'",
    )
    .bind(session_id)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(lang, "es");
}

#[tokio::test]
async fn quiz_history_persists_and_lists_with_gates() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let (tess, tess_jwt) = login(&srv, "Tess").await;
    let (_bob, bob_jwt) = login(&srv, "Bob").await; // Bob is NOT a participant
    let room = format!("quiz-{}", Uuid::new_v4().simple());
    let session_id = Uuid::new_v4();

    let svc = TranscriptService::new(srv.pool.clone());
    svc.session_started(session_id, &room).await.unwrap();
    svc.participant_joined(session_id, "tess-peer", Some(tess), "Tess", "it")
        .await
        .unwrap();

    let http = reqwest::Client::new();
    let base = format!("http://{}", srv.addr);
    let url = format!("{base}/api/sessions/{session_id}/quizzes");

    // Empty before any quiz.
    let empty: serde_json::Value = http
        .get(&url)
        .bearer_auth(&tess_jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(empty.as_array().unwrap().len(), 0);

    // Host persists a finished quiz (one authed participant + one guest score).
    let payload = serde_json::json!({
        "title": "General knowledge",
        "questions": [{"prompt": "2+2?", "options": ["3", "4", "5"], "correct_index": 1}],
        "results": [
            {"peer_id": "tess-peer", "display_name": "Tess", "score": 1, "total": 1},
            {"peer_id": "guest-1", "display_name": "Gigi", "score": 0, "total": 1}
        ]
    });
    let saved = http
        .post(&url)
        .bearer_auth(&tess_jwt)
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(saved.status(), 201);

    // Lists with results, best score first.
    let list: serde_json::Value = http
        .get(&url)
        .bearer_auth(&tess_jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let arr = list.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["title"], "General knowledge");
    let results = arr[0]["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["display_name"], "Tess"); // higher score first
    assert_eq!(results[0]["score"], 1);

    // A non-participant is forbidden.
    let forbidden = http.get(&url).bearer_auth(&bob_jwt).send().await.unwrap();
    assert_eq!(forbidden.status(), 403);

    sqlx::query("DELETE FROM call_sessions WHERE id = $1")
        .bind(session_id)
        .execute(&srv.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn analytics_rollup_aggregates_session_events() {
    let Some(srv) = setup().await else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let (uid, _jwt) = login(&srv, "Ann").await;
    let session_id = Uuid::new_v4();
    let svc = TranscriptService::new(srv.pool.clone());
    svc.session_started(session_id, "rollup-room")
        .await
        .unwrap();

    // A premium session_ended event of 180s should roll up to 3 minutes.
    let mut ev = voxtranslate_server::analytics::UsageEvent::new("premium", "session_ended")
        .session(session_id)
        .user(Some(uid));
    ev.duration_seconds = 180;
    voxtranslate_server::analytics::insert_event(&srv.pool, &ev)
        .await
        .unwrap();
    voxtranslate_server::analytics::roll_up(&srv.pool)
        .await
        .unwrap();

    let (total, prem): (i32, i32) = sqlx::query_as(
        "SELECT total_minutes, premium_minutes FROM user_usage_stats WHERE user_id = $1",
    )
    .bind(uid)
    .fetch_one(&srv.pool)
    .await
    .unwrap();
    assert_eq!(total, 3);
    assert_eq!(prem, 3);

    sqlx::query("DELETE FROM call_sessions WHERE id = $1")
        .bind(session_id)
        .execute(&srv.pool)
        .await
        .unwrap();
}
