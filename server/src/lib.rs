//! VoxTranslate server library — Axum WebSocket relay.
//!
//! V2: video-meeting model. The server relays WebRTC signaling between peers
//! (pure passthrough), orchestrates per-speaker Deepgram STT with translation
//! fan-out subtitles, and relays auto-translated chat. Rooms are capped at 4.
//!
//! `app()` builds the router from an [`AppState`]; `serve()` is the binary entry.

pub mod admin;
pub mod ai;
pub mod analytics;
pub mod api;
pub mod auth;
pub mod bench;
pub mod billing;
pub mod business;
pub mod cache;
pub mod config;
pub mod content;
pub mod db;
pub mod deepgram;
pub mod email;
pub mod email_template;
pub mod embeddings;
pub mod engine;
pub mod files;
pub mod friends;
pub mod glossary;
pub mod google_calendar;
pub mod google_oauth;
pub mod groq;
pub mod invite;
pub mod location;
pub mod log_shipping;
pub mod meetings;
pub mod metrics;
pub mod middleware;
pub mod moderation;
pub mod notifications;
pub mod notify_copy;
pub mod observability;
pub mod pdf;
pub mod protocol;
pub mod quiz_history;
pub mod rate_limit;
pub mod rooms;
pub mod safety;
pub mod storage;
pub mod stripe_handler;
pub mod subtitles;
pub mod transcripts;
pub mod translator;
pub mod usage;
pub mod webinar;

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc::{self, Receiver, UnboundedSender};
use tokio::sync::oneshot;
use tower_http::cors::CorsLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use uuid::Uuid;

use crate::auth::{GoogleVerifier, TokenVerifier};
use crate::billing::{usd, BillingService};
use crate::cache::TranslationCache;
use crate::config::Config;
use crate::db::Pool;
use crate::engine::{EngineRegistry, PremiumEngine, ProEngine, StandardEngine};
use crate::glossary::GlossaryService;
use crate::groq::Groq;
use crate::moderation::{Moderator, Severity};
use crate::protocol::{ClientMessage, RoomsResponse, ServerMessage, WsParams};
use crate::rate_limit::RateLimiter;
use crate::rooms::{LeaveOutcome, Peer, PeerTx, RoomManager, Visibility, OUT_CHANNEL_CAP};
use crate::safety::SafetyService;
use crate::transcripts::{EventKind, TranscriptEvent, TranscriptService};
use crate::translator::Translator;
use crate::usage::{run_guest_meter, run_usage_meter, MeterConfig, MeterScope};

// ---- Abuse-hardening limits (spec 0064) -----------------------------------
/// Default global cap on concurrent WS connections per instance; override with
/// `MAX_WS_CONNECTIONS`. This is the robust, IP-independent flood ceiling.
const DEFAULT_MAX_WS_CONNECTIONS: usize = 2000;
/// Max accepted WS frame size (binary audio + text). Real audio chunks are a few
/// hundred bytes and the largest signalling SDP is a few KB, so 64 KiB is generous.
const MAX_FRAME_BYTES: usize = 64 * 1024;
/// Default per-IP `/ws` connect attempts per minute (override `WS_CONNECT_MAX_PER_MIN`;
/// best-effort, the global cap is the real guard). Tunable so load tests / trusted
/// proxies can raise it.
const DEFAULT_WS_CONNECT_MAX_PER_MIN: u32 = 40;
/// Default per-IP `/rooms` + `/metrics` requests per minute (override `HTTP_PUBLIC_MAX_PER_MIN`).
const DEFAULT_HTTP_PUBLIC_MAX_PER_MIN: u32 = 60;
/// Per-connection message budget per [`WS_MSG_WINDOW`] before the socket is closed.
/// ≈100 msg/s sustained — far above audio (~10/s) + signalling/whiteboard bursts.
const WS_MSG_MAX: u32 = 500;
const WS_MSG_WINDOW: Duration = Duration::from_secs(5);
/// WebSocket keepalive (ghost-peer eviction). The server Pings each connection on
/// this cadence; browsers auto-reply with a Pong, which the reader counts as
/// activity. A connection that sends NO frame at all for [`WS_IDLE_TIMEOUT`] is a
/// frozen / half-open client — it's closed so its peer can't linger in the room as
/// a stale "ghost" tile (the disconnect cleanup then removes it for everyone).
const WS_PING_INTERVAL: Duration = Duration::from_secs(15);
const WS_IDLE_TIMEOUT: Duration = Duration::from_secs(45);

/// RAII guard for the live WS-connection counter: `acquire` increments and returns the
/// post-increment count; `Drop` decrements, so the count is balanced on every exit path.
struct ConnGuard(Arc<AtomicUsize>);

impl ConnGuard {
    fn acquire(counter: &Arc<AtomicUsize>) -> (Self, usize) {
        let count = counter.fetch_add(1, Ordering::Relaxed) + 1;
        (Self(counter.clone()), count)
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Rolling window over which a guest's cumulative STT time is tracked per IP.
/// Once it elapses the counter resets, so `GUEST_MAX_MINUTES` becomes a per-IP
/// budget *per window* rather than a per-connection cap that a reconnect resets (H1).
const GUEST_USAGE_WINDOW: Duration = Duration::from_secs(3600);

/// Cumulative guest speech-to-text usage, keyed by client IP (H1). The guest meter
/// ([`crate::usage::run_guest_meter`]) shares one counter per IP so an anonymous user
/// can't reset their `GUEST_MAX_MINUTES` cap by dropping and reopening the WebSocket.
/// Entries reset once [`GUEST_USAGE_WINDOW`] elapses, and are pruned when the map grows
/// large (same bound as the rate limiter).
#[derive(Default)]
pub struct GuestUsage {
    by_ip: dashmap::DashMap<String, (Arc<AtomicU64>, Instant)>,
}

impl GuestUsage {
    /// The shared cumulative-seconds counter for `ip`, resetting after the window.
    fn counter(&self, ip: &str) -> Arc<AtomicU64> {
        let now = Instant::now();
        if self.by_ip.len() > 10_000 {
            self.by_ip
                .retain(|_, (_, start)| now.duration_since(*start) <= GUEST_USAGE_WINDOW);
        }
        let mut entry = self
            .by_ip
            .entry(ip.to_string())
            .or_insert_with(|| (Arc::new(AtomicU64::new(0)), now));
        if now.duration_since(entry.1) > GUEST_USAGE_WINDOW {
            *entry = (Arc::new(AtomicU64::new(0)), now);
        }
        entry.0.clone()
    }
}

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub rooms: Arc<RoomManager>,
    pub translator: Translator,
    /// Translation-engine registry (spec 0093): Standard is always registered;
    /// Premium (OpenAI) is added when its key is configured. Routing resolves the
    /// per-speaker engine from this, never via `if engine == X`.
    pub engines: Arc<EngineRegistry>,
    /// Direct Groq chat client for the AI features (report, sentiment, email
    /// draft, suggestions). Shares the pooled HTTP client with `translator`.
    pub groq: Groq,
    /// OpenAI embeddings client for semantic transcript search (Business dashboard).
    /// `Some` whenever `OPENAI_API_KEY` is set — decoupled from the Pro engine flag.
    /// `None` ⇒ transcripts aren't embedded and `GET …/search` returns 503.
    pub embeddings: Option<embeddings::OpenAiEmbeddings>,
    /// Optional DragonflyDB translation cache (spec 0107). `Some` only when
    /// `TRANSLATION_CACHE_ENABLED` is set AND the lazy connect in [`AppState::init`]
    /// succeeded — fail-open, so a missing URL or an unreachable DragonflyDB just
    /// leaves this `None`. The same handle is folded into `translator`; it's kept
    /// here for the internal benchmark endpoint (spec 0107 R9).
    pub translation_cache: Option<Arc<TranslationCache>>,
    /// Postgres pool — `Some` only when auth/billing is configured.
    pub pool: Option<Pool>,
    /// Credit ledger service — `Some` only when auth/billing is configured.
    pub billing: Option<BillingService>,
    /// Trust & safety + GDPR service — `Some` only when the database is configured.
    pub safety: Option<SafetyService>,
    /// Transcript persistence/export — `Some` only when the database is configured.
    pub transcripts: Option<TranscriptService>,
    /// Room glossaries (spec 0011) — `Some` only when the database is configured.
    pub glossary: Option<GlossaryService>,
    /// Resend email client (spec 0016) — `Some` only when RESEND_* is configured.
    pub resend: Option<email::Resend>,
    /// Supabase Storage uploader (spec 0018) — `Some` only when SUPABASE_* is
    /// configured; gates chat file upload.
    pub storage: Option<storage::SupabaseStorage>,
    /// Private `recordings` bucket uploader for Business cloud recordings (spec
    /// 0106). Same Supabase project as `storage`, different bucket + 1h signed-URL
    /// TTL. `Some` whenever `storage` is.
    pub recordings_storage: Option<storage::SupabaseStorage>,
    /// Verifies Google credentials (swappable for tests).
    pub verifier: Arc<dyn TokenVerifier>,
    /// Shared HTTP client (Google tokeninfo, Stripe).
    pub http: reqwest::Client,
    /// Throttles auth + checkout endpoints.
    pub rate_limiter: Arc<RateLimiter>,
    /// Transcript/chat moderation (blocklist).
    pub moderator: Arc<Moderator>,
    /// Live WS connections on this instance — the global flood ceiling (spec 0064).
    pub ws_conns: Arc<AtomicUsize>,
    /// Max concurrent WS connections (`MAX_WS_CONNECTIONS`, default 2000).
    pub max_ws_conns: usize,
    /// Per-IP `/ws` connects per minute (`WS_CONNECT_MAX_PER_MIN`, default 40).
    pub ws_connect_max: u32,
    /// Per-IP `/rooms` + `/metrics` requests per minute (`HTTP_PUBLIC_MAX_PER_MIN`, default 60).
    pub http_public_max: u32,
    /// Optional bearer token gating `/metrics` (`METRICS_TOKEN`); open when unset.
    pub metrics_token: Option<String>,
    /// Optional shared secret that Cloudflare injects as `x-origin-verify` (#111).
    /// When set, [`origin_lock`] rejects any request (except `/health`) that lacks
    /// it — blocking direct-to-origin access that bypasses the Cloudflare WAF.
    /// `None` (default) ⇒ the guard is dormant.
    pub cf_origin_secret: Option<String>,
    /// Cumulative guest STT usage per IP (H1) — makes `GUEST_MAX_MINUTES` survive
    /// reconnects instead of resetting per connection.
    pub guest_usage: Arc<GuestUsage>,
    /// Process-wide semaphore for voice-assistant sessions (spec voice-assistant).
    /// `Some` only when `VoiceAssistantConfig` is present; cap = `max_sessions`.
    /// `None` when the voice assistant is disabled (route is not registered).
    pub voice_assistant_semaphore: Option<Arc<tokio::sync::Semaphore>>,
    /// Process-wide semaphore for help-assistant sessions.
    /// `Some` only when `HelpAssistantConfig` is present; cap = `max_sessions`.
    /// `None` when the help assistant is disabled (route is not registered).
    pub help_assistant_semaphore: Option<Arc<tokio::sync::Semaphore>>,
}

/// Read a positive `u32` from `var`, falling back to `default`.
fn env_u32(var: &str, default: u32) -> u32 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(default)
}

/// Read a positive `u64` from `var`, falling back to `default`.
fn env_u64(var: &str, default: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(default)
}

