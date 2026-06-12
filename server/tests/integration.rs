//! Integration tests: spin the real Axum app on a random port and drive it over
//! HTTP + WebSocket. Lifecycle / signaling / max-4 / mute tests need no external
//! APIs; the chat + audio tests need DEEPGRAM_API_KEY + GROQ_API_KEY (loaded from
//! server/.env) and are skipped if absent.

use std::net::SocketAddr;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use voxtranslate_server::config::Config;
use voxtranslate_server::{app, AppState};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Build state from env (real keys) or a dummy fallback; returns `has_keys`.
fn make_state() -> (AppState, bool) {
    let _ = dotenvy::dotenv();
    match Config::from_env() {
        Ok(c) => (AppState::new(c), true),
        Err(_) => (
            AppState::new(Config {
                deepgram_key: "dummy".into(),
                groq_key: "dummy".into(),
                port: 0,
                allowed_origins: vec![],
                auto_detect_buffer_ms: 3000,
                billing: None,
                resend: None,
                storage: None,
            }),
            false,
        ),
    }
}

/// Start the app on a random local port, return (addr, has_keys).
async fn spawn() -> (SocketAddr, bool) {
    let (state, has_keys) = make_state();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    (addr, has_keys)
}

async fn connect(addr: SocketAddr, params: &str) -> Ws {
    let url = format!("ws://{addr}/ws?{params}");
    let (ws, _resp) = connect_async(url).await.expect("ws connect");
    ws
}

/// Read the next JSON text frame within `ms`.
async fn next_json(ws: &mut Ws, ms: u64) -> Option<Value> {
    loop {
        match tokio::time::timeout(Duration::from_millis(ms), ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                return serde_json::from_str(t.as_str()).ok();
            }
            Ok(Some(Ok(_))) => continue, // ping/pong/binary
            _ => return None,
        }
    }
}

/// Read JSON frames until one with `type == ty` arrives (or timeout).
async fn wait_for(ws: &mut Ws, ty: &str, ms: u64) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return None;
        }
        match next_json(ws, left.as_millis() as u64).await {
            Some(v) if v["type"] == ty => return Some(v),
            Some(_) => continue,
            None => return None,
        }
    }
}

async fn send_text(ws: &mut Ws, s: &str) {
    ws.send(Message::text(s.to_string())).await.unwrap();
}

