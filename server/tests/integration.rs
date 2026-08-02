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
                push: None,
                deepgram_key: "dummy".into(),
                groq_key: "dummy".into(),
                translation_model: "openai/gpt-oss-20b".into(),
                port: 0,
                allowed_origins: vec![],
                extension_origins: vec![],
                auto_detect_buffer_ms: 3000,
                billing: None,
                resend: None,
                storage: None,
                turn: None,
                turn_restricted: None,
                bug_report_to: "test@example.com".into(),
                app_base_url: "https://voxtranslate.app".into(),
                dashboard_base_url: "https://dashboard.voxtranslate.app".into(),
                business_member_limit: 20,
                retention_sweep_enabled: false,
                retention_sweep_interval_secs: 21_600,
                retention_sweep_batch: 200,
                openai: None,
                google: None,
                cartesia: None,
                qwen: Default::default(),
                standard_enabled: true,
                listener_pays: false,
                language_first_ux: false,
                cache_enabled: false,
                cache_max_words: 8,
                cache_ttl_secs: 604_800,
                dragonfly_url: None,
                bench_secret: None,
                embeddings: None,
                embeddings_backfill_secret: None,
                voice_assistant: None,
                help_assistant: None,
                webinar: None,
            }),
            false,
        ),
    }
}

/// Build a minimal no-external-services state (resend=None, billing=None, storage=None).
/// Use for tests that must exercise "feature dormant / provider absent" paths regardless
/// of what the local .env contains.
fn make_minimal_state() -> AppState {
    AppState::new(Config {
        push: None,
        deepgram_key: "dummy".into(),
        groq_key: "dummy".into(),
        translation_model: "openai/gpt-oss-20b".into(),
        port: 0,
        allowed_origins: vec![],
        extension_origins: vec![],
        auto_detect_buffer_ms: 3000,
        billing: None,
        resend: None,
        storage: None,
        turn: None,
        turn_restricted: None,
        bug_report_to: "test@example.com".into(),
        app_base_url: "https://voxtranslate.app".into(),
        dashboard_base_url: "https://dashboard.voxtranslate.app".into(),
        business_member_limit: 20,
        retention_sweep_enabled: false,
        retention_sweep_interval_secs: 21_600,
        retention_sweep_batch: 200,
        openai: None,
        google: None,
        cartesia: None,
        qwen: Default::default(),
        standard_enabled: true,
        listener_pays: false,
        language_first_ux: false,
        cache_enabled: false,
        cache_max_words: 8,
        cache_ttl_secs: 604_800,
        dragonfly_url: None,
        bench_secret: None,
        embeddings: None,
        embeddings_backfill_secret: None,
        voice_assistant: None,
        help_assistant: None,
        webinar: None,
    })
}

async fn spawn_minimal() -> SocketAddr {
    let state = make_minimal_state();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    addr
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

    // /metrics (spec 0058): Prometheus exposition with this instance's live gauges.
    let metrics = http
        .get(format!("http://{addr}/metrics"))
        .send()
        .await
        .unwrap();
    assert_eq!(metrics.status(), 200);
    let ctype = metrics
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(ctype.starts_with("text/plain"), "content-type was {ctype}");
    let body = metrics.text().await.unwrap();
    assert!(body.contains("# TYPE voxtranslate_http_request_duration_ms histogram"));
    assert!(body.contains("voxtranslate_http_requests_total{status_class=\"2xx\"}"));
    // No room joined in this test → both gauges read zero on this instance.
    assert!(body.contains("voxtranslate_active_rooms 0"));
    assert!(body.contains("voxtranslate_active_peers 0"));

    // Missing lang -> 400.
    let bad = http
        .get(format!("http://{addr}/ws?room=r"))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
}