/// Lazily connect the translation cache (spec 0107), **fail-open**: returns `None`
/// — and logs why — when the flag is off, `DRAGONFLY_PRIVATE_URL` is unset, or the
/// DragonflyDB connection fails. The server then runs on the byte-for-byte
/// pre-cache path, so startup never fails on the cache (R2). `BENCH_SECRET` and the
/// connection URL are never logged.
async fn connect_translation_cache(config: &Config) -> Option<Arc<TranslationCache>> {
    if !config.cache_enabled {
        return None;
    }
    match config.dragonfly_url.as_deref() {
        Some(url) => match TranslationCache::connect(url).await {
            Ok(c) => {
                tracing::info!("translation cache connected (DragonflyDB)");
                Some(Arc::new(c))
            }
            Err(e) => {
                tracing::warn!("translation cache unavailable: {e} — continuing without cache");
                None
            }
        },
        None => {
            tracing::warn!(
                "TRANSLATION_CACHE_ENABLED is set but DRAGONFLY_PRIVATE_URL is unset — continuing without cache"
            );
            None
        }
    }
}

impl AppState {
    /// Build state from a [`Config`] **without** touching the database or the
    /// translation cache. The pool and `translation_cache` stay `None`; use
    /// [`AppState::init`] to connect + migrate (and lazily connect DragonflyDB)
    /// when configured.
    pub fn new(config: Config) -> Self {
        Self::with_translation_cache(config, None)
    }

    /// Inner constructor shared by [`AppState::new`] (cache `None`) and
    /// [`AppState::init`] (which lazily connects DragonflyDB first, spec 0107).
    /// The cache is folded into `translator` so the live fan-out and the bench
    /// endpoint share one handle, and stored on state for the bench endpoint.
    fn with_translation_cache(
        config: Config,
        translation_cache: Option<Arc<TranslationCache>>,
    ) -> Self {
        let config = Arc::new(config);
        let groq = Groq::new(config.groq_key.clone(), config.translation_model.clone());
        // Admission cap on concurrent in-flight Groq translation calls across the
        // whole process (spec 0069) — bounds fan-out under a traffic spike.
        let translate_max = env_u32(
            "GROQ_TRANSLATE_MAX_CONCURRENCY",
            translator::DEFAULT_MAX_INFLIGHT as u32,
        ) as usize;
        // Fold the (optional) cache in; `None` leaves the byte-for-byte pre-cache
        // path (spec 0107 R1).
        let translator = Translator::with_max_inflight(groq.clone(), translate_max).with_cache(
            translation_cache.clone(),
            config.cache_max_words,
            config.cache_ttl_secs,
        );
        let http = reqwest::Client::new();
        let client_id = config
            .billing
            .as_ref()
            .map(|b| b.google_client_id.clone())
            .unwrap_or_default();
        let verifier: Arc<dyn TokenVerifier> =
            Arc::new(GoogleVerifier::new(client_id, http.clone()));
        let resend = config
            .resend
            .as_ref()
            .map(|c| email::Resend::new(http.clone(), c));
        let storage = config
            .storage
            .as_ref()
            .map(|c| storage::SupabaseStorage::new(http.clone(), c));
        // Recordings live in a separate private bucket on the same project, with a
        // 1h signed-URL TTL for playback (spec 0106). Built whenever storage is.
        let recordings_storage = config.storage.as_ref().map(|c| {
            let bucket = std::env::var("SUPABASE_RECORDINGS_BUCKET")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "recordings".to_string());
            let ttl = env_u64("SUPABASE_RECORDINGS_TTL_SECS", 3600);
            storage::SupabaseStorage::for_bucket(http.clone(), c, bucket, ttl)
        });
        // Engine registry (spec 0093): Standard wraps the Deepgram+Groq pipeline
        // and is always available; Premium (OpenAI) is registered separately when
        // its key is configured. Per-session services (rooms, moderator,
        // transcripts) are passed at speak time, not captured here, so the
        // registry can be built before `init` connects the database.
        let mut registry = EngineRegistry::new(crate::engine::STANDARD_ID);
        // Standard (Deepgram + Groq) is the base tier and the registry's default. Gated by
        // `DEEPGRAM_STANDARD` (default ON) for symmetry with the optional tiers; a
        // fail-safe below force-registers it if the flag left it off, so the app always
        // has a default/capacity-fallback engine (spec 0101).
        if config.standard_enabled {
            registry.register(Arc::new(StandardEngine::new(
                config.clone(),
                http.clone(),
                translator.clone(),
            )));
        }
        // Registration order = display order: Standard, Enhanced, Pro, Premium. Enhanced
        // (Cartesia, spec 0108) is the client-direct tier between Standard and Pro; it ships
        // dark behind the CARTESIA_ENHANCED flag (gated in `config.cartesia`).
        if let Some(ca) = config.cartesia.as_ref() {
            registry.register(Arc::new(crate::engine::CartesiaEngine::new(ca)));
        }
        // The OpenAI engine is the "Pro" tier (its stable id is still `premium`); register
        // it BEFORE Gemini so Pro shows above Premium. Ships dark until its key is set.
        if let Some(oa) = config.openai.as_ref() {
            registry.register(Arc::new(ProEngine::new(oa)));
        }
        // Gemini Live Translate is the "Premium" tier (spec 0100), shown last. Like the
        // others it ships dark until its key is provisioned.
        if let Some(g) = config.google.as_ref() {
            registry.register(Arc::new(PremiumEngine::new(g)));
        }
        // Fail-safe (spec 0101): the registry's default engine (Standard) must exist, or
        // `resolve()`/`default()` panic and the whole call path breaks. If `DEEPGRAM_STANDARD`
        // was set to 0, force-register Standard now (it lands last in display order, but the
        // app stays up). Standard is a base tier, not a real kill switch.
        if registry.get(crate::engine::STANDARD_ID).is_none() {
            tracing::warn!(
                "DEEPGRAM_STANDARD is off but Standard is the mandatory default engine — force-registering it"
            );
            registry.register(Arc::new(StandardEngine::new(
                config.clone(),
                http.clone(),
                translator.clone(),
            )));
        }
        let engines = Arc::new(registry);
        // Client-direct engines (spec 0101): tell the room manager which engine ids
        // translate in the browser so its "serve everyone" subtitle fan-out skips those
        // listeners (they render their own). Computed from the live registry, so it's
        // non-empty only when such an engine (Cartesia "Enhanced") is registered.
        let client_direct_engines: std::collections::HashSet<String> = engines
            .list()
            .filter(|e| e.metadata().capabilities.client_direct)
            .map(|e| e.metadata().id.clone())
            .collect();
        let rooms = Arc::new(RoomManager::new());
        rooms.init_client_direct_engines(client_direct_engines);
        // OpenAI embeddings for semantic transcript search — built whenever the key is
        // configured (independent of the Pro engine rollout flag).
        let embeddings = config
            .embeddings
            .as_ref()
            .map(embeddings::OpenAiEmbeddings::from_config);
        // Voice-assistant semaphore — process-wide cap on concurrent relay sessions.
        // Built only when the voice assistant is enabled (same gate as the route).
        let voice_assistant_semaphore = config
            .voice_assistant
            .as_ref()
            .map(|va| Arc::new(tokio::sync::Semaphore::new(va.max_sessions.max(1))));
        // Help-assistant semaphore — independent cap for the dashboard help relay.
        let help_assistant_semaphore = config
            .help_assistant
            .as_ref()
            .map(|ha| Arc::new(tokio::sync::Semaphore::new(ha.max_sessions.max(1))));
        Self {
            config,
            rooms,
            translator,
            engines,
            groq,
            embeddings,
            translation_cache,
            pool: None,
            billing: None,
            safety: None,
            transcripts: None,
            glossary: None,
            resend,
            storage,
            recordings_storage,
            verifier,
            http,
            rate_limiter: Arc::new(RateLimiter::new()),
            moderator: Arc::new(Moderator::from_env()),
            ws_conns: Arc::new(AtomicUsize::new(0)),
            max_ws_conns: std::env::var("MAX_WS_CONNECTIONS")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|&n| n > 0)
                .unwrap_or(DEFAULT_MAX_WS_CONNECTIONS),
            ws_connect_max: env_u32("WS_CONNECT_MAX_PER_MIN", DEFAULT_WS_CONNECT_MAX_PER_MIN),
            http_public_max: env_u32("HTTP_PUBLIC_MAX_PER_MIN", DEFAULT_HTTP_PUBLIC_MAX_PER_MIN),
            metrics_token: std::env::var("METRICS_TOKEN")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            cf_origin_secret: std::env::var("CF_ORIGIN_SECRET")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            guest_usage: Arc::new(GuestUsage::default()),
            voice_assistant_semaphore,
            help_assistant_semaphore,
        }
    }

    /// Build state and, when billing is configured, connect the database and run
    /// migrations. Also lazily connects the translation cache (spec 0107) when
    /// enabled — fail-open, so an unreachable DragonflyDB never blocks startup. In
    /// guest-only mode the rest is just [`AppState::new`] with the connected cache.
    pub async fn init(config: Config) -> Result<Self, String> {
        let translation_cache = connect_translation_cache(&config).await;
        let mut state = Self::with_translation_cache(config, translation_cache);
        if let Some(billing) = state.config.billing.clone() {
            let pool = db::connect(&billing.database_url)
                .await
                .map_err(|e| format!("database connect failed: {e}"))?;
            db::migrate(&pool)
                .await
                .map_err(|e| format!("migrations failed: {e}"))?;
            // Best-effort: add the PostGIS geometry column for the Directus user map.
            // Never fatal — a DB without PostGIS just logs and skips it.
            location::ensure_geometry(&pool).await;
            let min_join = usd(billing.pricing.min_balance_to_join);
            state.billing = Some(BillingService::new(pool.clone(), min_join));
            state.safety = Some(SafetyService::new(pool.clone()));
            state.transcripts = Some(TranscriptService::new(pool.clone()));
            state.glossary = Some(GlossaryService::new(
                pool.clone(),
                billing.glossary_max_entries,
            ));
            // Layer the DB-managed blocklist over the env baseline.
            let db_terms = content::load_blocklist_terms(&pool).await;
            state.moderator = Arc::new(Moderator::from_env().with_terms(db_terms));
            state.pool = Some(pool);
            tracing::info!(
                "auth/billing enabled — database connected, migrations applied, {} blocklist terms",
                state.moderator.len()
            );
        } else {
            tracing::info!("guest-only mode — no auth/billing configured");
        }
        Ok(state)
    }
}