#[tokio::test]
async fn health_and_rooms_and_bad_params() {
    let (addr, _) = spawn().await;
    let http = reqwest::Client::new();

    let health = http
        .get(format!("http://{addr}/health"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(health, "ok");

    let rooms: Value = http
        .get(format!("http://{addr}/rooms"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rooms["rooms"].as_array().unwrap().len(), 0);

    // Missing lang -> 400.
    let bad = http
        .get(format!("http://{addr}/ws?room=r"))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
}

#[tokio::test]
async fn lifecycle_signaling_mute_and_lobby() {
    let (addr, _) = spawn().await;

    let mut a = connect(addr, "room=r1&lang=it&id=a&name=Alice&public=true").await;
    let aj = next_json(&mut a, 1000).await.unwrap();
    assert_eq!(aj["type"], "room_joined");
    assert_eq!(aj["peer_id"], "a");
    assert_eq!(aj["peers"].as_array().unwrap().len(), 0);

    let mut b = connect(addr, "room=r1&lang=en&id=b&name=Bob&public=true").await;
    let bj = next_json(&mut b, 1000).await.unwrap();
    assert_eq!(bj["type"], "room_joined");
    assert_eq!(bj["peers"][0]["id"], "a");

    // A is told B joined.
    let pj = wait_for(&mut a, "peer_joined", 1000).await.unwrap();
    assert_eq!(pj["peer_id"], "b");

    // Lobby now lists the public room with 2 members.
    let rooms: Value = reqwest::get(format!("http://{addr}/rooms"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rooms["rooms"][0]["room"], "r1");
    assert_eq!(rooms["rooms"][0]["count"], 2);

    // Signaling relay: B -> offer -> A (server adds `from`).
    send_text(&mut b, r#"{"type":"offer","to":"a","sdp":"SDP_B"}"#).await;
    let off = wait_for(&mut a, "offer", 1000).await.unwrap();
    assert_eq!(off["from"], "b");
    assert_eq!(off["sdp"], "SDP_B");
    send_text(&mut a, r#"{"type":"answer","to":"b","sdp":"SDP_A"}"#).await;
    assert_eq!(wait_for(&mut b, "answer", 1000).await.unwrap()["from"], "a");
    send_text(&mut a, r#"{"type":"ice","to":"b","candidate":{"x":1}}"#).await;
    assert_eq!(wait_for(&mut b, "ice", 1000).await.unwrap()["from"], "a");

    // Mute relay.
    send_text(&mut a, r#"{"type":"mute_audio","muted":true}"#).await;
    let pm = wait_for(&mut b, "peer_muted", 1000).await.unwrap();
    assert_eq!(pm["peer_id"], "a");
    assert_eq!(pm["kind"], "audio");
    assert_eq!(pm["muted"], true);
    send_text(&mut a, r#"{"type":"mute_video","muted":true}"#).await;
    assert_eq!(
        wait_for(&mut b, "peer_muted", 1000).await.unwrap()["kind"],
        "video"
    );

    // Unknown control frame is ignored (no crash, connection stays up).
    send_text(&mut a, r#"{"type":"nonsense"}"#).await;

    // A leaves -> B is told.
    a.close(None).await.unwrap();
    drop(a);
    let pl = wait_for(&mut b, "peer_left", 2000).await.unwrap();
    assert_eq!(pl["peer_id"], "a");
}

#[tokio::test]
async fn emoji_reaction_and_hand_raise_broadcast() {
    let (addr, _) = spawn().await;

    // Private rooms (no `public`) need no login, so guests can connect.
    let mut a = connect(addr, "room=rx&lang=it&id=a&name=Alice").await;
    assert_eq!(
        next_json(&mut a, 1000).await.unwrap()["type"],
        "room_joined"
    );
    let mut b = connect(addr, "room=rx&lang=en&id=b&name=Bob").await;
    assert_eq!(
        next_json(&mut b, 1000).await.unwrap()["type"],
        "room_joined"
    );
    wait_for(&mut a, "peer_joined", 1000).await.unwrap();

    // Emoji reactions broadcast to everyone, including the sender.
    send_text(&mut a, r#"{"type":"emoji","emoji":"👍"}"#).await;
    let er = wait_for(&mut b, "emoji_reaction", 1000).await.unwrap();
    assert_eq!(er["peer_id"], "a");
    assert_eq!(er["peer_name"], "Alice");
    assert_eq!(er["emoji"], "👍");
    // The sender receives its own reaction too (broadcast, not broadcast_except).
    assert_eq!(
        wait_for(&mut a, "emoji_reaction", 1000).await.unwrap()["emoji"],
        "👍"
    );

    // Hand-raise is relayed to the other peers only (broadcast_except).
    send_text(&mut a, r#"{"type":"hand_raise","raised":true}"#).await;
    let hr = wait_for(&mut b, "hand_raised", 1000).await.unwrap();
    assert_eq!(hr["peer_id"], "a");
    assert_eq!(hr["raised"], true);
}

#[tokio::test]
async fn room_full_rejects_fifth() {
    let (addr, _) = spawn().await;
    let mut held = Vec::new();
    for i in 0..4 {
        let mut w = connect(addr, &format!("room=full&lang=en&id=p{i}")).await;
        let j = next_json(&mut w, 1000).await.unwrap();
        assert_eq!(j["type"], "room_joined");
        held.push(w);
    }
    let mut fifth = connect(addr, "room=full&lang=en&id=p5").await;
    let j = next_json(&mut fifth, 1000).await.unwrap();
    assert_eq!(j["type"], "room_full");
}

#[tokio::test]
async fn chat_is_translated_and_broadcast() {
    let (addr, has_keys) = spawn().await;
    if !has_keys {
        eprintln!("skipping chat test — no API keys");
        return;
    }
    let mut a = connect(addr, "room=chat&lang=it&id=a&name=Alice").await;
    let _ = next_json(&mut a, 1000).await;
    let mut b = connect(addr, "room=chat&lang=en&id=b&name=Bob").await;
    let _ = next_json(&mut b, 1000).await;
    let _ = wait_for(&mut a, "peer_joined", 1000).await;

    send_text(&mut a, r#"{"type":"chat","text":"ciao a tutti"}"#).await;
    let msg = wait_for(&mut b, "chat_message", 8000)
        .await
        .expect("chat_message");
    assert_eq!(msg["sender_id"], "a");
    assert_eq!(msg["original"], "ciao a tutti");
    assert_eq!(msg["translations"]["it"], "ciao a tutti");
    assert!(
        msg["translations"]["en"].is_string(),
        "english translation present"
    );
}

#[tokio::test]
async fn audio_produces_subtitles() {
    let (addr, has_keys) = spawn().await;
    if !has_keys {
        eprintln!("skipping audio test — no API keys");
        return;
    }
    let audio = std::fs::read("tests/fixtures/sample.webm").expect("fixture");

    // Listener (en) in the room receives the translated subtitle.
    let mut listener = connect(addr, "room=aud&lang=en&id=l&name=Listener").await;
    let _ = next_json(&mut listener, 1000).await;

    // Speaker (it) streams the webm.
    let mut speaker = connect(addr, "room=aud&lang=it&id=s&name=Speaker").await;
    let _ = next_json(&mut speaker, 1000).await;

    send_text(&mut speaker, r#"{"type":"start"}"#).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    for chunk in audio.chunks(1024) {
        speaker.send(Message::binary(chunk.to_vec())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
    tokio::time::sleep(Duration::from_millis(2000)).await;
    send_text(&mut speaker, r#"{"type":"stop"}"#).await;

    let sub = wait_for(&mut listener, "subtitle_final", 15000)
        .await
        .expect("subtitle_final");
    assert_eq!(sub["speaker_id"], "s");
    assert_eq!(sub["lang"], "it");
    assert!(!sub["original"].as_str().unwrap().is_empty());
    assert!(sub["translations"]["it"].is_string());
    assert!(sub["translations"]["en"].is_string());
}

#[tokio::test]
async fn deepgram_unavailable_sends_error() {
    // A bad Deepgram key makes the speaking session fail to open -> the speaker
    // receives an Error (covers the open-failure branch).
    let _ = dotenvy::dotenv();
    let groq = std::env::var("GROQ_API_KEY").unwrap_or_else(|_| "dummy".into());
    let state = AppState::new(Config {
        deepgram_key: "bad-deepgram-key".into(),
        groq_key: groq,
        port: 0,
        allowed_origins: vec![],
        auto_detect_buffer_ms: 3000,
        billing: None,
        resend: None,
        storage: None,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });

    let mut s = connect(addr, "room=x&lang=it&id=s").await;
    let _ = next_json(&mut s, 1000).await; // room_joined
    send_text(&mut s, r#"{"type":"start"}"#).await;
    let err = wait_for(&mut s, "error", 8000).await.expect("error frame");
    assert_eq!(err["message"], "speech service unavailable");
}

// ---- Chat file upload (spec 0018) ------------------------------------------

/// Build a minimal `multipart/form-data` body with a `peer_id` text field and a
/// `file` part. Returns `(content_type_header, body_bytes)`.
fn multipart_body(peer_id: &str, filename: &str, file_bytes: &[u8]) -> (String, Vec<u8>) {
    let boundary = "voxtestboundary123";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"peer_id\"\r\n\r\n{peer_id}\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: text/plain\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(file_bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// Start the app on a random port from a prebuilt state; returns its address.
async fn spawn_state(state: AppState) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    addr
}

fn guest_config() -> Config {
    Config {
        deepgram_key: "dummy".into(),
        groq_key: "dummy".into(),
        port: 0,
        allowed_origins: vec![],
        auto_detect_buffer_ms: 3000,
        billing: None,
        resend: None,
        storage: None,
    }
}

#[tokio::test]
async fn upload_returns_503_when_storage_unconfigured() {
    // No SUPABASE_* -> storage is None -> the endpoint self-disables.
    let addr = spawn_state(AppState::new(guest_config())).await;
    let (ctype, body) = multipart_body("p1", "notes.txt", b"hello");
    let res = reqwest::Client::new()
        .post(format!("http://{addr}/api/rooms/x/files"))
        .header(reqwest::header::CONTENT_TYPE, ctype)
        .body(body)
        .send()
        .await
        .expect("request");
    assert_eq!(res.status().as_u16(), 503);
}

/// Spin a stand-in Supabase Storage server: any request → 200. Returns its addr.
async fn spawn_mock_storage() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let app = axum::Router::new().fallback(|| async { "ok" });
        let _ = axum::serve(listener, app).await;
    });
    addr
}

fn storage_cfg(
    mock_addr: SocketAddr,
    max_bytes: usize,
) -> voxtranslate_server::config::StorageConfig {
    voxtranslate_server::config::StorageConfig {
        supabase_url: format!("http://{mock_addr}"),
        service_key: "test-key".into(),
        bucket: "chat-files".into(),
        max_bytes,
    }
}

#[tokio::test]
async fn upload_text_file_broadcasts_chat_message() {
    // Full happy path, hermetic: a stand-in storage server accepts the bytes, and
    // a SINGLE peer in the room means the translation fan-out has no targets — so
    // no Groq/Deepgram call happens. The peer should receive a `chat_message`
    // carrying the file attachment + the extracted text (R1/R3 for text).
    let mock = spawn_mock_storage().await;
    let mut cfg = guest_config();
    cfg.storage = Some(storage_cfg(mock, 25 * 1024 * 1024));
    let addr = spawn_state(AppState::new(cfg)).await;

    let mut ws = connect(addr, "room=fileroom&lang=it&id=u1&name=Uno").await;
    assert_eq!(
        next_json(&mut ws, 1000).await.unwrap()["type"],
        "room_joined"
    );

    let (ctype, body) = multipart_body("u1", "notes.txt", b"ciao mondo");
    let res = reqwest::Client::new()
        .post(format!("http://{addr}/api/rooms/fileroom/files"))
        .header(reqwest::header::CONTENT_TYPE, ctype)
        .body(body)
        .send()
        .await
        .expect("request");
    assert_eq!(res.status().as_u16(), 200);
    let json: Value = res.json().await.unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["name"], "notes.txt");
    assert_eq!(json["type"], "text/plain");

    let msg = wait_for(&mut ws, "chat_message", 4000)
        .await
        .expect("chat_message broadcast");
    assert_eq!(msg["sender_id"], "u1");
    assert_eq!(msg["original"], "ciao mondo");
    assert_eq!(msg["attachment"]["name"], "notes.txt");
    assert_eq!(msg["attachment"]["content_type"], "text/plain");
    assert_eq!(msg["attachment"]["size"], 10);
    assert!(msg["attachment"]["url"]
        .as_str()
        .unwrap()
        .contains("/storage/v1/object/public/chat-files/"));
}

#[tokio::test]
async fn upload_rejects_unsupported_type_and_oversize() {
    // A member peer (so we pass the 403 gate) uploads a bad type then an oversize
    // file; both are rejected before any storage call.
    let mock = spawn_mock_storage().await;
    let mut cfg = guest_config();
    cfg.storage = Some(storage_cfg(mock, 4)); // 4-byte cap to trigger 413 cheaply
    let addr = spawn_state(AppState::new(cfg)).await;

    let mut ws = connect(addr, "room=valroom&lang=it&id=u9&name=Niner").await;
    assert_eq!(
        next_json(&mut ws, 1000).await.unwrap()["type"],
        "room_joined"
    );
    let client = reqwest::Client::new();

    // Unsupported extension -> 415.
    let (ctype, body) = multipart_body("u9", "virus.exe", b"MZ");
    let res = client
        .post(format!("http://{addr}/api/rooms/valroom/files"))
        .header(reqwest::header::CONTENT_TYPE, ctype)
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 415);

    // Supported type but over the (tiny) cap -> 413.
    let (ctype, body) = multipart_body("u9", "notes.txt", b"way too long");
    let res = client
        .post(format!("http://{addr}/api/rooms/valroom/files"))
        .header(reqwest::header::CONTENT_TYPE, ctype)
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 413);
}

#[tokio::test]
async fn upload_persists_when_db_configured() {
    // With the DB configured, the upload also inserts a `chat_files` row and a
    // transcript event (the DB-write branches). Skipped without DATABASE_URL.
    let Ok(db_url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping — no DATABASE_URL");
        return;
    };
    let mock = spawn_mock_storage().await;
    let mut cfg = Config::test_with_billing(&db_url, "test-jwt-secret", 5.0);
    cfg.storage = Some(storage_cfg(mock, 25 * 1024 * 1024));
    let state = AppState::init(cfg)
        .await
        .expect("init billing+storage state");
    let addr = spawn_state(state).await;

    // Guests join even under billing (no token → no balance gate), and a call
    // session row is created on join so the chat_files FK is satisfied.
    let mut ws = connect(addr, "room=dbfileroom&lang=it&id=g1&name=Guest").await;
    let joined = next_json(&mut ws, 1500).await.unwrap();
    assert_eq!(joined["type"], "room_joined");

    let (ctype, body) = multipart_body("g1", "memo.txt", b"persist me");
    let res = reqwest::Client::new()
        .post(format!("http://{addr}/api/rooms/dbfileroom/files"))
        .header(reqwest::header::CONTENT_TYPE, ctype)
        .body(body)
        .send()
        .await
        .expect("request");
    assert_eq!(res.status().as_u16(), 200);

    let msg = wait_for(&mut ws, "chat_message", 4000)
        .await
        .expect("chat_message");
    assert_eq!(msg["original"], "persist me");
    assert_eq!(msg["attachment"]["name"], "memo.txt");
}

#[tokio::test]
async fn upload_returns_403_when_peer_not_in_room() {
    // Storage configured (dummy) so the request passes the 503 gate; the peer is
    // not a member of the room, so the membership gate rejects it *before* any
    // network call to Supabase (the dummy URL is never contacted).
    use voxtranslate_server::config::StorageConfig;
    let mut cfg = guest_config();
    cfg.storage = Some(StorageConfig {
        supabase_url: "http://127.0.0.1:9".into(), // never contacted
        service_key: "dummy".into(),
        bucket: "chat-files".into(),
        max_bytes: 25 * 1024 * 1024,
    });
    let addr = spawn_state(AppState::new(cfg)).await;
    let (ctype, body) = multipart_body("ghost", "notes.txt", b"hello");
    let res = reqwest::Client::new()
        .post(format!("http://{addr}/api/rooms/emptyroom/files"))
        .header(reqwest::header::CONTENT_TYPE, ctype)
        .body(body)
        .send()
        .await
        .expect("request");
    assert_eq!(res.status().as_u16(), 403);
}