#[tokio::test]
async fn contact_form_validates_and_requires_email_provider() {
    // Public Business contact form (POST /api/contact). Validation runs before the
    // email-provider check, so bad input is 400 regardless of Resend; a well-formed
    // message with no Resend configured is 503.
    // Uses spawn_minimal() so the test is deterministic even when the local .env has
    // a real RESEND_API_KEY (make_state() would pick it up and return 200).
    let addr = spawn_minimal().await;
    let http = reqwest::Client::new();
    let url = format!("http://{addr}/api/contact");

    // Missing name/message -> 400.
    let bad = http
        .post(&url)
        .json(&serde_json::json!({ "name": "", "email": "a@b.com", "message": "" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);

    // Invalid email -> 400.
    let bad_email = http
        .post(&url)
        .json(&serde_json::json!({ "name": "Acme", "email": "nope", "message": "hi there" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_email.status(), 400);

    // Valid payload, but no Resend in the test config -> 503 (feature dormant).
    let ok_shape = http
        .post(&url)
        .json(&serde_json::json!({
            "name": "Acme Inc",
            "email": "buyer@acme.com",
            "company": "Acme",
            "message": "We'd like 30 seats on Enterprise."
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(ok_shape.status(), 503);
}

#[tokio::test]
async fn engines_endpoint_lists_standard_without_leaking_cost() {
    // The engine registry is surfaced for the pre-join selector (spec 0093). The
    // default `standard` engine is always present; raw cost/markup never leave the
    // server — only the computed `rate_per_minute`.
    let (addr, _) = spawn().await;
    let body = reqwest::Client::new()
        .get(format!("http://{addr}/api/engines"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&body).unwrap();
    // Shape: { engines: [...], flags: { language_first_ux } } (spec 0102). The flag is
    // exposed here (guest-safe) to gate the language-first picker.
    let arr = payload["engines"]
        .as_array()
        .expect("engines is a JSON array under `engines`");
    assert!(
        payload["flags"]["language_first_ux"].is_boolean(),
        "language_first_ux flag must be exposed"
    );
    let standard = arr
        .iter()
        .find(|e| e["id"] == "standard")
        .expect("standard engine is always available");
    assert_eq!(standard["tier"], "standard");
    assert!(standard["rate_per_minute"].is_number());
    assert!(standard["output_languages"]
        .as_array()
        .unwrap()
        .contains(&Value::from("en")));
    // Standard became speech-to-speech when it moved to Qwen realtime: the server
    // streams translated audio instead of leaving the browser to synthesize it. This
    // capability is also what makes the client capture PCM16 and the picker show the
    // per-language price note, so it is a contract, not a label.
    assert_eq!(
        standard["capabilities"]["translated_audio"],
        Value::from(true)
    );
    assert_eq!(
        standard["capabilities"]["cost_scales_per_language"],
        Value::from(true)
    );
    // The billing-internal raw cost and markup must never be serialized.
    assert!(!body.contains("cost_per_minute"), "raw cost leaked: {body}");
    assert!(!body.contains("markup"), "markup leaked: {body}");
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
    // PCM16 mono @ 24 kHz — the capture format every speech-to-speech tier takes, and
    // what the client now produces for Standard too. The old WebM/Opus fixture fed the
    // engine bytes it read as raw samples: no transcript, no subtitle, no error.
    let audio = std::fs::read("tests/fixtures/sample_24k.pcm").expect("fixture");

    // Listener (en) in the room receives the translated subtitle.
    let mut listener = connect(addr, "room=aud&lang=en&id=l&name=Listener").await;
    let _ = next_json(&mut listener, 1000).await;

    // Speaker (it) streams the PCM.
    let mut speaker = connect(addr, "room=aud&lang=it&id=s&name=Speaker").await;
    let _ = next_json(&mut speaker, 1000).await;

    send_text(&mut speaker, r#"{"type":"start"}"#).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    // 100 ms per chunk, paced in real time: turn detection keys off silence, so blasting
    // the whole clip at once would look like one impossibly fast utterance.
    const BYTES_PER_CHUNK: usize = 24_000 * 2 * 100 / 1000;
    for chunk in audio.chunks(BYTES_PER_CHUNK) {
        speaker.send(Message::binary(chunk.to_vec())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    send_text(&mut speaker, r#"{"type":"stop"}"#).await;

    let sub = wait_for(&mut listener, "subtitle_final", 20000)
        .await
        .expect("subtitle_final");
    assert_eq!(sub["speaker_id"], "s");
    // `lang` is the SOURCE language; the translation is keyed by the listener's.
    assert_eq!(sub["lang"], "it");
    assert!(!sub["original"].as_str().unwrap().is_empty());
    // Per-language targeting: this listener's frame carries THEIR language only, so the
    // text on screen can never disagree with the audio they hear. The old flat fan-out
    // shipped every room language in one frame.
    assert!(
        sub["translations"]["en"].is_string(),
        "listener's own language must be present: {sub}"
    );
}

#[tokio::test]
async fn lone_speaker_opens_no_upstream_session() {
    // Standard used to open a Deepgram connection the moment a speaker pressed start,
    // so a bad key produced an immediate "speech service unavailable" — even for someone
    // ALONE in a room, who had nobody to be translated for. Qwen is per-target-language
    // and lazy: with no other language present it opens nothing, costs nothing, and must
    // NOT surface an error. That is what this now guards.
    //
    // The genuine unavailable path (a real target language plus an exhausted reconnect
    // budget) is deliberately NOT exercised here: MAX_OPEN_FAILURES with capped backoff
    // takes ~23 s to give up, which would make this suite slow and flaky. The credential
    // shape that actually causes it is covered at boot in tests/config_env.rs.
    let _ = dotenvy::dotenv();
    let groq = std::env::var("GROQ_API_KEY").unwrap_or_else(|_| "dummy".into());
    let state = AppState::new(Config {
        push: None,
        deepgram_key: String::new(), // optional now: batch transcription only
        groq_key: groq,
        translation_model: "openai/gpt-oss-20b".into(),
        port: 0,
        allowed_origins: vec![],
        extension_origins: vec![],
        auto_detect_buffer_ms: 3000,
        billing: None,
        resend: None,
        storage: None,
        turn: None,
        turn_restricted: None,
        bug_report_to: "test@example.com".into(),
        app_base_url: "https://voxtranslate.app".into(),
        dashboard_base_url: "https://dashboard.voxtranslate.app".into(),
        business_member_limit: 20,
        retention_sweep_enabled: false,
        retention_sweep_interval_secs: 21_600,
        retention_sweep_batch: 200,
        openai: None,
        google: None,
        cartesia: None,
        qwen: Default::default(),
        standard_enabled: true,
        listener_pays: false,
        language_first_ux: false,
        cache_enabled: false,
        cache_max_words: 8,
        cache_ttl_secs: 604_800,
        dragonfly_url: None,
        bench_secret: None,
        embeddings: None,
        embeddings_backfill_secret: None,
        voice_assistant: None,
        help_assistant: None,
        webinar: None,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });

    let mut s = connect(addr, "room=x&lang=it&id=s").await;
    let _ = next_json(&mut s, 1000).await; // room_joined
    send_text(&mut s, r#"{"type":"start"}"#).await;
    // No target language → no upstream session → nothing can fail. A short window is
    // enough: the old behaviour errored on the very first connect attempt.
    assert!(
        wait_for(&mut s, "error", 2000).await.is_none(),
        "a speaker alone in a room must not open — or fail — an upstream session"
    );
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
        push: None,
        deepgram_key: "dummy".into(),
        groq_key: "dummy".into(),
        translation_model: "openai/gpt-oss-20b".into(),
        port: 0,
        allowed_origins: vec![],
        extension_origins: vec![],
        auto_detect_buffer_ms: 3000,
        billing: None,
        resend: None,
        storage: None,
        turn: None,
        turn_restricted: None,
        bug_report_to: "test@example.com".into(),
        app_base_url: "https://voxtranslate.app".into(),
        dashboard_base_url: "https://dashboard.voxtranslate.app".into(),
        business_member_limit: 20,
        retention_sweep_enabled: false,
        retention_sweep_interval_secs: 21_600,
        retention_sweep_batch: 200,
        openai: None,
        google: None,
        cartesia: None,
        qwen: Default::default(),
        standard_enabled: true,
        listener_pays: false,
        language_first_ux: false,
        cache_enabled: false,
        cache_max_words: 8,
        cache_ttl_secs: 604_800,
        dragonfly_url: None,
        bench_secret: None,
        embeddings: None,
        embeddings_backfill_secret: None,
        voice_assistant: None,
        help_assistant: None,
        webinar: None,
    }
}

#[tokio::test]
async fn bench_translate_404_without_secret() {
    // BENCH_SECRET unset (guest_config) → the internal endpoint is invisible: the
    // `BenchAuth` extractor 404s before any body parse (spec 0107 R9).
    let addr = spawn_state(AppState::new(guest_config())).await;
    let res = reqwest::Client::new()
        .post(format!("http://{addr}/internal/bench/translate"))
        .json(&serde_json::json!({ "text": "ciao", "src": "it", "tgt": "en" }))
        .send()
        .await
        .expect("request");
    assert_eq!(res.status().as_u16(), 404);
}

#[tokio::test]
async fn bench_translate_401_with_wrong_token() {
    // With a secret configured, a missing/wrong bearer token → 401, again before
    // the body is deserialized or Groq is ever touched (spec 0107 R9).
    let mut cfg = guest_config();
    cfg.bench_secret = Some("right-secret".into());
    let addr = spawn_state(AppState::new(cfg)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{addr}/internal/bench/translate"))
        .header(reqwest::header::AUTHORIZATION, "Bearer wrong-secret")
        .json(&serde_json::json!({ "text": "ciao", "src": "it", "tgt": "en" }))
        .send()
        .await
        .expect("request");
    assert_eq!(res.status().as_u16(), 401);
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

/// Spin a stand-in Supabase Storage server. The sign endpoint returns a
/// realistic `{ signedURL }`; everything else (object upload) → 200.
async fn spawn_mock_storage() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let app = axum::Router::new()
            .route(
                "/storage/v1/object/sign/{*rest}",
                axum::routing::post(|| async {
                    axum::Json(serde_json::json!({
                        "signedURL": "/object/sign/chat-files/mock.txt?token=mocktoken"
                    }))
                }),
            )
            .fallback(|| async { "ok" });
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
        signed_ttl_secs: 3600,
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
    // Private bucket → the link is a time-limited signed URL, not a public one.
    let url = msg["attachment"]["url"].as_str().unwrap();
    assert!(
        url.contains("/storage/v1/object/sign/chat-files/"),
        "url={url}"
    );
    assert!(url.contains("token="), "url={url}");
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
        signed_ttl_secs: 3600,
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

// ---- Abuse hardening (spec 0064) -------------------------------------------

#[tokio::test]
async fn metrics_token_gates_access() {
    // With METRICS_TOKEN set, /metrics requires a matching bearer token.
    let (mut state, _) = make_state();
    state.metrics_token = Some("s3cret".to_string());
    let addr = spawn_state(state).await;
    let http = reqwest::Client::new();

    let unauth = http
        .get(format!("http://{addr}/metrics"))
        .send()
        .await
        .expect("request");
    assert_eq!(unauth.status().as_u16(), 401);

    let authed = http
        .get(format!("http://{addr}/metrics"))
        .header(reqwest::header::AUTHORIZATION, "Bearer s3cret")
        .send()
        .await
        .expect("request");
    assert_eq!(authed.status().as_u16(), 200);
    assert!(authed
        .text()
        .await
        .unwrap()
        .contains("voxtranslate_http_requests_total"));
}

#[tokio::test]
async fn rooms_endpoint_is_rate_limited() {
    // Per-IP throttle (spec 0064): a 60/min budget, then 429. All test requests share
    // the same (header-less) client-IP key, so the limiter trips deterministically.
    let addr = spawn().await.0;
    let http = reqwest::Client::new();
    let mut got_429 = false;
    for _ in 0..70 {
        let code = http
            .get(format!("http://{addr}/rooms"))
            .send()
            .await
            .expect("request")
            .status()
            .as_u16();
        if code == 429 {
            got_429 = true;
            break;
        }
        assert_eq!(code, 200);
    }
    assert!(
        got_429,
        "expected a 429 once the per-IP /rooms budget was exhausted"
    );
}

// --- VoxTranslate for Chrome (server/src/extension.rs) ----------------------
//
// Deterministic, provider-free coverage of the rejection paths. The happy path needs
// Deepgram and a billed account, so it lives in docs/manual-testing.md in the extension
// repo — but these guards are exactly the ones that must never silently regress.

/// Attempt a `/ws/extension` upgrade and return the HTTP status the server answered with.
async fn ext_ws_status(addr: SocketAddr, query: &str) -> u16 {
    let url = format!("ws://{addr}/ws/extension?{query}");
    match connect_async(url).await {
        // An accepted upgrade is 101.
        Ok(_) => 101,
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => resp.status().as_u16(),
        Err(e) => panic!("unexpected ws error: {e}"),
    }
}

#[tokio::test]
async fn extension_ws_rejects_an_auto_target_language() {
    let addr = spawn_minimal().await;
    // `auto` is only meaningful as a SOURCE. An `auto` listener is skipped by the room
    // fan-out, so the session would connect and then silently produce nothing.
    assert_eq!(ext_ws_status(addr, "lang=auto").await, 400);
}

#[tokio::test]
async fn extension_ws_rejects_a_malformed_language() {
    let addr = spawn_minimal().await;
    // The language is interpolated into the Deepgram streaming URL, so a loose rule here
    // would reopen query smuggling (`?lang=en&redact=pci`).
    assert_eq!(ext_ws_status(addr, "lang=en%26redact%3Dpci").await, 400);
    assert_eq!(ext_ws_status(addr, "lang=toolonglang").await, 400);
    assert_eq!(ext_ws_status(addr, "lang=").await, 400);
}

#[tokio::test]
async fn extension_ws_rejects_a_missing_language() {
    let addr = spawn_minimal().await;
    // Axum's Query extractor rejects the missing required field before the handler runs.
    assert!(ext_ws_status(addr, "source=auto").await >= 400);
}

#[tokio::test]
async fn extension_token_exchange_rejects_a_bogus_code() {
    let addr = spawn_minimal().await;
    let client = reqwest::Client::new();
    let res = client
        .post(format!("http://{addr}/api/extension/token"))
        .json(&serde_json::json!({
            "code": "not-a-jwt",
            "code_verifier": "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
        }))
        .send()
        .await
        .expect("request");
    // Billing is absent in the minimal state, so this is 503; with billing it would be
    // 401. Either way it must never mint a token for an unverifiable code.
    assert!(
        res.status().as_u16() >= 400,
        "a bogus code must never be redeemed, got {}",
        res.status()
    );
}

#[tokio::test]
async fn extension_token_exchange_rejects_an_out_of_range_verifier() {
    let addr = spawn_minimal().await;
    let client = reqwest::Client::new();
    // RFC 7636 §4.1 bounds the verifier at 43..=128 chars.
    let res = client
        .post(format!("http://{addr}/api/extension/token"))
        .json(&serde_json::json!({ "code": "x", "code_verifier": "tooshort" }))
        .send()
        .await
        .expect("request");
    assert!(res.status().as_u16() >= 400);
}

#[tokio::test]
async fn extension_code_requires_authentication() {
    let addr = spawn_minimal().await;
    let client = reqwest::Client::new();
    let res = client
        .post(format!("http://{addr}/api/extension/code"))
        .json(&serde_json::json!({ "code_challenge": "x".repeat(43) }))
        .send()
        .await
        .expect("request");
    // No Authorization header: this endpoint speaks for a signed-in user and must never
    // issue a code without one.
    assert!(
        res.status().as_u16() >= 400,
        "unauthenticated code issue must fail, got {}",
        res.status()
    );
}