/// Build the Axum router (routes + middleware) for the given state.
pub fn app(state: AppState) -> Router {
    // CORS (spec 0028): when ALLOWED_ORIGINS is set, restrict to that allowlist;
    // otherwise stay permissive for local dev. Previously `permissive()` was used
    // on every route and the parsed `allowed_origins` was dead code, so any site
    // could call the API from a browser. (Auth is a Bearer header, no cookies, so
    // this was not credentialed-CSRF — but it left expensive endpoints open.)
    //
    // M8 — fail-closed in production: if the allowlist is empty but billing is
    // configured (i.e. this is a real deployment, not guest-only/dev), don't fall
    // back to permissive. Derive the allowlist from our own known origins
    // (`APP_BASE_URL` + `DASHBOARD_BASE_URL`) so a missing `ALLOWED_ORIGINS` can't
    // silently leave the API open to every origin.
    let mut allowed = state.config.allowed_origins.clone();
    if allowed.is_empty() && state.config.billing.is_some() {
        allowed = [
            state.config.app_base_url.clone(),
            state.config.dashboard_base_url.clone(),
        ]
        .into_iter()
        .filter(|o| !o.is_empty())
        .collect();
        tracing::warn!(
            "ALLOWED_ORIGINS unset in a billing deployment — restricting CORS to \
             app_base_url + dashboard_base_url instead of permissive (M8)"
        );
    }
    let cors = if allowed.is_empty() {
        CorsLayer::permissive()
    } else {
        let origins: Vec<axum::http::HeaderValue> =
            allowed.iter().filter_map(|o| o.parse().ok()).collect();
        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods([
                axum::http::Method::GET,
                axum::http::Method::POST,
                axum::http::Method::PATCH,
                axum::http::Method::DELETE,
                axum::http::Method::OPTIONS,
            ])
            .allow_headers([
                axum::http::header::AUTHORIZATION,
                axum::http::header::CONTENT_TYPE,
            ])
    };
    Router::new()
        .route("/ws", get(ws_handler))
        .route("/rooms", get(rooms_handler))
        .route("/health", get(|| async { "ok" }))
        .route("/version", get(version_handler))
        .route("/metrics", get(metrics_handler))
        .route("/api/auth/config", get(auth::auth_config))
        .route("/api/ice", get(api::ice))
        .route("/api/bug-report", post(api::bug_report))
        .route("/api/contact", post(api::contact))
        .route("/api/auth/google", post(auth::auth_google))
        .route(
            "/api/auth/google/connection",
            axum::routing::delete(auth::disconnect_google),
        )
        .route("/api/user/me", get(auth::user_me))
        // ---- Personal (B2C) scheduled meetings on Google Calendar ----
        .route("/api/meetings", get(meetings::list).post(meetings::create))
        .route("/api/meetings/{meeting_id}", get(meetings::get))
        .route("/api/meetings/{meeting_id}/cancel", post(meetings::cancel))
        // ---- Friends: list, requests, send/accept/remove, invite-to-call ----
        .route("/api/friends", get(friends::list))
        .route("/api/friends/requests", get(friends::requests))
        .route("/api/friends/request", post(friends::request))
        .route("/api/friends/{id}/accept", post(friends::accept))
        .route("/api/friends/{id}/invite", post(friends::invite))
        .route("/api/friends/{id}", axum::routing::delete(friends::remove))
        // ---- Notifications: web-push, preferences, in-app center ----
        .route(
            "/api/push/subscribe",
            post(notifications::subscribe).delete(notifications::unsubscribe),
        )
        .route(
            "/api/push/vapid-public-key",
            get(notifications::vapid_public_key),
        )
        .route(
            "/api/notifications/preferences",
            get(notifications::get_preferences).patch(notifications::patch_preferences),
        )
        .route("/api/notifications", get(notifications::list))
        .route(
            "/api/notifications/{id}/read",
            post(notifications::mark_read),
        )
        .route(
            "/api/notifications/read-all",
            post(notifications::mark_all_read),
        )
        .route("/api/engines", get(api::engines))
        .route(
            "/api/sessions/enhanced/session",
            post(api::enhanced_session),
        )
        .route("/api/sessions/enhanced/clone-voice", post(api::clone_voice))
        .route("/api/billing/packages", get(api::billing_packages))
        .route("/api/billing/checkout", post(api::billing_checkout))
        .route("/api/billing/webhook", post(api::billing_webhook))
        .route("/api/billing/history", get(api::billing_history))
        .route("/api/billing/ai-pricing", get(api::ai_pricing))
        .route("/api/usage/sessions", get(api::usage_sessions))
        .route("/api/sessions", get(api::sessions_list))
        .route(
            "/api/sessions/{id}/transcript.json",
            get(api::transcript_json),
        )
        .route(
            "/api/sessions/{id}/transcript.pdf",
            get(api::transcript_pdf),
        )
        .route(
            "/api/sessions/{id}/transcript.srt",
            get(api::transcript_srt),
        )
        .route(
            "/api/sessions/{id}/transcript.vtt",
            get(api::transcript_vtt),
        )
        .route(
            "/api/sessions/{id}/report",
            get(api::report_latest).post(api::report_generate),
        )
        .route(
            "/api/sessions/{id}/quizzes",
            get(api::quizzes_list).post(api::quiz_save),
        )
        .route(
            "/api/sessions/{id}/sentiment",
            get(api::sentiment_latest).post(api::sentiment_generate),
        )
        .route(
            "/api/sessions/{id}/correction",
            get(api::correction_status).post(api::correction_generate),
        )
        // Poll a background AI job (report / correction / email draft): the heavy
        // POSTs return a job handle and the client polls this until done/failed.
        .route(
            "/api/sessions/{id}/ai-job/{job_id}",
            get(api::ai_job_status),
        )
        .route("/api/quiz/generate", post(api::quiz_generate))
        .route(
            "/api/sessions/{id}/email-draft",
            post(api::email_draft_generate),
        )
        .route("/api/sessions/{id}/email-send", post(api::email_send))
        .route("/api/sessions/{id}/email", get(api::email_latest))
        .route("/api/rooms/{room}/invite", post(api::invite_send))
        .route(
            "/api/sessions/{id}/bookmarks",
            get(api::bookmarks_list).post(api::bookmark_add),
        )
        .route(
            "/api/sessions/{id}/bookmarks/{bid}",
            axum::routing::patch(api::bookmark_update).delete(api::bookmark_delete),
        )
        .route(
            "/api/rooms/{room}/glossary",
            get(api::glossary_get)
                .post(api::glossary_save)
                .delete(api::glossary_delete),
        )
        .route(
            "/api/rooms/{room}/glossary/import",
            post(api::glossary_import),
        )
        // Chat file upload (spec 0018). Public feature probe + the upload route.
        // Multipart can carry a file up to the configured cap, so the upload
        // route raises Axum's default 2 MB body limit; the handler enforces the
        // exact `storage.max_bytes`.
        .route("/api/files/config", get(files::files_config))
        .route(
            "/api/rooms/{room}/files",
            post(files::upload_file).layer(DefaultBodyLimit::max(files::MAX_BODY_BYTES)),
        )
        .route("/api/report", post(api::report))
        .route("/api/user/consent", post(api::submit_consent))
        .route("/api/user/tts-prefs", post(api::update_tts_prefs))
        .route("/api/user/data", get(api::export_data))
        .route(
            "/api/user/location",
            post(location::update_location).delete(location::clear_location),
        )
        .route("/api/user", axum::routing::delete(api::delete_account))
        // Public, read-only managed content (client merges over its bundled copy).
        .route("/api/content/i18n", get(content::get_i18n))
        .route("/api/content/legal/{slug}", get(content::get_legal))
        // Backoffice admin actions (Directus → secret-guarded, server-to-server).
        .route("/api/admin/ban", post(admin::ban))
        .route("/api/admin/unban", post(admin::unban))
        .route("/api/admin/credit", post(admin::credit))
        .route("/api/admin/bonus", post(admin::bonus))
        .route(
            "/api/admin/org/gift-subscription",
            post(admin::gift_subscription),
        )
        .route("/api/admin/report/resolve", post(admin::resolve_report))
        .route("/api/admin/user/delete", post(admin::delete_user))
        .route("/api/admin/rls/enforce", post(admin::enforce_rls))
        // Internal benchmark endpoint (spec 0107) — guarded by `BENCH_SECRET`
        // (404 when unset, 401 on a wrong token); intentionally undocumented.
        .route("/internal/bench/translate", post(bench::translate))
        // Internal one-time embeddings backfill — guarded by `EMBEDDINGS_BACKFILL_SECRET`
        // (404 when unset). Embeds pre-existing transcripts in resumable batches.
        .route(
            "/internal/embeddings/backfill",
            post(business::search::backfill),
        )
        // VoxTranslate for Business — org workspace API (spec 0106).
        .merge(business::routes::routes())
        // B2B Voice Assistant — registered only when config is present (ships dark).
        .merge(if state.config.voice_assistant.is_some() {
            business::routes::voice_assistant_routes()
        } else {
            axum::Router::new()
        })
        // Dashboard Help Assistant — registered only when config is present (ships dark).
        // When absent the route is 404 — satisfies spec feature-flag-disabled behavior.
        .merge(if state.config.help_assistant.is_some() {
            business::routes::help_assistant_routes()
        } else {
            axum::Router::new()
        })
        // Webinar Mode — 1-to-many broadcast API. Registered only when
        // config.webinar is set (MEDIA_INGEST_HOST); otherwise ships dark (404).
        .merge(if state.config.webinar.is_some() {
            webinar::routes::routes()
        } else {
            axum::Router::new()
        })
        // Canonical log line + request-id span per request (spec 0050).
        .layer(axum::middleware::from_fn(observability::canonical_log))
        .layer(cors)
        // Don't let browsers MIME-sniff responses (matters for the file/transcript
        // download endpoints) — spec 0028.
        .layer(SetResponseHeaderLayer::if_not_present(
            axum::http::header::X_CONTENT_TYPE_OPTIONS,
            axum::http::HeaderValue::from_static("nosniff"),
        ))
        // Origin lock (#111): outermost, so direct-to-origin requests that bypass the
        // Cloudflare WAF are rejected before any work. Dormant unless CF_ORIGIN_SECRET
        // is set; always exempts /health (Railway's healthcheck hits the origin directly).
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            origin_lock,
        ))
        .with_state(state)
}

/// Reject requests that didn't come through Cloudflare (#111). When
/// `CF_ORIGIN_SECRET` is configured, every request except `/health` and `/version`
/// must carry the matching `x-origin-verify` header (injected by a Cloudflare
/// Transform Rule); otherwise it's a `403`. `/health` is always allowed because
/// Railway's platform healthcheck hits the origin directly, bypassing Cloudflare —
/// blocking it would fail every deploy; `/version` stays open for deploy checks.
/// No-op (pass-through) when the secret is unset.
async fn origin_lock(
    State(state): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if let Some(secret) = state.cf_origin_secret.as_deref() {
        // `/health` and `/version` are always allowed: the former is Railway's
        // healthcheck (hits the origin directly), the latter must stay curl-able for
        // deploy verification even when the origin lock is armed.
        //
        // `/internal/media-auth/*` is also exempt: the off-box MediaMTX calls it
        // server-to-server via the direct Railway origin (bypassing Cloudflare, whose
        // bot challenge a headless Go client can't solve), so it never carries the
        // CF-injected header. It's independently protected by the caller secret in the
        // path + the HMAC publish-token check, so skipping the origin lock is safe.
        let path = req.uri().path();
        let exempt =
            matches!(path, "/health" | "/version") || path.starts_with("/internal/media-auth/");
        if !exempt && !origin_header_ok(req.headers(), secret) {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    next.run(req).await
}

/// Build/version info for deploy verification. Deliberately public (no auth, exempt
/// from the origin lock like `/health`) so a deployment can be confirmed with a plain
/// `curl /version`. `version` is the crate semver baked in at compile time — bumped on
/// every release, so it authoritatively identifies the shipped build. `commit` is
/// best-effort: the git SHA when the build or runtime environment supplies one
/// (`GIT_SHA`, or Railway's `RAILWAY_GIT_COMMIT_SHA`), otherwise `"unknown"`.
async fn version_handler() -> Json<serde_json::Value> {
    let commit = option_env!("GIT_SHA")
        .map(str::to_string)
        .or_else(|| std::env::var("GIT_SHA").ok())
        .or_else(|| std::env::var("RAILWAY_GIT_COMMIT_SHA").ok())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "commit": commit,
    }))
}

/// Whether the request carries the expected Cloudflare-injected origin secret.
/// Pure (no state) so it's unit-testable. Compared in constant time so a wrong
/// header can't recover the secret byte-by-byte via response timing.
fn origin_header_ok(headers: &HeaderMap, secret: &str) -> bool {
    match headers.get("x-origin-verify").and_then(|v| v.to_str().ok()) {
        Some(presented) => ct_eq(presented.as_bytes(), secret.as_bytes()),
        None => false,
    }
}

