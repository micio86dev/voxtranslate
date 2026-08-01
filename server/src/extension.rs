//! VoxTranslate for Chrome — the browser-extension surface.
//!
//! Two concerns live here, both deliberately additive so a fault in either cannot
//! affect calls, webinars, or the existing room path:
//!
//! 1. **Login handoff** (`/api/extension/code`, `/api/extension/token`) — an
//!    authorization-code exchange with PKCE, layered on the Google login that already
//!    exists. VoxTranslate is an OAuth *client*, not an authorization server, so rather
//!    than building one we mint a short-lived, PKCE-bound code and swap it for the same
//!    session JWT every other client uses.
//!
//! 2. **Extension sessions** (`/ws/extension`) — one tab's audio in, translated
//!    subtitles out.
//!
//! ## Why the session registers TWO peers
//!
//! VoxTranslate's model is a room of symmetric peers: each has ONE `lang`, which is both
//! what they speak and what they receive. Translation targets are derived from the OTHER
//! peers in the room (`rooms::get_room_languages` excludes self), and `usage::
//! billable_streams` returns `None` when no target differs from the speaker — which
//! means a room with a single peer transcribes nothing, translates nothing, and bills
//! nothing.
//!
//! An extension session is asymmetric: one foreign audio SOURCE and one LISTENER. So the
//! connection joins a private room as two peers:
//!
//! ```text
//!   source peer   id = "<sid>-src"   lang = "auto"    owns the Deepgram session
//!   listener peer id = "<sid>"       lang = <target>  receives subtitles, is billed
//! ```
//!
//! Every existing behaviour then falls out for free, with no change to `rooms.rs`,
//! `usage.rs`, or any engine:
//!
//! * fan-out sees one target language and translates into it;
//! * the meter bills one stream per interval;
//! * when detection resolves the source language to the listener's, delivery is skipped
//!   AND the meter skips the tick — the "already in your language" bypass, unbilled,
//!   because that is already how the room model behaves.
//!
//! The room is always `Private`, so it is never listed in the lobby and never joinable.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::deepgram::SpeakerCtx;
use crate::engine::{SessionDeps, SessionOutcome};
use crate::middleware::AuthUser;
use crate::protocol::ServerMessage;
use crate::rooms::{Peer, PeerTx, Visibility, OUT_CHANNEL_CAP};
use crate::usage::{run_usage_meter, MeterConfig, MeterScope};
use crate::AppState;

/// How long a handoff code stays valid. Short on purpose: the code is redeemed by the
/// extension within a second of the redirect, so anything longer only widens the window.
const CODE_TTL_SECONDS: i64 = 60;

/// Marker in the code's claims. Without it a *session* JWT would also verify here and
/// could be redeemed as a code (and vice versa) — the classic token-confusion bug.
const CODE_KIND: &str = "vox_ext_code";

// ---------------------------------------------------------------------------
// Login handoff
// ---------------------------------------------------------------------------

/// Claims of a one-time handoff code. Self-contained and signed rather than stored:
/// that avoids a table and a migration on a live billing database, at the cost of not
/// being able to revoke a code inside its 60-second life. The trade is acceptable
/// because the code is worthless without the PKCE verifier, which never leaves the
/// extension's memory.
#[derive(Debug, Serialize, Deserialize)]
struct CodeClaims {
    /// User id.
    sub: String,
    /// Always [`CODE_KIND`].
    kind: String,
    /// The S256 PKCE challenge this code is bound to.
    challenge: String,
    exp: usize,
}

#[derive(Deserialize)]
pub struct CodeRequest {
    /// base64url(SHA-256(verifier)), per RFC 7636.
    pub code_challenge: String,
    /// Only `S256` is accepted; `plain` defeats the purpose of PKCE.
    #[serde(default)]
    pub code_challenge_method: Option<String>,
}

#[derive(Serialize)]
pub struct CodeResponse {
    pub code: String,
    pub expires_in: i64,
}