/// Length-checked, branch-free byte comparison (avoids leaking a secret prefix via
/// timing). Matches the convention used in `admin`/`stripe_handler`.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Binary entry point: load config, build state, bind, and serve.
pub async fn serve() {
    dotenvy::dotenv().ok();
    observability::init_tracing(); // structured logs + canonical lines (spec 0050)

    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("configuration error: {e}");
            std::process::exit(1);
        }
    };

    // M7 — behind Cloudflare (CF_ORIGIN_SECRET set) the app sees Cloudflare's edge IP
    // as the last X-Forwarded-For hop, so unless CLIENT_IP_HEADER=cf-connecting-ip is
    // set every per-IP bucket keys on ONE shared IP and per-IP throttling becomes
    // global/meaningless. Warn loudly rather than silently degrade.
    let cf_deployment = std::env::var("CF_ORIGIN_SECRET")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .is_some();
    if cf_deployment {
        let ip_header = std::env::var("CLIENT_IP_HEADER")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if ip_header != "cf-connecting-ip" {
            tracing::warn!(
                "CF_ORIGIN_SECRET is set (Cloudflare deployment) but CLIENT_IP_HEADER \
                 is not 'cf-connecting-ip' — per-IP rate limits will collapse onto \
                 Cloudflare's edge IP. Set CLIENT_IP_HEADER=cf-connecting-ip."
            );
        }
    }

    let port = config.port;
    // Resilient startup: if billing is configured but the database can't be
    // reached/migrated, log it and fall back to guest-only mode instead of
    // crashing — the core call/translation features stay up. (For Supabase,
    // DATABASE_URL must be the IPv4 *connection pooler*, not the direct host.)
    let state = match AppState::init(config.clone()).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(
                "billing/database init failed ({e}); falling back to GUEST-ONLY mode \
                 — check DATABASE_URL (Supabase: use the IPv4 connection pooler)"
            );
            let mut guest = config;
            guest.billing = None;
            AppState::new(guest)
        }
    };

    // Periodic cleanup of rooms whose peers have all disconnected; their call
    // sessions are finalized (flush + ended_at + guest-only purge).
    let rooms = state.rooms.clone();
    let transcripts = state.transcripts.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        loop {
            tick.tick().await;
            for sid in rooms.prune() {
                if let Some(svc) = transcripts.as_ref() {
                    if let Err(e) = svc.finalize_session(sid).await {
                        tracing::error!("finalize pruned session {sid} failed: {e}");
                    }
                }
            }
        }
    });

    // Analytics roll-up (#241): periodically fold raw events into the aggregate
    // tables Directus dashboards read. Skipped in guest-only mode (no DB).
    if let Some(pool) = state.pool.clone() {
        tokio::spawn(crate::analytics::run_rollup(pool, Duration::from_secs(300)));
    }

    // Scheduled-meeting reminder scheduler (spec: scheduled meetings, Phase 1d).
    // Emits `meeting_reminder` notifications when a meeting's lead time is reached.
    if state.pool.is_some() {
        tokio::spawn(crate::notifications::run_reminder_scheduler(
            state.clone(),
            Duration::from_secs(30),
        ));
    }

    // Enterprise data-retention sweep (spec 0106): delete recordings + transcripts
    // past each org's retention window. Ships dormant — only runs when
    // RETENTION_SWEEP_ENABLED is set AND a DB is present. Recordings storage is
    // additionally required to purge the storage objects themselves.
    if state.config.retention_sweep_enabled {
        if state.pool.is_some() {
            let interval = Duration::from_secs(state.config.retention_sweep_interval_secs.max(60));
            let batch = state.config.retention_sweep_batch.max(1);
            tracing::info!(
                "Enterprise data-retention sweep enabled (every {}s, batch {batch})",
                interval.as_secs()
            );
            tokio::spawn(crate::business::retention::run_sweep(
                state.clone(),
                interval,
                batch,
            ));
        } else {
            tracing::warn!("RETENTION_SWEEP_ENABLED set but no database — sweep not started");
        }
    }

    let addr = format!("0.0.0.0:{port}");
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("failed to bind {addr}: {e}");
            std::process::exit(1);
        }
    };

    tracing::info!("VoxTranslate server listening on {addr}");
    if let Err(e) = axum::serve(listener, app(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        tracing::error!("server error: {e}");
    }
}

/// Max accepted length for the join `room`/`name` query params — bounds per-peer /
/// per-room memory and keeps room keys sane (the DB columns and client room codes are
/// well under this). Cosmetic/DoS-adjacent hardening, not an exploit fix on its own.
const MAX_ROOM_LEN: usize = 128;
const MAX_NAME_LEN: usize = 100;

async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(mut params): Query<WsParams>,
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Response {
    if params.room.trim().is_empty() || params.lang.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "missing room or lang").into_response();
    }
    // Validate + normalize the language BEFORE it is interpolated into the Deepgram
    // streaming URL (M4). Without this a peer could smuggle extra query params via
    // `?lang=en%26redact%3Dpci`. Matches the mid-call `set_lang` rules but keeps
    // "auto" (join-time auto-detect is legitimate; set_lang rejects it as a manual
    // override).
    let norm_lang = params.lang.trim().to_lowercase();
    let lang_ok = (1..=8).contains(&norm_lang.len())
        && norm_lang
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-');
    if !lang_ok {
        return (StatusCode::BAD_REQUEST, "invalid language code").into_response();
    }
    params.lang = norm_lang;
    // Bound room/name lengths so a crafted join can't store oversized strings per
    // peer/room (billing #5 / injection minor).
    if params.room.trim().len() > MAX_ROOM_LEN {
        return (StatusCode::BAD_REQUEST, "room too long").into_response();
    }
    if let Some(name) = params.name.as_ref() {
        if name.len() > MAX_NAME_LEN {
            return (StatusCode::BAD_REQUEST, "name too long").into_response();
        }
    }
    // Per-IP connect throttle (spec 0064). Best-effort (IP from X-Forwarded-For behind
    // Railway's proxy); the global cap in handle_peer is the robust flood ceiling.
    let ip = observability::client_ip(&headers);
    if !state.rate_limiter.allow(
        &format!("wsconnect:{ip}"),
        state.ws_connect_max,
        Duration::from_secs(60),
    ) {
        return (StatusCode::TOO_MANY_REQUESTS, "too many connections").into_response();
    }
    ws.on_upgrade(move |socket| handle_peer(socket, params, state, ip))
}

/// Lobby: list public rooms with their currently online participants. Per-IP throttled
/// (spec 0064) — it's unauthenticated and otherwise freely scrapable.
async fn rooms_handler(headers: HeaderMap, State(state): State<AppState>) -> Response {
    let ip = observability::client_ip(&headers);
    if !state.rate_limiter.allow(
        &format!("rooms:{ip}"),
        state.http_public_max,
        Duration::from_secs(60),
    ) {
        return (StatusCode::TOO_MANY_REQUESTS, "slow down").into_response();
    }
    Json(RoomsResponse {
        rooms: state.rooms.public_rooms(),
    })
    .into_response()
}