/// `POST /api/extension/code` — called by voxtranslate.app on behalf of a signed-in
/// user, with the challenge the extension generated. Returns a code that only the holder
/// of the matching verifier can redeem.
pub async fn issue_code(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<CodeRequest>,
) -> Response {
    let Some(billing) = state.config.billing.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "auth not configured").into_response();
    };

    if body.code_challenge_method.as_deref().unwrap_or("S256") != "S256" {
        return (StatusCode::BAD_REQUEST, "only S256 is supported").into_response();
    }
    // base64url of a SHA-256 digest is exactly 43 chars. Bounding it keeps a hostile
    // caller from stuffing the signed token with arbitrary data.
    let challenge = body.code_challenge.trim();
    if challenge.len() != 43
        || !challenge
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return (StatusCode::BAD_REQUEST, "invalid code_challenge").into_response();
    }

    let exp = (chrono::Utc::now() + chrono::Duration::seconds(CODE_TTL_SECONDS)).timestamp();
    let claims = CodeClaims {
        sub: user.user_id.to_string(),
        kind: CODE_KIND.to_string(),
        challenge: challenge.to_string(),
        exp: exp.max(0) as usize,
    };
    match encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(billing.jwt_secret.as_bytes()),
    ) {
        Ok(code) => Json(CodeResponse {
            code,
            expires_in: CODE_TTL_SECONDS,
        })
        .into_response(),
        Err(e) => {
            tracing::error!("extension code issue failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "could not issue code").into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct TokenRequest {
    pub code: String,
    pub code_verifier: String,
}

/// base64url(SHA-256(verifier)) — the value the challenge must equal.
fn s256(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// `POST /api/extension/token` — redeem a code plus its verifier for a session token.
/// Unauthenticated by definition: this is what produces the credential.
pub async fn exchange_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<TokenRequest>,
) -> Response {
    let (Some(billing), Some(pool)) = (state.config.billing.as_ref(), state.pool.as_ref()) else {
        return (StatusCode::SERVICE_UNAVAILABLE, "auth not configured").into_response();
    };

    // Throttle per client: a code is a bearer credential for 60 s, so brute-forcing the
    // verifier must be expensive even though it is 256 bits of entropy.
    let client_key = crate::observability::client_ip(&headers);
    if !state.rate_limiter.allow(
        &format!("extcode:{client_key}"),
        30,
        Duration::from_secs(60),
    ) {
        return (StatusCode::TOO_MANY_REQUESTS, "too many requests").into_response();
    }

    // RFC 7636 §4.1 length bounds; anything outside them cannot be a legitimate verifier.
    if !(43..=128).contains(&body.code_verifier.len()) {
        return (StatusCode::BAD_REQUEST, "invalid code_verifier").into_response();
    }

    let claims = match decode::<CodeClaims>(
        &body.code,
        &DecodingKey::from_secret(billing.jwt_secret.as_bytes()),
        &Validation::new(jsonwebtoken::Algorithm::HS256),
    ) {
        Ok(data) => data.claims,
        Err(_) => return (StatusCode::UNAUTHORIZED, "invalid or expired code").into_response(),
    };
    // Reject a session token presented as a code (and anything else signed by us).
    if claims.kind != CODE_KIND {
        return (StatusCode::UNAUTHORIZED, "invalid code").into_response();
    }
    if s256(&body.code_verifier) != claims.challenge {
        return (StatusCode::UNAUTHORIZED, "verifier does not match").into_response();
    }

    let Ok(user_id) = Uuid::parse_str(&claims.sub) else {
        return (StatusCode::UNAUTHORIZED, "invalid code").into_response();
    };

    let user = match sqlx::query_as::<_, crate::db::User>("SELECT * FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(pool)
        .await
    {
        Ok(Some(u)) => u,
        Ok(None) => return (StatusCode::UNAUTHORIZED, "account not found").into_response(),
        Err(e) => {
            tracing::error!("extension token load user failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "login failed").into_response();
        }
    };

    // A banned account must not be able to open an extension session either.
    if let Some(safety) = state.safety.as_ref() {
        if let Ok(Some(_reason)) = safety.is_banned(user_id).await {
            return (StatusCode::FORBIDDEN, "account suspended").into_response();
        }
    }

    match crate::auth::issue_jwt(
        &billing.jwt_secret,
        &user.id,
        &user.email,
        &user.name,
        billing.jwt_expiry_hours,
    ) {
        Ok(token) => Json(serde_json::json!({
            "token": token,
            "user": crate::auth::UserProfile::from(user),
        }))
        .into_response(),
        Err(e) => {
            tracing::error!("extension token issue failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "login failed").into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Extension translation session
// ---------------------------------------------------------------------------

/// Query parameters for `GET /ws/extension`.
#[derive(Debug, Clone, Deserialize)]
pub struct ExtParams {
    /// The language the user wants to understand.
    pub lang: String,
    /// The spoken language, or `auto` (the default and the normal case).
    #[serde(default)]
    pub source: Option<String>,
    /// Session JWT. Absent or invalid → rejected: this is a billed feature with no
    /// guest tier, unlike a call.
    #[serde(default)]
    pub token: Option<String>,
    /// Chosen engine id; unknown ids fall back to the default.
    #[serde(default)]
    pub engine: Option<String>,
}

/// Accept `a`–`z`, digits and `-`, 1..=8 chars — the same rule the room `/ws` route
/// applies, because this value is interpolated into the Deepgram streaming URL.
fn valid_lang(code: &str) -> bool {
    (1..=8).contains(&code.len()) && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// `GET /ws/extension` — upgrade a Chrome-extension translation session.
pub async fn ws_extension(
    ws: WebSocketUpgrade,
    Query(params): Query<ExtParams>,
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Response {
    let target = params.lang.trim().to_lowercase();
    let source = params
        .source
        .as_deref()
        .unwrap_or("auto")
        .trim()
        .to_lowercase();

    if !valid_lang(&target) || !valid_lang(&source) {
        return (StatusCode::BAD_REQUEST, "invalid language code").into_response();
    }
    // `auto` is only meaningful as a SOURCE. A target of `auto` would make the fan-out
    // skip this listener entirely and the session would silently produce nothing.
    if target == "auto" {
        return (StatusCode::BAD_REQUEST, "target language cannot be auto").into_response();
    }

    let ip = crate::observability::client_ip(&headers);
    if !state.rate_limiter.allow(
        &format!("extws:{ip}"),
        state.ws_connect_max,
        Duration::from_secs(60),
    ) {
        return (StatusCode::TOO_MANY_REQUESTS, "too many connections").into_response();
    }

    let params = ExtParams {
        lang: target,
        source: Some(source),
        ..params
    };
    ws.on_upgrade(move |socket| handle_extension_session(socket, params, state))
}

/// Everything one extension session owns, so teardown is a single place.
struct SessionGuard {
    rooms: Arc<crate::rooms::RoomManager>,
    room: String,
    listener_id: String,
    source_id: String,
    conn: Uuid,
    billing: Option<crate::billing::BillingService>,
    usage_session: Option<Uuid>,
}

impl SessionGuard {
    /// Release the room slots, stop any live meter, and close the usage session. Called
    /// on every exit path — a leaked peer would keep billing a user who has gone.
    async fn finish(self, meter_cancel: Option<oneshot::Sender<()>>) {
        if let Some(cancel) = meter_cancel {
            let _ = cancel.send(());
        }
        self.rooms.remove(&self.room, &self.listener_id, self.conn);
        self.rooms.remove(&self.room, &self.source_id, self.conn);
        if let (Some(svc), Some(sid)) = (self.billing.as_ref(), self.usage_session) {
            if let Err(e) = svc.finalize_session(sid).await {
                tracing::error!("extension finalize session failed: {e}");
            }
        }
    }
}

async fn handle_extension_session(socket: WebSocket, params: ExtParams, state: AppState) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // --- authenticate ------------------------------------------------------
    let authed = match crate::authorize(&state, params.token.as_deref()).await {
        Ok(Some(peer)) => peer,
        Ok(None) => {
            let _ = ws_tx
                .send(Message::Text(
                    ServerMessage::Error {
                        message: "sign in to use VoxTranslate for Chrome".to_string(),
                        code: Some("invalid_token".to_string()),
                    }
                    .to_json()
                    .into(),
                ))
                .await;
            return;
        }
        Err(msg) => {
            let _ = ws_tx.send(Message::Text(msg.to_json().into())).await;
            return;
        }
    };

    let engine = state.engines.resolve(params.engine.as_deref());
    // A client-direct engine (Cartesia "Enhanced") never runs on the server path, so it
    // would produce silence here. Fall back to the default rather than pretend.
    let engine = if engine.metadata().capabilities.client_direct {
        state.engines.default()
    } else {
        engine
    };
    let engine_id = engine.metadata().id.clone();
    let rate_per_second = engine.metadata().user_rate_per_second();

    // --- build the two-peer private room -----------------------------------
    let session_uuid = Uuid::new_v4();
    let room = format!("ext-{session_uuid}");
    let listener_id = session_uuid.to_string();
    let source_id = format!("{session_uuid}-src");
    let conn = Uuid::new_v4();
    let target_lang = params.lang.clone();
    let source_lang = params.source.clone().unwrap_or_else(|| "auto".to_string());

    let (out_tx, out_rx, _out_overflow) = PeerTx::channel(OUT_CHANNEL_CAP);
    // The source peer never receives anything meaningful; its channel exists only to
    // satisfy the Peer contract. Draining it keeps the sender from filling and blocking
    // a broadcast to the whole room.
    let (src_tx, mut src_rx, _src_overflow) = PeerTx::channel(OUT_CHANNEL_CAP);
    tokio::spawn(async move { while src_rx.recv().await.is_some() {} });

    let listener = Peer {
        id: listener_id.clone(),
        conn,
        name: "You".to_string(),
        lang: target_lang.clone(),
        user_id: Some(authed.user_id),
        engine: engine_id.clone(),
        avatar_url: authed.avatar_url.clone(),
        cartesia_voice_id: None,
        tx: out_tx.clone(),
        speaking: Arc::new(AtomicBool::new(false)),
    };
    let source = Peer {
        id: source_id.clone(),
        conn,
        name: "Tab audio".to_string(),
        lang: source_lang.clone(),
        user_id: Some(authed.user_id),
        engine: engine_id.clone(),
        avatar_url: None,
        cartesia_voice_id: None,
        tx: src_tx,
        speaking: Arc::new(AtomicBool::new(true)),
    };

    let joined = match state.rooms.join(&room, listener, Visibility::Private) {
        Ok(j) => {
            // The second join cannot realistically fail — the room is brand new and holds
            // one peer against a cap of four — but if it ever did, the listener would sit
            // in the room forever with nothing driving or ending it. Remove it explicitly
            // rather than rely on that reasoning staying true.
            if state
                .rooms
                .join(&room, source, Visibility::Private)
                .is_err()
            {
                state.rooms.remove(&room, &listener_id, conn);
                let _ = ws_tx
                    .send(Message::Text(ServerMessage::RoomFull.to_json().into()))
                    .await;
                return;
            }
            j
        }
        Err(()) => {
            let _ = ws_tx
                .send(Message::Text(ServerMessage::RoomFull.to_json().into()))
                .await;
            return;
        }
    };

    // --- billing -----------------------------------------------------------
    let usage_session = match state.billing.as_ref() {
        Some(svc) => match svc.create_session(authed.user_id, &room, &engine_id).await {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::error!("extension usage session create failed: {e}");
                None
            }
        },
        None => None,
    };

    let (exhaust_tx, mut exhaust_rx) = mpsc::unbounded_channel::<()>();

    // The meter is started PER STREAMING SESSION, not per connection — mirroring the
    // room path (`lib::spawn_meter` is called only once a session actually opens). A
    // connection-long meter would charge a user who opened the panel and never pressed
    // Start, and would keep charging after Stop until they closed the tab.
    let start_meter = || -> Option<oneshot::Sender<()>> {
        let (svc, sid, cfg) = match (
            state.billing.as_ref(),
            usage_session,
            state.config.billing.as_ref(),
        ) {
            (Some(svc), Some(sid), Some(cfg)) => (svc, sid, cfg),
            _ => return None,
        };
        let (cancel_tx, cancel_rx) = oneshot::channel();
        // Speaker scope on the SOURCE peer: the billable stream count comes from the
        // other languages in the room, which is exactly the listener's target — and
        // becomes zero (skipped tick, no charge) the moment detection resolves the
        // source language to the listener's own.
        let meter_cfg = MeterConfig {
            interval_secs: cfg.pricing.usage_update_interval,
            rate_per_second,
            low_balance_threshold: cfg.pricing.low_balance_threshold,
            rooms: Some(state.rooms.clone()),
            room: room.clone(),
            scope: MeterScope::Speaker {
                speaker_id: source_id.clone(),
                speaker_lang: source_lang.clone(),
                scale_by_target_count: engine.metadata().capabilities.cost_scales_per_language,
            },
        };
        tokio::spawn(run_usage_meter(
            svc.clone(),
            authed.user_id,
            sid,
            meter_cfg,
            out_tx.clone(),
            exhaust_tx.clone(),
            cancel_rx,
        ));
        Some(cancel_tx)
    };

    let guard = SessionGuard {
        rooms: state.rooms.clone(),
        room: room.clone(),
        listener_id: listener_id.clone(),
        source_id: source_id.clone(),
        conn,
        billing: state.billing.clone(),
        usage_session,
    };

    // Tell the client it is live. Reuses `room_joined` so the extension shares the
    // protocol types with every other client.
    let _ = out_tx.send(
        ServerMessage::RoomJoined {
            peer_id: listener_id.clone(),
            peers: Vec::new(),
            session_id: Some(joined.session_id.to_string()),
            public: false,
        }
        .to_json(),
    );

    let send_task = tokio::spawn(crate::pump_to_ws(out_rx, ws_tx));

    // --- the session loop --------------------------------------------------
    let mut audio_tx: Option<mpsc::Sender<Vec<u8>>> = None;
    let mut meter_cancel: Option<oneshot::Sender<()>> = None;
    // Once credits run out the meter task exits for good. Without this flag a client
    // could simply send `start` again and get an UNMETERED translation session, because
    // nothing would be left charging for it.
    let mut exhausted = false;

    loop {
        tokio::select! {
            // Credits ran out: stop feeding the STT session but leave the socket up so
            // the client can show the purchase prompt rather than just dropping.
            _ = exhaust_rx.recv() => {
                exhausted = true;
                audio_tx = None;
                // The meter task has already exited; drop the handle so teardown does
                // not try to cancel a channel nobody is listening on.
                meter_cancel = None;
            }
            incoming = ws_rx.next() => {
                let Some(Ok(message)) = incoming else { break };
                match message {
                    Message::Binary(bytes) => {
                        if let Some(tx) = audio_tx.as_ref() {
                            // Bounded channel: if Deepgram stalls this fills and the
                            // session ends cleanly rather than buffering without limit.
                            if tx.send(bytes.to_vec()).await.is_err() {
                                audio_tx = None;
                            }
                        }
                    }
                    Message::Text(text) => {
                        match serde_json::from_str::<crate::protocol::ClientMessage>(&text) {
                            Ok(crate::protocol::ClientMessage::Start) => {
                                if audio_tx.is_some() {
                                    continue; // already streaming; a second start is a no-op
                                }
                                if exhausted {
                                    // The meter is gone; restarting here would translate
                                    // for free. Tell the client to buy credit instead.
                                    let _ = out_tx.send(
                                        ServerMessage::Error {
                                            message: "out of credit".to_string(),
                                            code: Some("insufficient_balance".to_string()),
                                        }
                                        .to_json(),
                                    );
                                    continue;
                                }
                                let ctx = SpeakerCtx {
                                    room: room.clone(),
                                    speaker_id: source_id.clone(),
                                    speaker_name: "Tab audio".to_string(),
                                    speaker_lang: state
                                        .rooms
                                        .peer_lang(&room, &source_id)
                                        .unwrap_or_else(|| source_lang.clone()),
                                    session_id: joined.session_id,
                                    speaker_user_id: Some(authed.user_id),
                                    // No room glossary for an extension session.
                                    glossary: None,
                                };
                                let deps = SessionDeps {
                                    rooms: state.rooms.clone(),
                                    moderator: state.moderator.clone(),
                                    // Transcripts are a call/webinar feature; an
                                    // extension session persists nothing.
                                    transcripts: None,
                                    participant_row: None,
                                    listener_pays: false,
                                    pcm_input: false,
                                    translator: state.translator.clone(),
                                };
                                match engine.start_session(ctx, deps).await {
                                    SessionOutcome::Started(tx) => {
                                        audio_tx = Some(tx);
                                        // Charge only while a session is really open.
                                        meter_cancel = start_meter();
                                    }
                                    SessionOutcome::AtCapacity | SessionOutcome::Failed => {
                                        let _ = out_tx.send(
                                            ServerMessage::Error {
                                                message: "translation service is busy".to_string(),
                                                code: Some("provider_unavailable".to_string()),
                                            }
                                            .to_json(),
                                        );
                                    }
                                }
                            }
                            Ok(crate::protocol::ClientMessage::Stop) => {
                                // Dropping the sender flushes and closes the STT session,
                                // and the meter stops with it — billing must not outlive
                                // the audio.
                                audio_tx = None;
                                if let Some(cancel) = meter_cancel.take() {
                                    let _ = cancel.send(());
                                }
                            }
                            Ok(crate::protocol::ClientMessage::SetLang { lang }) => {
                                // Changes the TARGET language mid-session. `auto` is
                                // rejected: it would make the fan-out skip this listener.
                                let lang = lang.trim().to_lowercase();
                                if valid_lang(&lang) && lang != "auto" {
                                    state.rooms.set_peer_lang(&room, &listener_id, &lang);
                                }
                            }
                            // Every other client message belongs to the call protocol and
                            // is meaningless here; ignoring it is not an error.
                            Ok(_) => {}
                            Err(_) => {}
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    }

    drop(audio_tx);
    send_task.abort();
    guard.finish(meter_cancel).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s256_matches_the_rfc_7636_test_vector() {
        // RFC 7636 Appendix B. If this drifts, every extension login breaks — and it
        // would break silently, as a "verifier does not match" the client cannot debug.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(s256(verifier), expected);
    }

    #[test]
    fn s256_is_base64url_without_padding() {
        let challenge = s256("a-perfectly-ordinary-verifier-value-0123456");
        assert_eq!(
            challenge.len(),
            43,
            "a SHA-256 digest is 43 base64url chars"
        );
        assert!(!challenge.contains('='), "no padding");
        assert!(
            !challenge.contains('+') && !challenge.contains('/'),
            "url-safe alphabet"
        );
    }

    #[test]
    fn s256_is_sensitive_to_every_byte() {
        assert_ne!(s256("verifier-one"), s256("verifier-two"));
        assert_ne!(s256("abc"), s256("abd"));
    }

    #[test]
    fn valid_lang_matches_the_ws_route_rules() {
        // Same rule as the room `/ws` route, because the value is interpolated into the
        // Deepgram streaming URL — a looser rule here would reopen that injection.
        assert!(valid_lang("it"));
        assert!(valid_lang("en"));
        assert!(valid_lang("pt-br"));
        assert!(valid_lang("auto"));

        assert!(!valid_lang(""), "empty");
        assert!(!valid_lang("toolonglang"), "over 8 chars");
        assert!(!valid_lang("en&redact=pci"), "query smuggling");
        assert!(!valid_lang("en us"), "whitespace");
        assert!(!valid_lang("en/../x"), "path traversal");
    }

    /// A code and a session token are both HS256 JWTs signed with the same secret, so
    /// the `kind` claim is the only thing stopping one being redeemed as the other.
    #[test]
    fn code_claims_carry_the_kind_marker() {
        let claims = CodeClaims {
            sub: Uuid::new_v4().to_string(),
            kind: CODE_KIND.to_string(),
            challenge: s256("some-verifier"),
            exp: 0,
        };
        let json = serde_json::to_string(&claims).unwrap();
        assert!(json.contains("vox_ext_code"));

        // A session token's claims (sub/email/name/exp) must NOT deserialize into a
        // code — the missing `kind` and `challenge` fields make it fail.
        let session_like = r#"{"sub":"u","email":"a@b.c","name":"A","exp":1}"#;
        assert!(serde_json::from_str::<CodeClaims>(session_like).is_err());
    }

    #[test]
    fn round_trips_a_code_and_rejects_a_tampered_one() {
        let secret = "test-secret";
        let claims = CodeClaims {
            sub: "usr".into(),
            kind: CODE_KIND.into(),
            challenge: s256("v"),
            exp: (chrono::Utc::now() + chrono::Duration::seconds(60)).timestamp() as usize,
        };
        let token = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap();

        let decoded = decode::<CodeClaims>(
            &token,
            &DecodingKey::from_secret(secret.as_bytes()),
            &Validation::new(jsonwebtoken::Algorithm::HS256),
        )
        .unwrap();
        assert_eq!(decoded.claims.kind, CODE_KIND);
        assert_eq!(decoded.claims.challenge, s256("v"));

        // Signed with another key → rejected.
        assert!(decode::<CodeClaims>(
            &token,
            &DecodingKey::from_secret(b"other"),
            &Validation::new(jsonwebtoken::Algorithm::HS256),
        )
        .is_err());
    }

    #[test]
    fn an_expired_code_is_rejected() {
        let secret = "test-secret";
        let claims = CodeClaims {
            sub: "usr".into(),
            kind: CODE_KIND.into(),
            challenge: s256("v"),
            // Well past jsonwebtoken's 60 s leeway.
            exp: (chrono::Utc::now() - chrono::Duration::seconds(600)).timestamp() as usize,
        };
        let token = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap();
        assert!(decode::<CodeClaims>(
            &token,
            &DecodingKey::from_secret(secret.as_bytes()),
            &Validation::new(jsonwebtoken::Algorithm::HS256),
        )
        .is_err());
    }
}