/// `GET /metrics` — Prometheus exposition (spec 0058). Non-sensitive aggregates only
/// (request counters/latency + live room/peer gauges for THIS instance). Per-IP throttled,
/// and gated by `METRICS_TOKEN` when set so the operational view isn't world-readable (0064).
async fn metrics_handler(headers: HeaderMap, State(state): State<AppState>) -> Response {
    if let Some(token) = &state.metrics_token {
        let presented = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "));
        let ok = presented
            .map(|p| ct_eq(p.as_bytes(), token.as_bytes()))
            .unwrap_or(false);
        if !ok {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    let ip = observability::client_ip(&headers);
    if !state.rate_limiter.allow(
        &format!("metrics:{ip}"),
        state.http_public_max,
        Duration::from_secs(60),
    ) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let body = metrics::render(
        state.rooms.active_rooms() as u64,
        state.rooms.active_peers() as u64,
    );
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// An authenticated, billable peer: their user id, Google avatar, and cloned voice.
#[derive(Clone)]
struct AuthedPeer {
    user_id: Uuid,
    avatar_url: Option<String>,
    /// Cartesia cloned-voice id (spec 0108), if the user has completed voice prep.
    cartesia_voice_id: Option<String>,
}

/// Resolve the (optional) billed user for a WS connection from its token:
/// - `Ok(None)`       — guest (no token, or billing not configured here);
/// - `Ok(Some(peer))` — authenticated user with enough balance to join;
/// - `Err(msg)`       — reject the connection with this error frame.
async fn authorize(
    state: &AppState,
    token: Option<&str>,
) -> Result<Option<AuthedPeer>, ServerMessage> {
    let Some(token) = token.map(str::trim).filter(|t| !t.is_empty()) else {
        return Ok(None); // no token -> guest
    };
    // A token was supplied but this server has no billing — treat as guest.
    let (Some(cfg), Some(svc)) = (state.config.billing.as_ref(), state.billing.as_ref()) else {
        return Ok(None);
    };

    let claims =
        crate::auth::verify_jwt(&cfg.jwt_secret, token).map_err(|_| ServerMessage::Error {
            message: "invalid or expired session".to_string(),
            code: Some("invalid_token".to_string()),
        })?;
    let uid = Uuid::parse_str(&claims.sub).map_err(|_| ServerMessage::Error {
        message: "invalid session".to_string(),
        code: Some("invalid_token".to_string()),
    })?;

    // Banned users can't join (regardless of balance).
    if let Some(safety) = state.safety.as_ref() {
        if let Ok(Some(reason)) = safety.is_banned(uid).await {
            return Err(ServerMessage::Error {
                message: format!("You are banned: {reason}"),
                code: Some("banned".to_string()),
            });
        }

        // Age + ToS consent gate: the client shows a blocking consent modal, but enforce
        // 18+ server-side too so the WS can't be joined by bypassing the UI. Guests (no
        // token) returned above are exempt. Fails closed (like `can_join`) on DB error.
        match safety.has_consented(uid).await {
            Ok(true) => {}
            Ok(false) => {
                return Err(ServerMessage::Error {
                    message: "Please confirm you are 18+ and accept the Terms to continue"
                        .to_string(),
                    code: Some("consent_required".to_string()),
                });
            }
            Err(e) => {
                tracing::error!("consent check failed: {e}");
                return Err(ServerMessage::Error {
                    message: "service unavailable".to_string(),
                    code: Some("consent_unavailable".to_string()),
                });
            }
        }
    }

    match svc.can_join(uid).await {
        Ok(true) => {
            let avatar_url = svc.get_avatar(uid).await.unwrap_or_default();
            // Cloned-voice id (spec 0108) for the Enhanced tier — best-effort, like the
            // avatar: a lookup failure just means this peer is heard in a default voice.
            let cartesia_voice_id = svc.get_cartesia_voice_id(uid).await.unwrap_or_default();
            Ok(Some(AuthedPeer {
                user_id: uid,
                avatar_url,
                cartesia_voice_id,
            }))
        }
        Ok(false) => Err(ServerMessage::Error {
            message: "insufficient balance to join".to_string(),
            code: Some("insufficient_balance".to_string()),
        }),
        Err(e) => {
            tracing::error!("can_join check failed: {e}");
            Err(ServerMessage::Error {
                message: "billing service unavailable".to_string(),
                code: Some("billing_unavailable".to_string()),
            })
        }
    }
}

/// Spawn the usage meter for a just-started speaking session, returning its
/// cancel handle. Returns `None` when no metering applies (guest with no cap).
#[allow(clippy::too_many_arguments)] // cohesive per-session metering context
fn spawn_meter(
    state: &AppState,
    billed_user: Option<Uuid>,
    usage_session_id: Option<Uuid>,
    guest_cap_secs: Option<u64>,
    guest_spent: &Option<Arc<AtomicU64>>,
    out_tx: &PeerTx,
    exhaust_tx: &UnboundedSender<()>,
    room: &str,
    speaker_id: &str,
    speaker_lang: &str,
    rate_per_second: f64,
    scale_by_target_count: bool,
) -> Option<oneshot::Sender<()>> {
    let billing_cfg = state.config.billing.as_ref()?;
    let interval = billing_cfg.pricing.usage_update_interval;

    // Billed user: charge credits per interval.
    if let (Some(uid), Some(sid), Some(svc)) =
        (billed_user, usage_session_id, state.billing.as_ref())
    {
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let cfg = MeterConfig {
            interval_secs: interval,
            // Per-engine rate (spec 0093): Premium bills higher than Standard.
            rate_per_second,
            low_balance_threshold: billing_cfg.pricing.low_balance_threshold,
            rooms: Some(state.rooms.clone()),
            room: room.to_string(),
            // Speaker-pays (spec 0093): the live model bills the speaker; the
            // listener-pays cutover (spec 0099) swaps this for MeterScope::Listener.
            scope: MeterScope::Speaker {
                speaker_id: speaker_id.to_string(),
                speaker_lang: speaker_lang.to_string(),
                scale_by_target_count,
            },
        };
        tokio::spawn(run_usage_meter(
            svc.clone(),
            uid,
            sid,
            cfg,
            out_tx.clone(),
            exhaust_tx.clone(),
            cancel_rx,
        ));
        return Some(cancel_tx);
    }

    // Guest with a cumulative time cap.
    if let (Some(cap), Some(spent)) = (guest_cap_secs, guest_spent.as_ref()) {
        let (cancel_tx, cancel_rx) = oneshot::channel();
        tokio::spawn(run_guest_meter(
            spent.clone(),
            cap,
            interval,
            out_tx.clone(),
            exhaust_tx.clone(),
            cancel_rx,
        ));
        return Some(cancel_tx);
    }

    None
}

/// Push each peer the audio CAPTURE FORMAT the room demands (spec 0099, listener-pays):
/// PCM16 iff a listener of a speech-to-speech (`translated_audio`) engine — OpenAI or
/// Gemini — is present (they read raw PCM; Standard takes WebM/Opus, or `linear16` when a
/// premium listener is present). No-op unless listener-pays is active. Capability-driven
/// (NOT a hardcoded id) and the SAME condition the core loop uses for `pcm_input`, so the
/// wire format always agrees server↔client.
fn notify_capture_formats(state: &AppState, room: &str) {
    if !state.config.listener_pays {
        return;
    }
    let pcm_engines: Vec<String> = state
        .engines
        .list()
        .filter(|e| e.metadata().capabilities.translated_audio)
        .map(|e| e.metadata().id.clone())
        .collect();
    for (pid, _engine) in state.rooms.peer_engines(room) {
        let pcm = pcm_engines
            .iter()
            .any(|eid| state.rooms.has_engine_listener(room, &pid, eid));
        state
            .rooms
            .relay_to_peer(room, &pid, &ServerMessage::CaptureFormat { pcm }.to_json());
    }
}

/// Spawn the LISTENER meter (spec 0099): bill `listener_id` for the cross-language
/// sources they receive, at their own receive-engine rate, for the WHOLE connection
/// (they pay for what they RECEIVE, not for speaking). `None` without a DB / for a guest.
/// Mirror of [`spawn_meter`]'s billed branch with a `MeterScope::Listener`.
#[allow(clippy::too_many_arguments)]
fn spawn_listener_meter(
    state: &AppState,
    billed_user: Option<Uuid>,
    usage_session_id: Option<Uuid>,
    out_tx: &PeerTx,
    exhaust_tx: &UnboundedSender<()>,
    room: &str,
    listener_id: &str,
    rate_per_second: f64,
) -> Option<oneshot::Sender<()>> {
    let billing_cfg = state.config.billing.as_ref()?;
    let (uid, sid, svc) = match (billed_user, usage_session_id, state.billing.as_ref()) {
        (Some(uid), Some(sid), Some(svc)) => (uid, sid, svc),
        _ => return None,
    };
    let (cancel_tx, cancel_rx) = oneshot::channel();
    let cfg = MeterConfig {
        interval_secs: billing_cfg.pricing.usage_update_interval,
        rate_per_second,
        low_balance_threshold: billing_cfg.pricing.low_balance_threshold,
        rooms: Some(state.rooms.clone()),
        room: room.to_string(),
        scope: MeterScope::Listener {
            listener_id: listener_id.to_string(),
        },
    };
    tokio::spawn(run_usage_meter(
        svc.clone(),
        uid,
        sid,
        cfg,
        out_tx.clone(),
        exhaust_tx.clone(),
        cancel_rx,
    ));
    Some(cancel_tx)
}

/// A peer's WebSocket: receives audio (binary) + control/signaling/chat (text),
/// and is sent room lifecycle, relayed signaling, subtitles, chat, and peer state.
async fn handle_peer(socket: WebSocket, params: WsParams, state: AppState, client_ip: String) {
    let WsParams {
        room,
        lang,
        name,
        id,
        public,
        token,
        engine: engine_id,
    } = params;
    let id = id
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let name = name
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "Guest".to_string());
    let visibility = if public.unwrap_or(false) {
        Visibility::Public
    } else {
        Visibility::Private
    };
    // Resolve the speaker's chosen translation engine once for the connection
    // (spec 0093): drives the usage-session tag, the per-engine billing rate, and
    // the speaking pipeline. Unknown/absent id → the default engine. Mutable so a
    // mid-call credit squeeze can gracefully downgrade Premium → default.
    let mut active_engine = state.engines.resolve(engine_id.as_deref());

    let (mut ws_tx, mut ws_rx) = socket.split();

    // Global connection cap (spec 0064): the guard balances the live-connection counter
    // on every return path below; reject when over the ceiling.
    let (_conn, conn_count) = ConnGuard::acquire(&state.ws_conns);
    if conn_count > state.max_ws_conns {
        let _ = ws_tx
            .send(Message::Text(
                ServerMessage::Error {
                    message: "server at capacity, please retry shortly".to_string(),
                    code: Some("server_busy".to_string()),
                }
                .to_json()
                .into(),
            ))
            .await;
        let _ = ws_tx.close().await;
        return;
    }

    // Auth / billing gate: resolve the (optional) billed user before joining.
    let authed = match authorize(&state, token.as_deref()).await {
        Ok(u) => u,
        Err(err) => {
            let _ = ws_tx.send(Message::Text(err.to_json().into())).await;
            let _ = ws_tx.close().await;
            return;
        }
    };
    let billed_user = authed.as_ref().map(|a| a.user_id);
    let avatar_url = authed.as_ref().and_then(|a| a.avatar_url.clone());
    // Cloned-voice id (spec 0108), propagated to peers so an Enhanced listener speaks this
    // peer's translated audio in their own voice. `None` for guests / users without a clone.
    let cartesia_voice_id = authed.and_then(|a| a.cartesia_voice_id);

    // Guests always use the default (Standard) engine: Premium/Pro are paid per target
    // language and a guest has no billing account to charge. Pin it server-side so a
    // crafted `?engine=premium` can't open a paid upstream session for an unbilled peer
    // (the client already hides the selector for guests; this is the enforcement).
    if billed_user.is_none() {
        active_engine = state.engines.default();
    }

    // Enhanced (Cartesia, spec 0108) is a client-direct, listener-side RECEIVE tier: the
    // browser translates the audio it hears locally. It only makes sense under
    // listener-pays (each listener picks the quality THEY receive). In the legacy
    // speaker-pays mode there is no listener-side engine, and the server has no session
    // to run for it, so a client-direct pick would translate nothing — fall back to the
    // default so the speaker is still transcribed for the room.
    if !state.config.listener_pays && active_engine.metadata().capabilities.client_direct {
        active_engine = state.engines.default();
    }

    // Accountability: when accounts are live (the DB is connected, so users can
    // actually sign in), public rooms require a signed-in user. Guests can still
    // use private rooms via an invite link. We key off the live pool rather than
    // mere config so the degraded guest-only fallback (billing configured but DB
    // unreachable) doesn't lock everyone out of public rooms.
    if matches!(visibility, Visibility::Public) && billed_user.is_none() && state.pool.is_some() {
        let _ = ws_tx
            .send(Message::Text(
                ServerMessage::Error {
                    message: "sign in to use public rooms".to_string(),
                    code: Some("login_required".to_string()),
                }
                .to_json()
                .into(),
            ))
            .await;
        let _ = ws_tx.close().await;
        return;
    }

    // Outgoing channel: server -> this peer's WS (text frames). Bounded (#123):
    // control/chat/signalling are never dropped — a stalled reader trips
    // `out_overflow` and the receive loop below closes the connection cleanly.
    let (out_tx, out_rx, out_overflow) = PeerTx::channel(OUT_CHANNEL_CAP);
    // Per-connection id (distinct from the reused peer `id`): teardown removes by
    // `(id, conn)` so a same-id reconnect can't be dropped by its predecessor's
    // late teardown (the reconnect black-screen bug).
    let conn = Uuid::new_v4();
    let peer = Peer {
        id: id.clone(),
        conn,
        name: name.clone(),
        lang: lang.clone(),
        user_id: billed_user,
        // The engine whose quality this peer RECEIVES (spec 0099). In speaker-pays it's
        // unused for routing; in listener-pays it drives which engine translates others'
        // speech for this peer. Guests are already pinned to the default above.
        engine: active_engine.metadata().id.clone(),
        avatar_url: avatar_url.clone(),
        cartesia_voice_id: cartesia_voice_id.clone(),
        tx: out_tx.clone(),
        speaking: Arc::new(AtomicBool::new(false)),
    };

    let joined = match state.rooms.join(&room, peer, visibility) {
        Ok(joined) => joined,
        Err(()) => {
            // Room full — tell the peer directly and close.
            let _ = ws_tx
                .send(Message::Text(ServerMessage::RoomFull.to_json().into()))
                .await;
            let _ = ws_tx.close().await;
            return;
        }
    };
    let session_id = joined.session_id;
    let room_public = joined.public;
    let existing = joined.existing;

    // Security (#232): the pre-join gate above keys off the client-supplied
    // `public` query param, which a guest can spoof — sending `public=false` to
    // slip into a PUBLIC room by its code. The room's CANONICAL visibility is set
    // by its creator and a later joiner's param can't change it (rooms.rs), so it
    // is only known here, after `join()`. Re-check against `room_public`: when
    // accounts are live, guests may use private rooms only. Back out of the room
    // we just joined and close. This runs before any RoomJoined/PeerJoined is
    // emitted and before the usage/transcript session is created, so the only
    // cleanup needed is removing the freshly-added peer.
    if room_public && billed_user.is_none() && state.pool.is_some() {
        state.rooms.remove(&room, &id, conn);
        tracing::info!(%room, %name, "guest rejected from public room (canonical visibility)");
        let _ = ws_tx
            .send(Message::Text(
                ServerMessage::Error {
                    message: "sign in to use public rooms".to_string(),
                    code: Some("login_required".to_string()),
                }
                .to_json()
                .into(),
            ))
            .await;
        let _ = ws_tx.close().await;
        return;
    }

    let session_start = std::time::Instant::now(); // for the WS session canonical line (spec 0050)
    tracing::info!(%room, %name, %lang, peers = existing.len() + 1, "peer joined");

    // Tell this user's friends they're now in a PUBLIC room, so they can drop in for a
    // chat (spec: friends). Fire-and-forget + rate-limited; never blocks the join path.
    if room_public {
        if let Some(uid) = billed_user {
            let st = state.clone();
            let nm = name.clone();
            let rm = room.clone();
            tokio::spawn(async move {
                crate::friends::notify_friends_of_public_join(st, uid, nm, rm).await;
            });
        }
    }

    // Transcript persistence: ensure the session row exists (first joiner wins)
    // and record this participant. `participant_row` is kept for `left_at`.
    let participant_row = match state.transcripts.as_ref() {
        Some(svc) => {
            if let Err(e) = svc.session_started(session_id, &room).await {
                tracing::error!("transcript session_started failed: {e}");
            }
            match svc
                .participant_joined(session_id, &id, billed_user, &name, &lang)
                .await
            {
                Ok(pid) => Some(pid),
                Err(e) => {
                    tracing::error!("transcript participant_joined failed: {e}");
                    None
                }
            }
        }
        None => None,
    };

    // Tell the new peer its id + the peers already present (it will connect to them).
    let _ = out_tx.send(
        ServerMessage::RoomJoined {
            peer_id: id.clone(),
            peers: existing,
            // Doubles as the client's "transcript recording on" signal.
            session_id: state.transcripts.as_ref().map(|_| session_id.to_string()),
            public: room_public,
        }
        .to_json(),
    );
    // Whiteboard (spec 0045): replay the current op-log so the joiner sees the
    // drawing already on the board.
    let wb_snapshot = state.rooms.whiteboard_snapshot(&room);
    if !wb_snapshot.is_empty() {
        let _ = out_tx.send(ServerMessage::WhiteboardSnapshot { ops: wb_snapshot }.to_json());
    }
    // Mini-game (spec 0046): hand the joiner the current game, if any.
    if let Some(game) = state.rooms.game_snapshot(&room) {
        let _ = out_tx.send(ServerMessage::GameSnapshot { state: game }.to_json());
    }
    // Room glossary (spec 0011): load it into the cache (the translation hot
    // path reads it synchronously) and tell the joiner when one is active.
    if let Some(g) = state.glossary.as_ref() {
        match g.get(&room).await {
            Ok(gl) if !gl.entries.is_empty() => {
                let _ = out_tx.send(
                    ServerMessage::GlossaryActive {
                        name: gl.name.clone(),
                        entries: gl.entries.len(),
                    }
                    .to_json(),
                );
            }
            Ok(_) => {}
            Err(e) => tracing::error!("glossary load for room {room} failed: {e}"),
        }
    }
    // Tell the others a new peer arrived (they initiate the WebRTC offer).
    state.rooms.broadcast_except(
        &room,
        &id,
        &ServerMessage::PeerJoined {
            peer_id: id.clone(),
            user_name: name.clone(),
            lang: lang.clone(),
            user_id: billed_user,
            avatar_url: avatar_url.clone(),
            cartesia_voice_id: cartesia_voice_id.clone(),
        }
        .to_json(),
    );
    // Listener-pays (spec 0099): this peer's engine may have changed the room's capture
    // demand (e.g. the first Premium listener) — re-push the format to everyone. No-op
    // when listener-pays is off.
    notify_capture_formats(&state, &room);

    let send_task = tokio::spawn(pump_to_ws(out_rx, ws_tx));

    // One usage session per call for billed users (cost accrues while speaking).
    let usage_session_id = match (billed_user, state.billing.as_ref()) {
        (Some(uid), Some(svc)) => match svc
            .create_session(uid, &room, active_engine.metadata().id.as_str())
            .await
        {
            Ok(sid) => Some(sid),
            Err(e) => {
                tracing::error!("create usage session failed: {e}");
                None
            }
        },
        _ => None,
    };

    // Analytics (spec 0097 / #241): append a non-blocking session-start event. The
    // event store reuses call_sessions(id); `plan` is the engine tier. Fire-and-
    // forget — it never blocks or fails the call. (Further event types —
    // translation_used / subtitles_on / voice_generated / screen_shared /
    // recording_started — wire in at their respective points; see the spec.)
    if let Some(pool) = state.pool.as_ref() {
        crate::analytics::record_event(
            pool,
            crate::analytics::UsageEvent::new(
                crate::analytics::plan_for_tier(&active_engine.metadata().tier),
                "session_started",
            )
            .session(session_id)
            .user(billed_user)
            .feature("translation"),
        );
    }

    // Guest speaking-time cap (cumulative across bursts), if configured.
    let guest_cap_secs = if billed_user.is_none() {
        state
            .config
            .billing
            .as_ref()
            .and_then(|b| b.guest_max_minutes)
            .map(|m| m.saturating_mul(60))
    } else {
        None
    };
    // Cumulative per-IP counter (H1): shared across this IP's connections within the
    // rolling window, so reconnecting no longer resets the guest cap.
    let guest_spent = guest_cap_secs.map(|_| state.guest_usage.counter(&client_ip));

    // Active speaking session (Some only while unmuted/talking) — speaker-pays path.
    let mut audio_tx: Option<mpsc::Sender<Vec<u8>>> = None;
    // Listener-pays (spec 0099): the speaker's audio fans to one feed per engine the
    // room's listeners demand (Standard always; each premium engine when wanted).
    // Non-empty only while speaking. Kept separate from `audio_tx` so the speaker-pays
    // path is byte-identical when the flag is off.
    let mut audio_feeds: Vec<mpsc::Sender<Vec<u8>>> = Vec::new();
    // Cancels the running usage/guest meter (on Stop / disconnect).
    let mut meter_cancel: Option<oneshot::Sender<()>> = None;
    // The meter signals here when credits/cap are exhausted -> stop audio.
    let (exhaust_tx, mut exhaust_rx) = mpsc::unbounded_channel::<()>();

    // Listener-pays (spec 0099): a billed user is metered for the WHOLE connection (they
    // pay for what they RECEIVE), so the meter starts at JOIN — speaker activity drives
    // the per-tick scale. Guests are pinned to the default engine and use the speaking-
    // side cap instead. Speaker-pays keeps its per-`Start` meter below.
    if state.config.listener_pays && billed_user.is_some() {
        let rate = active_engine.metadata().user_rate_per_second();
        meter_cancel = spawn_listener_meter(
            &state,
            billed_user,
            usage_session_id,
            &out_tx,
            &exhaust_tx,
            &room,
            &id,
            rate,
        );
    }

    // Per-connection abuse caps (spec 0064): a fixed-window message-rate budget.
    let mut msg_count: u32 = 0;
    let mut msg_window = Instant::now();

    // Keepalive: any inbound frame (incl. the browser's automatic Pong reply to our
    // pings) refreshes this. If nothing arrives for WS_IDLE_TIMEOUT the client is
    // frozen / half-open — break out so the disconnect cleanup evicts the ghost peer.
    let mut last_activity = Instant::now();
    let mut idle_check = tokio::time::interval(WS_PING_INTERVAL);
    idle_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            maybe_msg = ws_rx.next() => {
                let Some(Ok(msg)) = maybe_msg else { break };
                last_activity = Instant::now();
                // Drop oversized frames (binary audio / text) before processing (spec 0064).
                let frame_len = match &msg {
                    Message::Binary(d) => d.len(),
                    Message::Text(t) => t.as_str().len(),
                    _ => 0,
                };
                if frame_len > MAX_FRAME_BYTES {
                    continue;
                }
                // Per-connection message-rate cap → close an abusive socket.
                if msg_window.elapsed() > WS_MSG_WINDOW {
                    msg_window = Instant::now();
                    msg_count = 0;
                }
                msg_count += 1;
                if msg_count > WS_MSG_MAX {
                    tracing::warn!(%id, "ws message-rate flood — closing connection");
                    break;
                }
                match msg {
                    Message::Binary(data) => {
                        if state.config.listener_pays {
                            // Listener-pays: fan the captured chunk to every engine feed.
                            // A feed that errs (saturated/closed) is dropped; the others
                            // keep going. When all are gone the speaking session ended —
                            // stop marking the speaker active.
                            if !audio_feeds.is_empty() {
                                let chunk = data.to_vec();
                                audio_feeds.retain(|tx| tx.try_send(chunk.clone()).is_ok());
                                if audio_feeds.is_empty() {
                                    state.rooms.set_speaking(&room, &id, false);
                                }
                            }
                        } else {
                            // Bounded audio channel (#123): if Deepgram can't keep up
                            // (Full) or the forwarder is gone (Closed), end the session
                            // cleanly rather than drop a mid-stream chunk — dropping one
                            // would corrupt the WebM container. STT resumes on the next
                            // `Start`. The borrow ends before we clear `audio_tx`.
                            let end_session = match audio_tx.as_ref() {
                                Some(tx) => tx.try_send(data.to_vec()).is_err(),
                                None => false,
                            };
                            if end_session {
                                tracing::warn!(%id, "audio channel saturated/closed — ending STT session (#123)");
                                audio_tx = None; // drop sender → forward_audio flushes + closes
                            }
                        }
                    }
                    Message::Text(t) => match serde_json::from_str::<ClientMessage>(t.as_str()) {
                        Ok(ClientMessage::Start) => {
                            if state.config.listener_pays {
                                // Listener-pays (spec 0099): run EVERY engine the room's
                                // listeners demand on this one captured stream, fanning the
                                // audio to each. Each engine self-filters to ITS listeners'
                                // languages (target_langs_for_engine) and bills the LISTENER.
                                if audio_feeds.is_empty() {
                                    let live_lang = state
                                        .rooms
                                        .peer_lang(&room, &id)
                                        .unwrap_or_else(|| lang.clone());
                                    let routes =
                                        state.rooms.translation_routes(&room, &id, &live_lang);
                                    // PCM-input engines = the speech-to-speech tiers.
                                    // Surgical capture: PCM iff such a listener is present
                                    // (room-level; the SAME condition as CaptureFormat).
                                    let pcm_engines: Vec<String> = state
                                        .engines
                                        .list()
                                        .filter(|e| e.metadata().capabilities.translated_audio)
                                        .map(|e| e.metadata().id.clone())
                                        .collect();
                                    let pcm = pcm_engines
                                        .iter()
                                        .any(|eid| state.rooms.has_engine_listener(&room, &id, eid));
                                    let build_ctx = || deepgram::SpeakerCtx {
                                        room: room.clone(),
                                        speaker_id: id.clone(),
                                        speaker_name: name.clone(),
                                        speaker_lang: live_lang.clone(),
                                        session_id,
                                        speaker_user_id: billed_user,
                                        glossary: state.glossary.clone(),
                                    };
                                    let build_deps = |lp: bool| engine::SessionDeps {
                                        rooms: state.rooms.clone(),
                                        moderator: state.moderator.clone(),
                                        transcripts: state.transcripts.clone(),
                                        participant_row,
                                        listener_pays: lp,
                                        pcm_input: pcm,
                                        translator: state.translator.clone(),
                                    };
                                    let mut feeds: Vec<mpsc::Sender<Vec<u8>>> = Vec::new();
                                    let mut any_premium_ok = false;
                                    // Distinct premium engines a CROSS-LANGUAGE listener chose.
                                    let mut wanted: Vec<String> = routes
                                        .iter()
                                        .map(|r| r.engine.clone())
                                        .filter(|eid| pcm_engines.contains(eid))
                                        .collect();
                                    wanted.sort();
                                    wanted.dedup();
                                    for eid in &wanted {
                                        if let Some(eng) = state.engines.get(eid) {
                                            match eng
                                                .start_session(build_ctx(), build_deps(true))
                                                .await
                                            {
                                                engine::SessionOutcome::Started(tx) => {
                                                    feeds.push(tx);
                                                    any_premium_ok = true;
                                                }
                                                engine::SessionOutcome::AtCapacity
                                                | engine::SessionOutcome::Failed => {
                                                    // Capacity fallback (spec 0094): this
                                                    // engine's listeners drop to the Standard
                                                    // stream (served below). Partial-fail
                                                    // billing is a documented dry-run item.
                                                    state.rooms.broadcast(
                                                        &room,
                                                        &ServerMessage::EngineDowngraded {
                                                            peer_id: id.clone(),
                                                            from: eid.clone(),
                                                            to: crate::engine::STANDARD_ID
                                                                .to_string(),
                                                            reason: "premium_at_capacity"
                                                                .to_string(),
                                                        }
                                                        .to_json(),
                                                    );
                                                }
                                            }
                                        }
                                    }
                                    // Standard ALWAYS runs. listener_pays = any_premium_ok →
                                    // serve only Standard listeners; false (no premium running /
                                    // all fell back) → serve EVERYONE.
                                    let std_engine = state
                                        .engines
                                        .get(crate::engine::STANDARD_ID)
                                        .unwrap_or_else(|| state.engines.default());
                                    if let engine::SessionOutcome::Started(tx) = std_engine
                                        .start_session(build_ctx(), build_deps(any_premium_ok))
                                        .await
                                    {
                                        feeds.push(tx);
                                    }
                                    audio_feeds = feeds;
                                    if !audio_feeds.is_empty() {
                                        state.rooms.set_speaking(&room, &id, true);
                                        // Guests aren't billed for receiving; cap their speaking
                                        // time (billed listeners are metered from join).
                                        if billed_user.is_none() && meter_cancel.is_none() {
                                            meter_cancel = spawn_meter(
                                                &state,
                                                billed_user,
                                                usage_session_id,
                                                guest_cap_secs,
                                                &guest_spent,
                                                &out_tx,
                                                &exhaust_tx,
                                                &room,
                                                &id,
                                                &live_lang,
                                                0.0,
                                                false,
                                            );
                                        }
                                    }
                                }
                            } else if audio_tx.is_none() {
                                // Live language: auto-detect / set_lang may have
                                // updated it since join, so never trust `lang`.
                                let live_lang = state
                                    .rooms
                                    .peer_lang(&room, &id)
                                    .unwrap_or_else(|| lang.clone());
                                // Built fresh per engine attempt (the engine consumes
                                // them), so a capacity fallback can retry cleanly.
                                let build_ctx = || deepgram::SpeakerCtx {
                                    room: room.clone(),
                                    speaker_id: id.clone(),
                                    speaker_name: name.clone(),
                                    speaker_lang: live_lang.clone(),
                                    session_id,
                                    speaker_user_id: billed_user,
                                    glossary: state.glossary.clone(),
                                };
                                let build_deps = || engine::SessionDeps {
                                    rooms: state.rooms.clone(),
                                    moderator: state.moderator.clone(),
                                    transcripts: state.transcripts.clone(),
                                    participant_row,
                                    // Speaker-pays path: engines use their legacy
                                    // per-lang fan-out + WebM/Opus capture.
                                    listener_pays: false,
                                    pcm_input: false,
                                    translator: state.translator.clone(),
                                };
                                // Engine routing (spec 0093): the engine owns the
                                // STT/translation pipeline incl. the `auto` detect
                                // flow — no engine-specific branching.
                                let mut outcome =
                                    active_engine.start_session(build_ctx(), build_deps()).await;
                                // Capacity fallback (spec 0094): Premium has no free
                                // upstream session right now → switch this speaker to
                                // the default engine so translation never stops, and
                                // tell the room (the speaker swaps capture, listeners
                                // resume TTS). Billed at the cheaper Standard rate.
                                if matches!(outcome, engine::SessionOutcome::AtCapacity) {
                                    let from = active_engine.metadata().id.clone();
                                    active_engine = state.engines.default();
                                    let to = active_engine.metadata().id.clone();
                                    state.rooms.broadcast(
                                        &room,
                                        &ServerMessage::EngineDowngraded {
                                            peer_id: id.clone(),
                                            from,
                                            to,
                                            reason: "premium_at_capacity".to_string(),
                                        }
                                        .to_json(),
                                    );
                                    outcome = active_engine
                                        .start_session(build_ctx(), build_deps())
                                        .await;
                                }
                                audio_tx = match outcome {
                                    engine::SessionOutcome::Started(tx) => Some(tx),
                                    engine::SessionOutcome::AtCapacity
                                    | engine::SessionOutcome::Failed => None,
                                };
                                if audio_tx.is_some() {
                                    meter_cancel = spawn_meter(
                                        &state,
                                        billed_user,
                                        usage_session_id,
                                        guest_cap_secs,
                                        &guest_spent,
                                        &out_tx,
                                        &exhaust_tx,
                                        &room,
                                        &id,
                                        &live_lang,
                                        active_engine.metadata().user_rate_per_second(),
                                        active_engine.metadata().capabilities.cost_scales_per_language,
                                    );
                                }
                            }
                        }
                        Ok(ClientMessage::Stop) => {
                            if state.config.listener_pays {
                                audio_feeds.clear(); // drop all engine feeds → flush + close
                                state.rooms.set_speaking(&room, &id, false);
                                // Billed listeners keep their connection-long meter (they pay
                                // for receiving, not speaking); only the guest speaking-time
                                // meter is per-Start.
                                if billed_user.is_none() {
                                    if let Some(c) = meter_cancel.take() {
                                        let _ = c.send(());
                                    }
                                }
                            } else {
                                audio_tx = None; // flush + close Deepgram
                                if let Some(c) = meter_cancel.take() {
                                    let _ = c.send(());
                                }
                            }
                        }
                        Ok(ClientMessage::Offer { to, sdp }) => {
                            state.rooms.relay_to_peer(
                                &room,
                                &to,
                                &ServerMessage::Offer { from: id.clone(), sdp }.to_json(),
                            );
                        }
                        Ok(ClientMessage::Answer { to, sdp }) => {
                            state.rooms.relay_to_peer(
                                &room,
                                &to,
                                &ServerMessage::Answer { from: id.clone(), sdp }.to_json(),
                            );
                        }
                        Ok(ClientMessage::Ice { to, candidate }) => {
                            state.rooms.relay_to_peer(
                                &room,
                                &to,
                                &ServerMessage::Ice { from: id.clone(), candidate }.to_json(),
                            );
                        }
                        Ok(ClientMessage::Chat { text }) => {
                            // Cap chat size before any work — an unbounded message would
                            // drive the Groq translation fan-out + DB writes (spec 0028).
                            if text.len() > 8 * 1024 {
                                // A real client never sends this; drop the oversized frame.
                            } else if state.moderator.severity(&text) == Severity::Severe {
                                // Moderate chat too: block + warn the sender on a hit.
                                let _ = out_tx.send(
                                    ServerMessage::ModerationWarning {
                                        message: "Your message was blocked by moderation."
                                            .to_string(),
                                    }
                                    .to_json(),
                                );
                            } else {
                                handle_chat(
                                    &state,
                                    deepgram::SpeakerCtx {
                                        room: room.clone(),
                                        speaker_id: id.clone(),
                                        speaker_name: name.clone(),
                                        // Live language (post-detection); "auto"
                                        // is still possible pre-probe — the
                                        // translator handles it.
                                        speaker_lang: state
                                            .rooms
                                            .peer_lang(&room, &id)
                                            .unwrap_or_else(|| lang.clone()),
                                        session_id,
                                        speaker_user_id: billed_user,
                                        glossary: state.glossary.clone(),
                                    },
                                    &avatar_url,
                                    text,
                                );
                            }
                        }
                        Ok(ClientMessage::MuteAudio { muted }) => {
                            state.rooms.broadcast_except(
                                &room,
                                &id,
                                &ServerMessage::PeerMuted {
                                    peer_id: id.clone(),
                                    kind: "audio".to_string(),
                                    muted,
                                }
                                .to_json(),
                            );
                        }
                        Ok(ClientMessage::MuteVideo { muted }) => {
                            state.rooms.broadcast_except(
                                &room,
                                &id,
                                &ServerMessage::PeerMuted {
                                    peer_id: id.clone(),
                                    kind: "video".to_string(),
                                    muted,
                                }
                                .to_json(),
                            );
                        }
                        Ok(ClientMessage::Emoji { emoji }) => {
                            state.rooms.broadcast(
                                &room,
                                &ServerMessage::EmojiReaction {
                                    peer_id: id.clone(),
                                    peer_name: name.clone(),
                                    emoji,
                                }
                                .to_json(),
                            );
                        }
                        Ok(ClientMessage::HandRaise { raised }) => {
                            state.rooms.broadcast_except(
                                &room,
                                &id,
                                &ServerMessage::HandRaised {
                                    peer_id: id.clone(),
                                    raised,
                                }
                                .to_json(),
                            );
                        }
                        Ok(ClientMessage::ScreenShare { active, audio }) => {
                            state.rooms.broadcast_except(
                                &room,
                                &id,
                                &ServerMessage::ScreenShare {
                                    peer_id: id.clone(),
                                    active,
                                    audio,
                                }
                                .to_json(),
                            );
                            // Analytics (#241): one screen_shared event per share start.
                            if active {
                                if let Some(pool) = state.pool.as_ref() {
                                    crate::analytics::record_event(
                                        pool,
                                        crate::analytics::UsageEvent::new(
                                            crate::analytics::plan_for_tier(
                                                &active_engine.metadata().tier,
                                            ),
                                            "screen_shared",
                                        )
                                        .session(session_id)
                                        .user(billed_user)
                                        .feature("screen_share"),
                                    );
                                }
                            }
                        }
                        Ok(ClientMessage::Whiteboard { op }) => {
                            // Persist for late-joiners, then relay to the others (spec 0045).
                            state.rooms.whiteboard_apply(&room, op.clone());
                            state.rooms.broadcast_except(
                                &room,
                                &id,
                                &ServerMessage::Whiteboard {
                                    peer_id: id.clone(),
                                    op,
                                }
                                .to_json(),
                            );
                        }
                        Ok(ClientMessage::Game { state: game_state }) => {
                            // Game-agnostic relay + snapshot for late-joiners (spec 0046).
                            state.rooms.game_set(&room, game_state.clone());
                            state.rooms.broadcast_except(
                                &room,
                                &id,
                                &ServerMessage::Game {
                                    peer_id: id.clone(),
                                    state: game_state,
                                }
                                .to_json(),
                            );
                        }
                        Ok(ClientMessage::SetLang { lang: new_lang }) => {
                            // Manual correction (spec 0012). The audio session is
                            // NOT restarted here: a mid-stream Deepgram reopen
                            // needs a fresh WebM header, so the client sends
                            // stop + start around this message.
                            let new_lang = new_lang.trim().to_lowercase();
                            let valid = !new_lang.is_empty()
                                && new_lang.len() <= 8
                                && new_lang != "auto"
                                && new_lang
                                    .chars()
                                    .all(|c| c.is_ascii_alphanumeric() || c == '-');
                            if !valid {
                                let _ = out_tx.send(
                                    ServerMessage::Error {
                                        message: "invalid language code".to_string(),
                                        code: Some("bad_lang".to_string()),
                                    }
                                    .to_json(),
                                );
                            } else if state.rooms.set_peer_lang(&room, &id, &new_lang) {
                                if let (Some(pid), Some(svc)) =
                                    (participant_row, state.transcripts.as_ref())
                                {
                                    if let Err(e) =
                                        svc.update_participant_lang(pid, &new_lang).await
                                    {
                                        tracing::error!("participant lang update failed: {e}");
                                    }
                                }
                                // `confidence: None` marks a manual correction.
                                state.rooms.broadcast(
                                    &room,
                                    &ServerMessage::LanguageDetected {
                                        peer_id: id.clone(),
                                        lang: new_lang,
                                        confidence: None,
                                    }
                                    .to_json(),
                                );
                            }
                        }
                        Ok(ClientMessage::EnhancedFallback { speaker_id }) => {
                            // The browser-direct Enhanced (Cartesia) pipeline gave up
                            // translating in-browser (spec 0108) — e.g. Cartesia's concurrency
                            // limit (429). Fall this listener back to server-side Standard for
                            // the rest of the call, mirroring the low-balance downgrade above.
                            // Idempotent: act only while still on a client-direct engine.
                            let cur = state.rooms.peer_engine(&room, &id);
                            let on_client_direct = cur
                                .as_deref()
                                .and_then(|eid| state.engines.get(eid))
                                .map(|e| e.metadata().capabilities.client_direct)
                                .unwrap_or(false);
                            if on_client_direct {
                                let from =
                                    cur.unwrap_or_else(|| crate::engine::CARTESIA_ID.to_string());
                                let default = state.engines.default();
                                let to = default.metadata().id.clone();
                                state.rooms.set_peer_engine(&room, &id, &to);
                                tracing::info!(
                                    %id, %speaker_id, %from, %to,
                                    "enhanced fallback → standard"
                                );
                                // Re-bill at the Standard rate from here on: reassigning drops
                                // the Enhanced-rate meter handle (cancelling it) and starts a
                                // Standard-rate meter for the same usage session.
                                if state.config.listener_pays && billed_user.is_some() {
                                    meter_cancel = spawn_listener_meter(
                                        &state,
                                        billed_user,
                                        usage_session_id,
                                        &out_tx,
                                        &exhaust_tx,
                                        &room,
                                        &id,
                                        default.metadata().user_rate_per_second(),
                                    );
                                }
                                let _ = out_tx.send(
                                    ServerMessage::EngineDowngraded {
                                        peer_id: id.clone(),
                                        from,
                                        to,
                                        reason: "enhanced_unavailable".to_string(),
                                    }
                                    .to_json(),
                                );
                                // This listener consumed no server audio (client-direct); the
                                // room's PCM-capture decision may shift — re-push capture formats.
                                notify_capture_formats(&state, &room);
                            }
                        }
                        Ok(ClientMessage::TranslateText {
                            request_id,
                            text,
                            source,
                            target,
                        }) => {
                            // Enhanced translate hop (spec 0108): Cartesia does STT + TTS but
                            // not translation, so the client-direct listener sends each
                            // finalized source segment here and we reply with the Groq
                            // translation. Text only — no audio. Spawn so the per-utterance
                            // Groq round-trip never blocks this peer's WS receive loop, and
                            // route the reply back over the same `out_tx` channel. The
                            // DragonflyDB cache stays Standard-only (spec 0107), so call the
                            // raw Groq client here rather than the cached `translator`.
                            let groq = state.groq.clone();
                            let reply_tx = out_tx.clone();
                            tokio::spawn(async move {
                                let translated = groq
                                    .translate(&text, &source, &target, &[])
                                    .await
                                    .unwrap_or(text);
                                let _ = reply_tx.send(
                                    ServerMessage::TranslatedText {
                                        request_id,
                                        text: translated,
                                    }
                                    .to_json(),
                                );
                            });
                        }
                        Err(_) => {} // unknown / malformed control frame
                    },
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            _ = exhaust_rx.recv() => {
                meter_cancel = None; // stop billing in both models
                if state.config.listener_pays {
                    // Listener-pays (spec 0099): THIS listener ran out of credit for what
                    // they RECEIVE. Drop them to the (free) default engine so translation
                    // continues, and notify just them. We do NOT stop their speaking —
                    // others pay to hear them. A listener already on the default tier has
                    // no cheaper one (a hard cap is a documented dry-run item).
                    let on_premium = state
                        .rooms
                        .peer_engine(&room, &id)
                        .and_then(|eid| state.engines.get(&eid))
                        .map(|e| e.metadata().capabilities.translated_audio)
                        .unwrap_or(false);
                    if on_premium {
                        let to = state.engines.default().metadata().id.clone();
                        let from = state
                            .rooms
                            .peer_engine(&room, &id)
                            .unwrap_or_else(|| crate::engine::STANDARD_ID.to_string());
                        state.rooms.set_peer_engine(&room, &id, &to);
                        let _ = out_tx.send(
                            ServerMessage::EngineDowngraded {
                                peer_id: id.clone(),
                                from,
                                to,
                                reason: "insufficient_balance".to_string(),
                            }
                            .to_json(),
                        );
                        // This listener may have been the room's last premium consumer ⇒
                        // speakers can drop back to Opus — re-push capture formats.
                        notify_capture_formats(&state, &room);
                    }
                } else {
                    // Speaker-pays (spec 0093): stop the speaking session and gracefully
                    // downgrade a paid-tier speaker (Pro/OpenAI or Premium/Gemini) to the
                    // cheaper default engine rather than going silent. The client re-Starts
                    // under the new engine, opening a Standard session + meter. Gated on
                    // "is a non-default engine" so it covers EVERY paid engine, not one id.
                    audio_tx = None;
                    let default_id = state.engines.default().metadata().id.clone();
                    if default_id != active_engine.metadata().id {
                        let from = active_engine.metadata().id.clone();
                        active_engine = state.engines.default();
                        state.rooms.broadcast(
                            &room,
                            &ServerMessage::EngineDowngraded {
                                peer_id: id.clone(),
                                from,
                                to: default_id,
                                reason: "low_balance".to_string(),
                            }
                            .to_json(),
                        );
                    }
                }
            }
            _ = out_overflow.notified() => {
                // Outbound channel saturated (#123): this reader has stopped
                // draining, so its frames have backed up to OUT_CHANNEL_CAP. We
                // never drop control frames — instead we close the connection and
                // let the normal teardown below run (PeerLeft, session finalize).
                tracing::warn!(%id, "outbound channel saturated (slow consumer) — closing connection (#123)");
                break;
            }
            _ = idle_check.tick() => {
                // No frame (not even an automatic Pong) for the whole timeout ⇒ the
                // client is frozen / its socket is half-open. Close it so the teardown
                // below removes the peer for everyone (no lingering ghost tile / room).
                if last_activity.elapsed() > WS_IDLE_TIMEOUT {
                    tracing::info!(%id, "ws idle timeout — closing frozen/half-open connection");
                    break;
                }
            }
        }
    }

    tracing::info!(
        target: "canonical",
        kind = "ws",
        room = %room,
        peer = %id,
        name = %name,
        duration_secs = session_start.elapsed().as_secs(),
        "ws session ended"
    );
    // Analytics (#241): one session_ended event carrying the connection's duration —
    // the core "minutes per plan" KPI. Non-blocking.
    if let Some(pool) = state.pool.as_ref() {
        let secs = session_start.elapsed().as_secs().min(i32::MAX as u64) as i32;
        let mut ev = crate::analytics::UsageEvent::new(
            crate::analytics::plan_for_tier(&active_engine.metadata().tier),
            "session_ended",
        )
        .session(session_id)
        .user(billed_user)
        .feature("session");
        ev.duration_seconds = secs;
        crate::analytics::record_event(pool, ev);
    }
    drop(audio_tx); // flush any active speaking session
    if let Some(c) = meter_cancel.take() {
        let _ = c.send(());
    }
    if let (Some(sid), Some(svc)) = (usage_session_id, state.billing.as_ref()) {
        let _ = svc.finalize_session(sid).await;
    }
    if let (Some(pid), Some(svc)) = (participant_row, state.transcripts.as_ref()) {
        if let Err(e) = svc.participant_left(pid).await {
            tracing::error!("transcript participant_left failed: {e}");
        }
    }
    // Remove THIS connection. If it was already superseded by a same-id reconnect,
    // the peer is still in the room under a newer socket — stay silent so we don't
    // tear the live peer out from under itself or fire a spurious PeerLeft.
    match state.rooms.remove(&room, &id, conn) {
        LeaveOutcome::Left(ended) => {
            // The last leaver's removal ends the call session: flush + finalize it.
            if let Some(sid) = ended {
                if let Some(svc) = state.transcripts.as_ref() {
                    if let Err(e) = svc.finalize_session(sid).await {
                        tracing::error!("finalize transcript session {sid} failed: {e}");
                    }
                }
            }
            state
                .rooms
                .broadcast(&room, &ServerMessage::PeerLeft { peer_id: id }.to_json());
            // Listener-pays (spec 0099): a departing premium listener can flip the room
            // back to Opus capture — re-push formats to the survivors. No-op when off.
            notify_capture_formats(&state, &room);
        }
        LeaveOutcome::Superseded => {
            tracing::debug!(%room, %id, "stale connection superseded by reconnect; no PeerLeft");
        }
    }
    send_task.abort();
}

/// Translate a chat message into every language in the room (parallel), persist
/// it to the transcript, and broadcast it to everyone, including the sender.
/// (Moderation-blocked messages never reach here, so they are never persisted.)
fn handle_chat(
    state: &AppState,
    ctx: deepgram::SpeakerCtx,
    avatar_url: &Option<String>,
    text: String,
) {
    let rooms = state.rooms.clone();
    let translator = state.translator.clone();
    let transcripts = state.transcripts.clone();
    let sender_avatar = avatar_url.clone();
    let timestamp = now_unix();
    let ts = chrono::Utc::now(); // capture send time before the translation await
    tokio::spawn(async move {
        let targets = rooms.get_room_languages(&ctx.room, &ctx.speaker_id);
        // Chat honors the room glossary too (same cache snapshot as speech).
        let glo = ctx.glossary.as_ref().and_then(|g| g.cached(&ctx.room));
        let translations = translator
            .translate_fanout(&text, &ctx.speaker_lang, &targets, glo.as_deref())
            .await;
        if let Some(svc) = transcripts.as_ref() {
            svc.record(TranscriptEvent {
                session_id: ctx.session_id,
                kind: EventKind::Chat,
                speaker_peer_id: ctx.speaker_id.clone(),
                speaker_user_id: ctx.speaker_user_id,
                speaker_name: ctx.speaker_name.clone(),
                original_text: text.clone(),
                original_lang: ctx.speaker_lang.clone(),
                translations: translations.clone(),
                ts,
            });
        }
        rooms.broadcast(
            &ctx.room,
            &ServerMessage::ChatMessage {
                sender_id: ctx.speaker_id,
                sender_name: ctx.speaker_name,
                sender_lang: ctx.speaker_lang,
                sender_avatar,
                original: text,
                translations,
                timestamp,
                attachment: None,
            }
            .to_json(),
        );
    });
}

/// Forward queued JSON strings to a WebSocket as text frames until the channel
/// closes or the socket errors.
async fn pump_to_ws(mut rx: Receiver<String>, mut ws_tx: SplitSink<WebSocket, Message>) {
    // Keepalive: Ping on a fixed cadence between queued messages. Healthy browsers
    // auto-reply with a Pong (seen by the reader as activity); a frozen client
    // stops responding and the reader's idle timeout closes it. The first interval
    // tick fires immediately, so consume it to avoid an extra ping at t=0.
    let mut ping = tokio::time::interval(WS_PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ping.tick().await;
    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Some(m) => {
                        if ws_tx.send(Message::Text(m.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break, // out channel closed → connection going away
                }
            }
            _ = ping.tick() => {
                if ws_tx.send(Message::Ping(Vec::<u8>::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// Resolve on Ctrl-C or SIGTERM for graceful shutdown.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}

#[cfg(test)]
mod tests {
    use super::{origin_header_ok, version_handler};
    use axum::http::HeaderMap;

    #[tokio::test]
    async fn version_handler_reports_the_crate_version() {
        let axum::Json(body) = version_handler().await;
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        // `commit` is always a string (a SHA, or "unknown" off Railway/CI).
        assert!(body["commit"].is_string());
    }

    fn with_header(name: &str, value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
        h
    }

    #[test]
    fn origin_lock_matches_only_the_exact_secret() {
        let secret = "s3cr3t-from-cloudflare";
        assert!(origin_header_ok(
            &with_header("x-origin-verify", secret),
            secret
        ));
        // Wrong value, or the header missing entirely, is rejected.
        assert!(!origin_header_ok(
            &with_header("x-origin-verify", "nope"),
            secret
        ));
        assert!(!origin_header_ok(&HeaderMap::new(), secret));
    }
}
