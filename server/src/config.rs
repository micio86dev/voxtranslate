//! Environment configuration, loaded from `.env` via dotenvy.
//!
//! Auth/billing is **optional**: it activates only when `DATABASE_URL`,
//! `GOOGLE_CLIENT_ID` and `JWT_SECRET` are all set. Otherwise the server runs in
//! guest-only mode (no accounts, no metering) — the original behavior.

use std::collections::HashMap;
use std::env;

use serde::{Deserialize, Serialize};

/// Default cumulative guest (unauthenticated) STT cap, in minutes, when
/// `GUEST_MAX_MINUTES` is unset. Chosen so anonymous abuse can't run up unbounded
/// paid Deepgram/Groq spend while still allowing a genuine guest trial (H1).
const DEFAULT_GUEST_MAX_MINUTES: u64 = 10;

/// Runtime configuration for the server.
// NOTE: `Debug` is hand-written below (redacting secrets) rather than derived, so
// an accidental `tracing::debug!("{config:?}")`/panic can never dump keys/secrets.
#[derive(Clone)]
pub struct Config {
    pub deepgram_key: String,
    pub groq_key: String,
    /// Real-time translation model (Groq), env-driven via `GROQ_TRANSLATION_MODEL`.
    /// Core pipeline setting that must work in guest mode too, so it lives here
    /// rather than under the optional billing `AiConfig`. Latency-critical — keep
    /// it a fast/cheap model.
    pub translation_model: String,
    pub port: u16,
    /// Allowed CORS origins; empty means permissive (dev).
    pub allowed_origins: Vec<String>,
    /// `chrome-extension://<id>` origins allowed to call the API (`EXTENSION_ORIGINS`,
    /// comma-separated). An extension id is only known after packaging, so it cannot be
    /// derived like `app_base_url` — and allowing every `chrome-extension://` origin by
    /// pattern would let any installed extension reach the API from a browser.
    pub extension_origins: Vec<String>,
    /// How much speech (ms) to buffer before the language-detect REST probe
    /// when a peer joins with `lang=auto` (spec 0012).
    pub auto_detect_buffer_ms: u64,
    /// Present only when auth/billing is configured.
    pub billing: Option<BillingConfig>,
    /// Present only when all `RESEND_*` vars are set; gates follow-up email.
    pub resend: Option<ResendConfig>,
    /// Present only when `SUPABASE_URL` + `SUPABASE_SERVICE_KEY` are set; gates
    /// chat file upload (spec 0018). When absent the attach button is hidden and
    /// the upload endpoint returns 503.
    pub storage: Option<StorageConfig>,
    /// Present when `TURN_URLS` + a credential (`TURN_SECRET`, or `TURN_USERNAME` +
    /// `TURN_PASSWORD`) are set; gates the TURN relay returned by `/api/ice`
    /// (spec 0026 / 0059). Without it the client uses STUN only and cross-NAT
    /// (e.g. cross-border) calls may fail to connect.
    pub turn: Option<TurnConfig>,
    /// Restricted-network (Great Firewall) TURN profile, returned by
    /// `/api/ice?restricted=1` to clients that detect a GFW-restricted network. It
    /// is a `turns://…:443` TLS-on-443 relay (managed Asia PoP or coturn) that looks
    /// like ordinary HTTPS, so it survives the DPI that resets plain TURN/UDP and
    /// Cloudflare's 3478/5349 endpoints inside mainland China. Parsed from `TURN_TLS_*`,
    /// SEPARATELY from `turn` (whose Cloudflare mode has no :443 endpoint). `None` when
    /// unconfigured — restricted clients then fall back to the default `turn` relay.
    pub turn_restricted: Option<TurnConfig>,
    /// Recipient for user bug reports (spec 0071). Defaults to the owner's address;
    /// override via `BUG_REPORT_TO`. Email is only sent when `resend` is also set.
    pub bug_report_to: String,
    /// Canonical public origin of the web app (spec 0082). Used to build call
    /// invite links and the logo URL inside transactional emails — emails must
    /// point at our own domain, never a client-supplied host. Override via
    /// `APP_BASE_URL`; trailing slash is trimmed.
    pub app_base_url: String,
    /// Base URL of the Business dashboard — a SEPARATE origin from the call app
    /// (`app_base_url`). Used to build org invite/join links, whose page lives in
    /// the dashboard, not the consumer app. Override via `DASHBOARD_BASE_URL`.
    pub dashboard_base_url: String,
    /// Web-push (VAPID) config for notifications. `Some` only when the VAPID keys are
    /// set; otherwise push delivery is skipped (email + in-app still work).
    pub push: Option<PushConfig>,
    /// Max members for a `business`-plan org (Enterprise is unlimited). Enforced
    /// when an invite is accepted. Override via `ORG_BUSINESS_MEMBER_LIMIT`.
    pub business_member_limit: i64,
    /// Enterprise data-retention sweep (spec 0106). OFF by default so the feature
    /// ships dormant — enabling it (`RETENTION_SWEEP_ENABLED` truthy) starts a
    /// background task that deletes recordings + transcripts older than each
    /// Enterprise org's configured `retention_days`. Also needs `recordings`
    /// storage configured to do anything.
    pub retention_sweep_enabled: bool,
    /// How often the retention sweep runs, in seconds (default 6h). Override via
    /// `RETENTION_SWEEP_INTERVAL_SECS`.
    pub retention_sweep_interval_secs: u64,
    /// Max sessions purged per sweep pass — a safety bound so a first run over a
    /// large backlog can't issue an unbounded burst of deletes. Override via
    /// `RETENTION_SWEEP_BATCH`.
    pub retention_sweep_batch: i64,
    /// OpenAI GPT-Realtime-Translate "Pro" engine (spec 0093). Present only when the
    /// `OPENAI_PRO` rollout flag is truthy AND `OPENAI_API_KEY` is set — registered (and
    /// shown in the selector) iff this is `Some`, so the tier ships dark behind the flag.
    pub openai: Option<OpenAiConfig>,
    /// Google Gemini 3.5 Live Translate "Premium" engine (spec 0100). Present only when
    /// the `GEMINI_PREMIUM` rollout flag is truthy AND `GOOGLE_AI_API_KEY` is set —
    /// registered iff this is `Some`, so it ships dark behind the flag.
    pub google: Option<GeminiConfig>,
    /// Cartesia "Enhanced" engine (spec 0108) — the client-direct tier between Standard
    /// and Pro (browser ↔ Cartesia STT Ink-2 + TTS Sonic-3.5). Present only when the
    /// rollout flag `CARTESIA_ENHANCED` is truthy AND `CARTESIA_API_KEY` is set; the
    /// engine is registered (and shown in the selector) iff this is `Some`, so the tier
    /// ships dark behind the flag (rollback = unset it).
    pub cartesia: Option<CartesiaConfig>,
    /// Qwen-Omni Realtime credentials for the **Standard** base tier. Unlike the
    /// optional tiers this is NOT a dark launch: Standard is the registry's default and
    /// capacity-fallback engine, so `Config::from_env` requires `DASHSCOPE_API_KEY` and
    /// this is always `Some` on a booted server.
    pub qwen: QwenConfig,
    /// Whether the Standard (Qwen-Omni Realtime) base tier is enabled (rollout flag
    /// `QWEN_STANDARD`, default ON). Standard is the registry's default and
    /// capacity-fallback engine, so it is force-registered even when this is `false`
    /// (with a warning) — the flag exists for symmetry with the optional tiers, not as a
    /// true kill switch.
    pub standard_enabled: bool,
    /// Pro tier (OpenAI) kill switch, `PRO_TIER_ENABLED`. **OFF by default**, even when
    /// `OPENAI_API_KEY` is set: at its current price the tier does not earn its cost, so
    /// it is withdrawn rather than deleted — if the model improves, set this and it comes
    /// back with no code change.
    ///
    /// It gates REGISTRATION, not just the listing. An unregistered engine is absent from
    /// `GET /api/engines` (so every client — web, extension, dashboard — hides it) and
    /// `EngineRegistry::resolve` falls back to the default for its id, so a stale saved
    /// preference or a hand-crafted request cannot start it either.
    pub pro_enabled: bool,
    /// Listener-pays rollout flag (spec 0099). OFF by default: the live model is
    /// speaker-pays (spec 0093). When `LISTENER_PAYS` is truthy, each participant
    /// receives — and is billed for — the engine quality THEY chose, and the core
    /// WS loop runs every engine the room's listeners demand. Gated so the
    /// re-architecture ships dark until the billing dry-run signs it off.
    pub listener_pays: bool,
    /// Language-first picker rollout flag (spec 0102). OFF by default: the live picker is
    /// the legacy side-by-side language + engine selector. When `LANGUAGE_FIRST_UX` is
    /// truthy, the client flips to "pick target language → see only the tiers that output
    /// it" over the full language union. Exposed to the client via `GET /api/engines`
    /// (guest-safe). Rollback = unset it.
    pub language_first_ux: bool,
    /// Translation cache master switch (spec 0107), `TRANSLATION_CACHE_ENABLED`. OFF by
    /// default: when false the Standard-tier translator carries no cache and every
    /// translation takes the byte-for-byte pre-cache Groq path. Even when ON, the cache
    /// only activates if `dragonfly_url` is set AND the connection succeeds (fail-open).
    pub cache_enabled: bool,
    /// Phrases longer than this many words bypass the cache entirely — no read, no
    /// write — regardless of the flag (long phrases rarely recur, spec 0107 R5).
    /// `TRANSLATION_CACHE_MAX_WORDS`, default 8.
    pub cache_max_words: usize,
    /// TTL applied to every cache write, in seconds. `TRANSLATION_CACHE_TTL_SECONDS`,
    /// default 604800 (7 days).
    pub cache_ttl_secs: u64,
    /// DragonflyDB connection URL, Railway-provided `DRAGONFLY_PRIVATE_URL`. `None` ⇒ no
    /// connection is attempted and the cache stays off even when `cache_enabled` is true
    /// (logged as a warn at startup, spec 0107 R2).
    pub dragonfly_url: Option<String>,
    /// Bearer secret guarding `POST /internal/bench/translate` (spec 0107 R9). `None` ⇒
    /// the endpoint returns 404. Server-only, never logged.
    pub bench_secret: Option<String>,
    /// OpenAI embeddings for semantic transcript search (Business dashboard). Present
    /// whenever `OPENAI_API_KEY` is set — DECOUPLED from the `OPENAI_PRO` realtime flag,
    /// so search works even when the Pro tier ships dark. `None` ⇒ transcripts are not
    /// embedded and `GET …/search` returns 503.
    pub embeddings: Option<EmbeddingsConfig>,
    /// Bearer secret guarding `POST /internal/embeddings/backfill` (one-time embed of
    /// pre-existing transcripts). `None` ⇒ the endpoint returns 404. Server-only.
    pub embeddings_backfill_secret: Option<String>,
    /// B2B Voice Assistant — GPT-Realtime relay for Business/Enterprise orgs (spec voice-
    /// assistant). Present only when `VOICE_ASSISTANT_ENABLED` is truthy AND
    /// `OPENAI_API_KEY` is set. `None` ⇒ the relay route is not registered and the
    /// dashboard hides the voice button. Ships dark until both env vars are set.
    pub voice_assistant: Option<VoiceAssistantConfig>,
    /// Dashboard Help Assistant — global GPT-Realtime relay with static product-knowledge
    /// prompt, available to all active B2B members (no role restriction). Present only
    /// when `HELP_ASSISTANT_ENABLED` is truthy AND `OPENAI_API_KEY` is set. `None` ⇒
    /// the relay route is not registered and the floating help button is hidden. Ships
    /// dark until both env vars are set. Decoupled from voice_assistant so pricing and
    /// the session cap can be tuned independently.
    pub help_assistant: Option<HelpAssistantConfig>,
    /// Webinar Mode (SPEC "Webinar Mode") — 1-to-many broadcast control plane.
    /// Present only when `MEDIA_INGEST_HOST` is set; `None` ⇒ the `/api/webinars`
    /// and `/api/w/{code}` routes are not registered (feature ships dark).
    pub webinar: Option<WebinarConfig>,
}

/// Webinar Mode config: the off-box media server hosts, join-code length, and the
/// HMAC/caller secrets for the WHIP publish auth (F1-1/F1-2). Secrets are
/// server-only and redacted from `Debug`.
#[derive(Clone)]
pub struct WebinarConfig {
    /// WHIP ingest host, e.g. `ingest.voxtranslate.app` (`MEDIA_INGEST_HOST`).
    pub ingest_host: String,
    /// LL-HLS playback host (`MEDIA_HLS_HOST`); defaults to `ingest_host` in Phase 1
    /// (HLS served from the origin) until the R2/CDN host is split out.
    pub hls_host: String,
    /// Join-code length (`WEBINAR_CODE_LEN`, default [`crate::webinar::DEFAULT_CODE_LEN`]).
    pub code_len: usize,
    /// HMAC-SHA256 key that signs WHIP publish tokens (`MEDIAMTX_AUTH_SECRET`).
    /// Server-only; never serialized or logged.
    pub auth_secret: Vec<u8>,
    /// Shared secret carried in the `/internal/media-auth/{secret}` path, which
    /// authenticates MediaMTX as the caller (`MEDIA_CALLER_SECRET`). Server-only.
    pub caller_secret: String,
    /// Publish-token TTL, seconds (`WEBINAR_PUBLISH_TOKEN_TTL_SECS`, default 120).
    /// Short by design — the token is only needed at WHIP connect; the live WebRTC
    /// session outlives it, so a short TTL shrinks the replay window.
    pub publish_token_ttl_secs: u64,
}

impl WebinarConfig {
    fn from_env() -> Self {
        let ingest_host = env::var("MEDIA_INGEST_HOST")
            .unwrap_or_default()
            .trim()
            .to_string();
        let hls_host = env::var("MEDIA_HLS_HOST")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| ingest_host.clone());
        Self {
            ingest_host,
            hls_host,
            code_len: parse_or("WEBINAR_CODE_LEN", crate::webinar::DEFAULT_CODE_LEN),
            auth_secret: env::var("MEDIAMTX_AUTH_SECRET")
                .unwrap_or_default()
                .into_bytes(),
            caller_secret: env::var("MEDIA_CALLER_SECRET")
                .unwrap_or_default()
                .trim()
                .to_string(),
            publish_token_ttl_secs: parse_or("WEBINAR_PUBLISH_TOKEN_TTL_SECS", 120u64),
        }
    }

    /// Test-only config (doc-hidden) so the integration test can enable the
    /// webinar routes without provisioning a real media server.
    #[doc(hidden)]
    pub fn test_default() -> Self {
        Self {
            ingest_host: "ingest.test".into(),
            hls_host: "hls.test".into(),
            code_len: crate::webinar::DEFAULT_CODE_LEN,
            auth_secret: b"test-webinar-auth-secret".to_vec(),
            caller_secret: "test-caller-secret".into(),
            publish_token_ttl_secs: 120,
        }
    }
}

impl std::fmt::Debug for WebinarConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // auth_secret + caller_secret are server-only secrets — redacted.
        f.debug_struct("WebinarConfig")
            .field("ingest_host", &self.ingest_host)
            .field("hls_host", &self.hls_host)
            .field("code_len", &self.code_len)
            .field("publish_token_ttl_secs", &self.publish_token_ttl_secs)
            .finish_non_exhaustive()
    }
}

/// OpenAI Realtime Translation credentials + pricing (spec 0093). All-or-nothing
/// like billing/Resend: activates only when `OPENAI_API_KEY` is present.
#[derive(Clone)]
pub struct OpenAiConfig {
    /// Server-only API key (Bearer auth to the realtime WS); never sent to clients.
    pub api_key: String,
    /// Realtime translation model (`OPENAI_REALTIME_MODEL`).
    pub model: String,
    /// Raw server cost per minute, USD (`OPENAI_COST_PER_MINUTE`). Used for the
    /// engine's user rate (`cost × (1 + markup)`); the raw value is never serialized.
    pub cost_per_minute: f64,
    /// Markup as a FRACTION (0.5 = 50%). From `OPENAI_COST_MARKUP_PERCENT`, falling
    /// back to `ENGINE_DEFAULT_MARKUP_PERCENT`, divided by 100.
    pub markup: f64,
    /// Hard cap on concurrent OpenAI realtime sessions across the process
    /// (`OPENAI_REALTIME_MAX_SESSIONS`) — backpressure for group rooms (spec 0093).
    pub max_sessions: usize,
    /// Optional fixed realtime voice (`OPENAI_VOICE`, e.g. `marin`, `cedar`, `alloy`).
    /// `None` (default) leaves the model's default. Set it to pin one consistent
    /// timbre for all translated audio. Opt-in so the default behaviour is unchanged.
    pub voice: Option<String>,
}

/// OpenAI embeddings config for semantic transcript search. Reuses the same
/// `OPENAI_API_KEY` as the Pro engine but activates independently of the
/// `OPENAI_PRO` flag (search is a separate, always-eligible feature).
#[derive(Clone)]
pub struct EmbeddingsConfig {
    /// Server-only OpenAI API key (Bearer auth to the embeddings REST endpoint);
    /// never sent to clients and never logged.
    pub api_key: String,
    /// Embedding model id (`EMBEDDINGS_MODEL`, default `text-embedding-3-small`).
    /// The model's output dimension MUST match the `vector(1536)` column in
    /// migration 030 — changing to a different-dimension model needs a new migration.
    pub model: String,
}

/// B2B Voice Assistant config (OpenAI Realtime relay for Business/Enterprise orgs).
/// Decoupled from `OpenAiConfig` (the translation Pro tier) — different model,
/// pricing/markup (fixed 25%), and session cap. Activates only when
/// `VOICE_ASSISTANT_ENABLED` is truthy AND `OPENAI_API_KEY` is set.
#[derive(Clone)]
pub struct VoiceAssistantConfig {
    /// Server-only OpenAI API key (same var as the Pro engine: `OPENAI_API_KEY`).
    /// Never sent to clients. Shared with `EmbeddingsConfig` but the feature is gated
    /// independently on `VOICE_ASSISTANT_ENABLED`.
    pub api_key: String,
    /// GPT-Realtime model id (`VOICE_ASSISTANT_MODEL`, default `gpt-realtime-2`).
    pub model: String,
    /// Raw server cost per minute, USD (`VOICE_ASSISTANT_COST_PER_MINUTE`, default 0.30).
    /// Never serialized; used only for credit-deduction math.
    pub cost_per_minute: f64,
    /// Markup as a FRACTION (e.g. 0.25 = 25%). Configured via
    /// `VOICE_ASSISTANT_MARKUP_PERCENT` (a percentage, e.g. 25), divided by 100.
    /// Default 25.0 → 0.25.
    pub markup: f64,
    /// Hard cap on concurrent voice-assistant sessions across the process
    /// (`VOICE_ASSISTANT_MAX_SESSIONS`, default 10). Semaphore-backed; new
    /// connections beyond this cap receive a `capacity_full` error and are closed.
    pub max_sessions: usize,
}

impl VoiceAssistantConfig {
    fn from_env() -> Self {
        let percent = env::var("VOICE_ASSISTANT_MARKUP_PERCENT")
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .unwrap_or(25.0);
        Self {
            api_key: env::var("OPENAI_API_KEY")
                .unwrap_or_default()
                .trim()
                .to_string(),
            model: env::var("VOICE_ASSISTANT_MODEL")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "gpt-realtime-2".to_string()),
            cost_per_minute: parse_or("VOICE_ASSISTANT_COST_PER_MINUTE", 0.30f64),
            markup: percent / 100.0,
            max_sessions: parse_or("VOICE_ASSISTANT_MAX_SESSIONS", 10usize),
        }
    }
}

/// Dashboard Help Assistant config (static product-knowledge relay for ALL active
/// B2B members). Decoupled from `VoiceAssistantConfig` — different default pricing,
/// independent session cap, and no role restriction. Activates only when
/// `HELP_ASSISTANT_ENABLED` is truthy AND `OPENAI_API_KEY` is set.
#[derive(Clone)]
pub struct HelpAssistantConfig {
    /// Server-only OpenAI API key (same var as VA/Pro: `OPENAI_API_KEY`).
    /// Never sent to clients. The feature is gated independently on
    /// `HELP_ASSISTANT_ENABLED` so the key can be shared without enabling all tiers.
    pub api_key: String,
    /// GPT-Realtime model id (`HELP_ASSISTANT_MODEL`, default `gpt-realtime-2`).
    pub model: String,
    /// Raw server cost per minute, USD (`HELP_ASSISTANT_COST_PER_MINUTE`, default 0.18).
    /// Never serialized; used only for credit-deduction math.
    pub cost_per_minute: f64,
    /// Markup as a FRACTION (e.g. 0.25 = 25%). Configured via
    /// `HELP_ASSISTANT_MARKUP_PERCENT` (a percentage, e.g. 25), divided by 100.
    /// Default 25.0 → 0.25.
    pub markup: f64,
    /// Hard cap on concurrent help-assistant sessions across the process
    /// (`HELP_ASSISTANT_MAX_SESSIONS`, default 10). Semaphore-backed; new
    /// connections beyond this cap receive a `capacity_full` error and are closed.
    pub max_sessions: usize,
}

impl HelpAssistantConfig {
    fn from_env() -> Self {
        let percent = env::var("HELP_ASSISTANT_MARKUP_PERCENT")
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .unwrap_or(25.0);
        Self {
            api_key: env::var("OPENAI_API_KEY")
                .unwrap_or_default()
                .trim()
                .to_string(),
            model: env::var("HELP_ASSISTANT_MODEL")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "gpt-realtime-2".to_string()),
            cost_per_minute: parse_or("HELP_ASSISTANT_COST_PER_MINUTE", 0.18f64),
            markup: percent / 100.0,
            max_sessions: parse_or("HELP_ASSISTANT_MAX_SESSIONS", 10usize),
        }
    }
}

impl EmbeddingsConfig {
    fn from_env() -> Self {
        Self {
            api_key: env::var("OPENAI_API_KEY")
                .unwrap_or_default()
                .trim()
                .to_string(),
            model: env::var("EMBEDDINGS_MODEL")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "text-embedding-3-small".to_string()),
        }
    }
}

/// Default Qwen-Omni Realtime endpoint — Alibaba Cloud Model Studio's **international**
/// (Singapore) gateway. Accounts that route per workspace can point
/// `QWEN_REALTIME_ENDPOINT` at the regional host template
/// `wss://{workspace}.ap-southeast-1.maas.aliyuncs.com/api-ws/v1/realtime`, whose
/// `{workspace}` placeholder is filled from `QWEN_WORKSPACE_ID`.
const QWEN_DEFAULT_ENDPOINT: &str = "wss://dashscope-intl.aliyuncs.com/api-ws/v1/realtime";

/// Default realtime ASR model backing `input_audio_transcription` — what turns the
/// speaker's own audio into the ORIGINAL-language transcript.
///
/// Overridable through `QWEN_ASR_MODEL` because it is a SECOND catalogue entry the
/// region has to carry: a Model Studio region can serve the livetranslate model and
/// still not serve this one, in which case translation works while original captions
/// and every webinar die. Being able to rename it is what makes that recoverable
/// without a rebuild.
const QWEN_DEFAULT_ASR_MODEL: &str = "qwen3-asr-flash-realtime";

/// A SECOND Model Studio route for the Standard tier — same models and pricing, a
/// different region.
///
/// Standard is the default AND the capacity-fallback engine, and by design it never
/// reports [`crate::engine::SessionOutcome::AtCapacity`], so there is nothing beneath it
/// to catch a failure. That is fine while one region is healthy and catastrophic during
/// a region migration: a Frankfurt key whose catalogue is missing a model makes Pro and
/// Premium overflow land on a dead engine and the room goes silent. This slot is the
/// net — configured, it lets a session that cannot open on the primary route retry once
/// somewhere that still works.
///
/// Only the three ROUTING values differ. Model ids, VAD, segmentation and pricing are
/// tier-wide judgements, not regional ones, so they are inherited from the primary
/// config — see [`QwenConfig::fallback_config`].
#[derive(Clone)]
pub struct QwenFallback {
    /// Credential for the fallback region (`QWEN_FALLBACK_API_KEY`). Defaults to the
    /// primary key when only an endpoint is given. Server-only, never logged.
    pub api_key: String,
    /// WebSocket endpoint for the fallback region (`QWEN_FALLBACK_ENDPOINT`). Accepts
    /// the same `{workspace}` template as the primary endpoint.
    pub endpoint: String,
    /// Workspace id for the fallback region (`QWEN_FALLBACK_WORKSPACE_ID`). Falls back
    /// to the primary workspace when unset.
    pub workspace_id: Option<String>,
}

/// The Standard tier's DashScope credential, under either accepted name.
///
/// `DASHSCOPE_API_KEY` is what Alibaba calls it, so it wins; `QWEN_API_KEY` is accepted
/// because the model — not the platform — is what operators actually think they're
/// configuring, and silently ignoring a key that IS set is a miserable way to spend an
/// afternoon. Returns `None` when neither is set or both are blank.
fn qwen_api_key() -> Option<String> {
    ["DASHSCOPE_API_KEY", "QWEN_API_KEY"]
        .into_iter()
        .find_map(|k| {
            env::var(k)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        })
}

/// The optional second Model Studio route, assembled from `QWEN_FALLBACK_*`.
///
/// Armed by EITHER `QWEN_FALLBACK_ENDPOINT` or `QWEN_FALLBACK_API_KEY`, with the missing
/// half inherited from the primary. Both directions are real configurations — a second
/// region with its own key, and the same account reached through a different host
/// template — and requiring the pair would mean an operator who set one and not the
/// other gets silence instead of a fallback. That is the same trap [`qwen_api_key`]
/// exists to avoid.
///
/// Returns `None` when neither is set, which is the shipped default: no second route,
/// and the single-route behaviour is bit-for-bit what it was before.
fn qwen_fallback(
    primary_key: &str,
    primary_endpoint: &str,
    primary_workspace: Option<&str>,
) -> Option<QwenFallback> {
    let non_empty = |key: &str| {
        env::var(key)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let endpoint = non_empty("QWEN_FALLBACK_ENDPOINT");
    let api_key = non_empty("QWEN_FALLBACK_API_KEY");
    if endpoint.is_none() && api_key.is_none() {
        return None;
    }
    Some(QwenFallback {
        api_key: api_key.unwrap_or_else(|| primary_key.to_string()),
        endpoint: endpoint.unwrap_or_else(|| primary_endpoint.to_string()),
        workspace_id: non_empty("QWEN_FALLBACK_WORKSPACE_ID")
            .or_else(|| primary_workspace.map(str::to_string)),
    })
}

/// Qwen-Omni Realtime credentials + pricing — the **Standard** tier's speech-to-speech
/// engine. Required (not optional like the premium tiers): Standard is the default and
/// capacity-fallback engine, so a server without `DASHSCOPE_API_KEY` has no base tier
/// and refuses to boot.
#[derive(Clone)]
pub struct QwenConfig {
    /// Server-only DashScope API key, sent as `Authorization: Bearer …`. Never
    /// serialized to clients and never logged. Read from `DASHSCOPE_API_KEY` (Alibaba's
    /// own name for it) or `QWEN_API_KEY`, whichever is set — see [`qwen_api_key`].
    pub api_key: String,
    /// Realtime model id (`QWEN_REALTIME_MODEL`). Read from env because Model Studio
    /// revises the id across releases. Its FAMILY also selects the wire dialect — see
    /// [`crate::engine::qwen::QwenDialect`] — so changing this to an omni model changes
    /// the session shape and event names, not just the id.
    pub model: String,
    /// Realtime ASR model id (`QWEN_ASR_MODEL`) used for `input_audio_transcription` in
    /// BOTH session shapes, and dialled directly by the webinar's transcribe-only
    /// session — see [`QWEN_DEFAULT_ASR_MODEL`].
    pub asr_model: String,
    /// WebSocket endpoint template (`QWEN_REALTIME_ENDPOINT`). May contain the literal
    /// `{workspace}` placeholder — see [`QWEN_DEFAULT_ENDPOINT`].
    pub endpoint: String,
    /// Optional second region to retry a failed session on (`QWEN_FALLBACK_*`). `None`
    /// — the default — leaves the single-route behaviour exactly as it was. See
    /// [`QwenFallback`].
    pub fallback: Option<QwenFallback>,
    /// Model Studio workspace id (`QWEN_WORKSPACE_ID`). Substituted into `endpoint` when
    /// it carries the placeholder, otherwise sent as `X-DashScope-WorkSpace`.
    pub workspace_id: Option<String>,
    /// Optional fixed output voice (`QWEN_VOICE`, e.g. `Ethan`). `None` (default) lets
    /// the model pick the natural voice for the target language.
    pub voice: Option<String>,
    /// Turn-detection mode (`QWEN_TURN_DETECTION`). `semantic_vad` is Alibaba's
    /// recommendation for the 3.5-Omni realtime models; `server_vad` is the alternative.
    pub turn_detection: String,
    /// Silence (ms) that ends a turn (`QWEN_SILENCE_MS`). Lower = snappier captions,
    /// higher = fewer mid-sentence splits.
    pub silence_duration_ms: u64,
    /// Idle gap (ms) after which an accumulated transcript is flushed as a FINAL caption
    /// (`QWEN_SEGMENT_IDLE_MS`).
    ///
    /// Shared by both Qwen-backed surfaces — the Standard tier in calls and the webinar
    /// ingest — because it is the same judgement in both: how long a pause has to be
    /// before it counts as the end of a sentence rather than a breath. Too low chops
    /// sentences at natural pauses; too high delays every subtitle by that much. The
    /// premium engines keep their own constants: they talk to different providers, whose
    /// models segment differently, so one number across all four would be a false
    /// uniformity.
    pub segment_idle_ms: u64,
    /// Raw server cost per minute PER TARGET-LANGUAGE SESSION, USD
    /// (`QWEN_COST_PER_MINUTE`). The default is derived in `docs/` from Model Studio's
    /// audio token rates; operators MUST confirm the live number for their region.
    pub cost_per_minute: f64,
    /// Markup as a FRACTION (0.25 = 25%). From `QWEN_COST_MARKUP_PERCENT`, falling back
    /// to `ENGINE_DEFAULT_MARKUP_PERCENT`, divided by 100.
    pub markup: f64,
    /// Hard cap on concurrent Qwen sessions across the process
    /// (`QWEN_REALTIME_MAX_SESSIONS`). One session per speaker per target language.
    pub max_sessions: usize,
}

/// The shipped defaults — every field except the key matches what [`QwenConfig::from_env`]
/// falls back to when the corresponding variable is unset. Exists so config fixtures (and
/// any caller that only wants to override one field) don't have to restate the whole
/// struct and drift from the real defaults.
impl Default for QwenConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            // The DEDICATED simultaneous-interpreter model, not the general omni one.
            // It takes the target language as a field instead of needing a prompt that
            // begs it not to behave like a chatbot — the same reason the Premium tier
            // uses `gemini-*-live-translate` rather than a plain Gemini. Switching model
            // is just an env change; `engine::qwen::QwenDialect` adapts the session shape
            // and turn control automatically.
            //
            // Pinned to the 3.5 REALTIME livetranslate model. An earlier revision fell
            // back to v3 after 3.5 closed every session with `thread pool exausted
            // max_workers 100` — an upstream capacity condition, not a client error, and
            // it cleared. 3.5 is both faster (first audio ~1.3 s vs ~2.8 s on the same
            // clip) and far wider: 29 languages it can SPEAK, against v3's 18. Do not
            // "fix" a transient capacity error by downgrading this again; retry it.
            model: "qwen3.5-livetranslate-flash-realtime".into(),
            asr_model: QWEN_DEFAULT_ASR_MODEL.into(),
            endpoint: QWEN_DEFAULT_ENDPOINT.into(),
            fallback: None,
            workspace_id: None,
            voice: None,
            turn_detection: "semantic_vad".into(),
            silence_duration_ms: 500,
            segment_idle_ms: 900,
            cost_per_minute: 0.0036,
            markup: 0.25,
            max_sessions: 32,
        }
    }
}

impl QwenConfig {
    /// The config that dials the [`fallback`](Self::fallback) route, or `None` when no
    /// second route is configured.
    ///
    /// Everything except the three routing values is inherited, so the fallback session
    /// is the SAME session in another region — same models, same VAD, same billing.
    /// Its own `fallback` is cleared: the result is a leaf, which is what stops a retry
    /// loop from recursing through a chain of regions.
    pub fn fallback_config(&self) -> Option<Self> {
        let fb = self.fallback.as_ref()?;
        Some(Self {
            api_key: fb.api_key.clone(),
            endpoint: fb.endpoint.clone(),
            workspace_id: fb.workspace_id.clone(),
            fallback: None,
            ..self.clone()
        })
    }

    /// Build from the environment. `pub` so the live protocol probe in `engine::qwen`
    /// and the `qwen-catalogue` binary can construct the real config from `.env` rather
    /// than duplicating the variable names (and drifting from them).
    pub fn from_env() -> Self {
        // Markup is configured in PERCENT (e.g. 25); store it as a fraction. Prefer the
        // engine-specific override, then the global engine default, then 25% — Standard
        // keeps the base tier's historical markup, not the premium tiers' 50%.
        let percent = env::var("QWEN_COST_MARKUP_PERCENT")
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .or_else(|| {
                env::var("ENGINE_DEFAULT_MARKUP_PERCENT")
                    .ok()
                    .and_then(|v| v.trim().parse::<f64>().ok())
            })
            .unwrap_or(25.0);
        let non_empty = |key: &str| {
            env::var(key)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        // Every fallback comes from `Default` so the shipped values live in exactly one
        // place; the pricing rationale for `cost_per_minute` is in
        // docs/pricing-standard-qwen.md.
        let d = Self::default();
        // Bound before the literal: the fallback route inherits whichever of these the
        // operator left out, so they have to exist as values first.
        let api_key = qwen_api_key().unwrap_or_default();
        let endpoint = non_empty("QWEN_REALTIME_ENDPOINT").unwrap_or(d.endpoint);
        let workspace_id = non_empty("QWEN_WORKSPACE_ID");
        Self {
            api_key: api_key.clone(),
            model: non_empty("QWEN_REALTIME_MODEL").unwrap_or(d.model),
            asr_model: non_empty("QWEN_ASR_MODEL").unwrap_or(d.asr_model),
            endpoint: endpoint.clone(),
            fallback: qwen_fallback(&api_key, &endpoint, workspace_id.as_deref()),
            workspace_id,
            voice: non_empty("QWEN_VOICE"),
            turn_detection: non_empty("QWEN_TURN_DETECTION").unwrap_or(d.turn_detection),
            silence_duration_ms: parse_or("QWEN_SILENCE_MS", d.silence_duration_ms),
            segment_idle_ms: parse_or("QWEN_SEGMENT_IDLE_MS", d.segment_idle_ms),
            cost_per_minute: parse_or("QWEN_COST_PER_MINUTE", d.cost_per_minute),
            markup: percent / 100.0,
            max_sessions: parse_or("QWEN_REALTIME_MAX_SESSIONS", d.max_sessions),
        }
    }
}

/// Gemini Live Translate credentials + pricing (spec 0100). All-or-nothing like
/// the OpenAI engine: activates only when `GOOGLE_AI_API_KEY` is present.
#[derive(Clone)]
pub struct GeminiConfig {
    /// Server-only Google API key (passed in the Live API URL query string, NOT a
    /// header); never sent to clients and never logged.
    pub api_key: String,
    /// Live Translate model id (`GEMINI_LIVE_TRANSLATE_MODEL`). Read from env because
    /// the preview id changes at GA.
    pub model: String,
    /// Raw server cost per minute, USD (`GEMINI_COST_PER_MINUTE`). Used for the
    /// engine's user rate (`cost × (1 + markup)`); the raw value is never serialized.
    pub cost_per_minute: f64,
    /// Markup as a FRACTION (0.5 = 50%). From `GEMINI_COST_MARKUP_PERCENT`, falling
    /// back to `ENGINE_DEFAULT_MARKUP_PERCENT`, divided by 100.
    pub markup: f64,
    /// Hard cap on concurrent Gemini Live sessions across the process
    /// (`GEMINI_LIVE_MAX_SESSIONS`). The preview tier limits concurrent sessions, so
    /// keep this conservative — we hold one session per target language.
    pub max_sessions: usize,
    /// Optional fixed prebuilt voice (`GEMINI_VOICE`, e.g. `Aoede`, `Kore`, `Puck`).
    /// `None` (default) lets the model choose — on the Live-Translate model that
    /// follows each speaker's own voice, so the timbre varies. Set it to pin one
    /// consistent voice for all translated audio. Opt-in so the default is unchanged.
    pub voice: Option<String>,
}

impl GeminiConfig {
    fn from_env() -> Self {
        // Markup is configured in PERCENT (e.g. 50); store it as a fraction. Prefer
        // the engine-specific override, then the global engine default, then 50%.
        let percent = env::var("GEMINI_COST_MARKUP_PERCENT")
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .or_else(|| {
                env::var("ENGINE_DEFAULT_MARKUP_PERCENT")
                    .ok()
                    .and_then(|v| v.trim().parse::<f64>().ok())
            })
            .unwrap_or(50.0);
        Self {
            api_key: env::var("GOOGLE_AI_API_KEY").unwrap_or_default(),
            model: env::var("GEMINI_LIVE_TRANSLATE_MODEL")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "gemini-3.5-live-translate-preview".into()),
            // Planning figure from spec 0100; operators MUST confirm the real preview
            // rate before launch (it sits below Premium, above Standard).
            cost_per_minute: parse_or("GEMINI_COST_PER_MINUTE", 0.023f64),
            markup: percent / 100.0,
            max_sessions: parse_or("GEMINI_LIVE_MAX_SESSIONS", 16usize),
            voice: env::var("GEMINI_VOICE")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        }
    }
}

impl OpenAiConfig {
    fn from_env() -> Self {
        // Markup is configured in PERCENT (e.g. 50); store it as a fraction. Prefer
        // the engine-specific override, then the global engine default, then 50%.
        let percent = env::var("OPENAI_COST_MARKUP_PERCENT")
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .or_else(|| {
                env::var("ENGINE_DEFAULT_MARKUP_PERCENT")
                    .ok()
                    .and_then(|v| v.trim().parse::<f64>().ok())
            })
            .unwrap_or(50.0);
        Self {
            api_key: env::var("OPENAI_API_KEY").unwrap_or_default(),
            model: env::var("OPENAI_REALTIME_MODEL")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "gpt-realtime-translate".into()),
            // Realtime speech-to-speech is materially pricier than Deepgram+Groq;
            // this is a conservative placeholder — operators MUST set the real rate.
            cost_per_minute: parse_or("OPENAI_COST_PER_MINUTE", 0.30f64),
            markup: percent / 100.0,
            max_sessions: parse_or("OPENAI_REALTIME_MAX_SESSIONS", 16usize),
            voice: env::var("OPENAI_VOICE")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        }
    }
}

/// Cartesia REST/WS API base (spec 0108). The browser connects DIRECTLY to the STT/TTS
/// WebSockets with a short-lived access token minted server-side; the raw `CARTESIA_API_KEY`
/// never leaves the server. Override per deployment via `CARTESIA_API_BASE` (REST) and
/// `CARTESIA_STT_ENDPOINT` / `CARTESIA_TTS_ENDPOINT` (WS).
const CARTESIA_DEFAULT_API_BASE: &str = "https://api.cartesia.ai";
const CARTESIA_DEFAULT_STT_ENDPOINT: &str = "wss://api.cartesia.ai/stt/websocket";
const CARTESIA_DEFAULT_TTS_ENDPOINT: &str = "wss://api.cartesia.ai/tts/websocket";
/// Cartesia API version, sent as the `Cartesia-Version` header / `cartesia_version` query
/// param. Override via `CARTESIA_VERSION`.
const CARTESIA_DEFAULT_VERSION: &str = "2026-03-01";

/// Cartesia "Enhanced" credentials + pricing + endpoints (spec 0108). All-or-nothing like
/// the other engine configs, but additionally gated behind the `CARTESIA_ENHANCED` flag.
/// Cost/markup follow the OpenAI/Gemini pattern exactly; nothing here touches
/// billing logic — the values flow into `EngineMetadata` and the existing meter does the
/// rest. Cartesia does STT + TTS but NOT translation (Groq stays the translator); it is a
/// single global endpoint, so there is no region map.
#[derive(Clone)]
pub struct CartesiaConfig {
    /// Raw server API key (`CARTESIA_API_KEY`, `sk_car_…`). Server-only — never serialized;
    /// the browser receives only short-lived access tokens minted from it.
    pub api_key: String,
    /// Realtime STT model id (`CARTESIA_STT_MODEL`). Defaults to `ink-whisper`, which is
    /// multilingual — `ink-2` is ENGLISH-ONLY and closes the STT WS with `1008 "Invalid
    /// language for model"` for any non-`en` speaker, dropping Enhanced to Standard. Only
    /// override to `ink-2` for an English-only deployment.
    pub stt_model: String,
    /// Per-language STT model overrides (`CARTESIA_STT_MODEL_BY_LANG`, a JSON object like
    /// `{"en":"ink-2"}`). The client picks, per speaker, the fastest model that supports the
    /// SOURCE language, falling back to `stt_model` (multilingual). Defaults to routing
    /// English to `ink-2` (Cartesia's fastest STT, English-only); every other language uses
    /// `ink-whisper`. Keyed by lowercase base language code.
    pub stt_model_by_lang: HashMap<String, String>,
    /// Streaming TTS model id (`CARTESIA_TTS_MODEL`, e.g. `sonic-3.5`).
    pub tts_model: String,
    /// Raw server cost per minute, USD (`CARTESIA_COST_PER_MINUTE`). Never serialized.
    pub cost_per_minute: f64,
    /// Markup as a FRACTION (0.85 = 85%). From `CARTESIA_COST_MARKUP_PERCENT`, falling
    /// back to `ENGINE_DEFAULT_MARKUP_PERCENT`, divided by 100. Never serialized.
    pub markup: f64,
    /// Instant Voice Cloning master switch (`CARTESIA_VOICE_CLONING_ENABLED`). When off,
    /// the pre-join voice-prep step is skipped and TTS uses a default voice.
    pub voice_cloning_enabled: bool,
    /// Optional fallback Cartesia voice id (`CARTESIA_DEFAULT_VOICE_ID`) used for a speaker
    /// without a clone (cloning off, or they never completed voice prep). `None` → such a
    /// speaker is shown subtitles only. Sent to the client with the session.
    pub default_voice_id: Option<String>,
    /// REST base for access-token minting + voice cloning (`CARTESIA_API_BASE`).
    pub api_base: String,
    /// Public STT WebSocket endpoint handed to the browser (`CARTESIA_STT_ENDPOINT`).
    pub stt_endpoint: String,
    /// Public TTS WebSocket endpoint handed to the browser (`CARTESIA_TTS_ENDPOINT`).
    pub tts_endpoint: String,
    /// API version string (`CARTESIA_VERSION`).
    pub version: String,
}

impl CartesiaConfig {
    fn from_env() -> Self {
        // Markup is configured in PERCENT (e.g. 85); store it as a fraction. Prefer the
        // engine-specific override, then the global engine default, then 50%.
        let percent = env::var("CARTESIA_COST_MARKUP_PERCENT")
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .or_else(|| {
                env::var("ENGINE_DEFAULT_MARKUP_PERCENT")
                    .ok()
                    .and_then(|v| v.trim().parse::<f64>().ok())
            })
            .unwrap_or(50.0);
        let str_or = |key: &str, default: &str| {
            env::var(key)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| default.to_string())
        };
        Self {
            api_key: env::var("CARTESIA_API_KEY")
                .unwrap_or_default()
                .trim()
                .to_string(),
            stt_model: str_or("CARTESIA_STT_MODEL", "ink-whisper"),
            stt_model_by_lang: parse_stt_model_map(
                env::var("CARTESIA_STT_MODEL_BY_LANG").ok().as_deref(),
            ),
            tts_model: str_or("CARTESIA_TTS_MODEL", "sonic-3.5"),
            cost_per_minute: parse_or("CARTESIA_COST_PER_MINUTE", 0.036f64),
            markup: percent / 100.0,
            voice_cloning_enabled: env_flag("CARTESIA_VOICE_CLONING_ENABLED"),
            default_voice_id: env::var("CARTESIA_DEFAULT_VOICE_ID")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            api_base: str_or("CARTESIA_API_BASE", CARTESIA_DEFAULT_API_BASE),
            stt_endpoint: str_or("CARTESIA_STT_ENDPOINT", CARTESIA_DEFAULT_STT_ENDPOINT),
            tts_endpoint: str_or("CARTESIA_TTS_ENDPOINT", CARTESIA_DEFAULT_TTS_ENDPOINT),
            version: str_or("CARTESIA_VERSION", CARTESIA_DEFAULT_VERSION),
        }
    }

    /// `POST {api_base}/access-token` — mints a short-lived access token for the browser.
    pub fn access_token_url(&self) -> String {
        format!("{}/access-token", self.api_base.trim_end_matches('/'))
    }

    /// `POST {api_base}/voices/clone` — Instant Voice Cloning (IVC).
    pub fn clone_voice_url(&self) -> String {
        format!("{}/voices/clone", self.api_base.trim_end_matches('/'))
    }
}

/// How `/api/ice` authenticates a client to the TURN relay (spec 0026 / 0059).
#[derive(Clone)]
pub enum TurnCred {
    /// coturn REST convention: mint a time-limited credential by HMAC-signing an
    /// expiry with the `static-auth-secret`. The secret never leaves the server —
    /// only the short-lived derived credential does (self-hosted coturn).
    Secret { secret: String, ttl_secs: u64 },
    /// A managed relay's long-lived username/password, passed straight through —
    /// the zero-deploy fallback (Metered / Twilio / etc., issue #40). These DO
    /// reach the client, so use a relay-scoped account.
    Static { username: String, password: String },
    /// Cloudflare Realtime TURN (spec 0077 / issue #112): `/api/ice` mints a
    /// short-lived credential per request by calling Cloudflare's API with a TURN
    /// key (`key_id` + `api_token`). Cloudflare returns geo-distributed (anycast)
    /// URLs plus a time-limited username/credential, so the relay sits near each
    /// user. The `api_token` is server-only and never reaches the client.
    Cloudflare {
        key_id: String,
        api_token: String,
        ttl_secs: u64,
    },
}

impl TurnCred {
    /// Choose a credential mode from raw values. Precedence: **Cloudflare** (key +
    /// token both set) wins, then the HMAC **secret**, then the **static**
    /// username/password. Pure (no env reads) so it's unit-testable. `None` ⇒ TURN
    /// stays off.
    fn pick(
        cf_key_id: &str,
        cf_api_token: &str,
        secret: &str,
        username: &str,
        password: &str,
        ttl_secs: u64,
    ) -> Option<Self> {
        if !cf_key_id.is_empty() && !cf_api_token.is_empty() {
            Some(TurnCred::Cloudflare {
                key_id: cf_key_id.to_string(),
                api_token: cf_api_token.to_string(),
                ttl_secs,
            })
        } else if !secret.is_empty() {
            Some(TurnCred::Secret {
                secret: secret.to_string(),
                ttl_secs,
            })
        } else if !username.is_empty() && !password.is_empty() {
            Some(TurnCred::Static {
                username: username.to_string(),
                password: password.to_string(),
            })
        } else {
            None
        }
    }
}

/// TURN (media relay) config for when direct P2P fails (spec 0026). Active when a
/// credential mode is configured: `TURN_CLOUDFLARE_KEY_ID` + `TURN_CLOUDFLARE_API_TOKEN`
/// (Cloudflare Realtime TURN, spec 0077), `TURN_URLS` + `TURN_SECRET` (self-hosted
/// coturn), or `TURN_URLS` + `TURN_USERNAME` + `TURN_PASSWORD` (a managed relay,
/// spec 0059). Without it the client uses STUN only and cross-NAT (e.g. cross-border)
/// calls may fail.
#[derive(Clone)]
pub struct TurnConfig {
    /// TURN URLs, e.g. `turn:relay.example.com:3478?transport=tcp`. Empty in the
    /// Cloudflare mode — there the minted response carries Cloudflare's anycast URLs.
    pub urls: Vec<String>,
    /// The credential the client presents to the relay.
    pub cred: TurnCred,
}

impl TurnConfig {
    /// Parse from env, or `None` when TURN isn't fully configured (no URLs, or no
    /// usable credential).
    fn from_env() -> Option<Self> {
        let urls: Vec<String> = env::var("TURN_URLS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let cred = TurnCred::pick(
            &env::var("TURN_CLOUDFLARE_KEY_ID").unwrap_or_default(),
            &env::var("TURN_CLOUDFLARE_API_TOKEN").unwrap_or_default(),
            &env::var("TURN_SECRET").unwrap_or_default(),
            &env::var("TURN_USERNAME").unwrap_or_default(),
            &env::var("TURN_PASSWORD").unwrap_or_default(),
            parse_or("TURN_TTL_SECS", 3600u64),
        )?;
        // Cloudflare returns its own (anycast) URLs in the minted response; the
        // coturn/managed modes need TURN_URLS to point at the relay.
        if !matches!(cred, TurnCred::Cloudflare { .. }) && urls.is_empty() {
            return None;
        }
        Some(TurnConfig { urls, cred })
    }

    /// Build the restricted-network (GFW) TURN profile from raw values. Pure (no env
    /// reads) so it's unit-testable. Requires non-empty `urls` (the `turns://…:443`
    /// relay) AND a resolvable credential — HMAC secret (coturn) preferred, else a
    /// managed relay's static username/password. Cloudflare is intentionally excluded:
    /// it offers no `turns://…:443` endpoint, which is the entire reason this profile
    /// exists. `None` when URLs or a credential are missing.
    fn restricted(
        urls: Vec<String>,
        secret: &str,
        username: &str,
        password: &str,
        ttl_secs: u64,
    ) -> Option<Self> {
        if urls.is_empty() {
            return None;
        }
        // Reuse `pick` with Cloudflare disabled → precedence Secret → Static → None.
        let cred = TurnCred::pick("", "", secret, username, password, ttl_secs)?;
        Some(TurnConfig { urls, cred })
    }

    /// Parse the restricted-network TURN profile from `TURN_TLS_URLS` + a credential
    /// (`TURN_TLS_SECRET`, or `TURN_TLS_USERNAME` + `TURN_TLS_PASSWORD`), sharing
    /// `TURN_TTL_SECS` with the default relay. `None` unless fully configured, so the
    /// feature is off by default and the default `turn` path is unaffected.
    fn restricted_from_env() -> Option<Self> {
        let urls: Vec<String> = env::var("TURN_TLS_URLS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        Self::restricted(
            urls,
            &env::var("TURN_TLS_SECRET").unwrap_or_default(),
            &env::var("TURN_TLS_USERNAME").unwrap_or_default(),
            &env::var("TURN_TLS_PASSWORD").unwrap_or_default(),
            parse_or("TURN_TTL_SECS", 3600u64),
        )
    }
}

/// Supabase Storage credentials for chat file upload (spec 0018). All-or-nothing
/// like billing — the feature activates only when both URL and key are present.
#[derive(Clone)]
pub struct StorageConfig {
    /// Project base URL, e.g. `https://<ref>.supabase.co` (no trailing slash).
    pub supabase_url: String,
    /// Service-role key — server-only, never sent to the client.
    pub service_key: String,
    /// Bucket name; defaults to `chat-files`.
    pub bucket: String,
    /// Max upload size in bytes (default 25 MiB).
    pub max_bytes: usize,
    /// Signed-URL lifetime in seconds — how long a chat file download link stays
    /// valid (the bucket is private). Default 24h (issue #117: was 7 days — the URL is
    /// broadcast in the chat message, so keep the exposure window short); override with
    /// `SUPABASE_SIGNED_URL_TTL_SECS`.
    pub signed_ttl_secs: u64,
}

/// Everything needed for accounts, credits, and payments.
#[derive(Clone)]
pub struct BillingConfig {
    pub database_url: String,
    pub google_client_id: String,
    /// OAuth web-client secret — needed for the authorization-code exchange that
    /// yields a refresh token (Calendar access). Empty disables the calendar flow.
    pub google_client_secret: String,
    /// 32-byte AEAD key (base64 `GOOGLE_TOKEN_ENC_KEY`) used to encrypt Google
    /// refresh tokens at rest. `None`/wrong-length disables the calendar flow.
    pub google_token_enc_key: Option<Vec<u8>>,
    /// Space-separated OAuth scopes requested at login (incl. `calendar.events`).
    pub google_calendar_scopes: String,
    pub jwt_secret: String,
    pub jwt_expiry_hours: i64,
    pub stripe_secret_key: String,
    pub stripe_webhook_secret: String,
    pub stripe_success_url: String,
    pub stripe_cancel_url: String,
    /// Optional cap on guest (un-authenticated) session length.
    pub guest_max_minutes: Option<u64>,
    /// Shared secret the Directus backoffice presents to the `/api/admin/*`
    /// endpoints (server-to-server). When absent, admin endpoints are disabled.
    pub admin_api_secret: Option<String>,
    pub pricing: PricingConfig,
    /// Max term pairs allowed per room glossary.
    pub glossary_max_entries: usize,
    pub ai: AiConfig,
    /// Org (B2B) billing — subscriptions + portal + one-off top-up (spec 0106).
    /// `Some` only when `ORG_STRIPE_WEBHOOK_SECRET` is configured.
    pub org_billing: Option<OrgBillingConfig>,
}

/// B2B org billing config (spec 0106): Stripe price ids per plan/interval, the
/// monthly credit allotment per plan (annual grants 12×), the one-off credit unit
/// price, and the URLs + webhook secret for the org Checkout/Portal flow.
#[derive(Clone)]
pub struct OrgBillingConfig {
    pub webhook_secret: String,
    pub success_url: String,
    pub cancel_url: String,
    pub portal_return_url: String,
    /// One-off purchase: Stripe charges this many cents per credit.
    pub credit_unit_amount_cents: i64,
    pub business_monthly_price_id: String,
    pub business_annual_price_id: String,
    pub enterprise_monthly_price_id: String,
    pub enterprise_annual_price_id: String,
    pub business_monthly_credits: i32,
    pub enterprise_monthly_credits: i32,
}

impl OrgBillingConfig {
    fn from_env() -> Self {
        Self {
            webhook_secret: env::var("ORG_STRIPE_WEBHOOK_SECRET").unwrap_or_default(),
            success_url: env::var("ORG_STRIPE_SUCCESS_URL").unwrap_or_default(),
            cancel_url: env::var("ORG_STRIPE_CANCEL_URL").unwrap_or_default(),
            portal_return_url: env::var("ORG_STRIPE_PORTAL_RETURN_URL").unwrap_or_default(),
            credit_unit_amount_cents: parse_or("ORG_CREDIT_UNIT_CENTS", 100i64),
            business_monthly_price_id: env::var("ORG_PRICE_BUSINESS_MONTHLY").unwrap_or_default(),
            business_annual_price_id: env::var("ORG_PRICE_BUSINESS_ANNUAL").unwrap_or_default(),
            enterprise_monthly_price_id: env::var("ORG_PRICE_ENTERPRISE_MONTHLY")
                .unwrap_or_default(),
            enterprise_annual_price_id: env::var("ORG_PRICE_ENTERPRISE_ANNUAL").unwrap_or_default(),
            business_monthly_credits: parse_or("ORG_CREDITS_BUSINESS_MONTHLY", 1000i32),
            enterprise_monthly_credits: parse_or("ORG_CREDITS_ENTERPRISE_MONTHLY", 5000i32),
        }
    }

    /// Stripe price id for a `(plan, interval)` pair, or `None` if unknown/unset.
    pub fn price_id(&self, plan: &str, interval: &str) -> Option<&str> {
        let id = match (plan, interval) {
            ("business", "month") => &self.business_monthly_price_id,
            ("business", "year") => &self.business_annual_price_id,
            ("enterprise", "month") => &self.enterprise_monthly_price_id,
            ("enterprise", "year") => &self.enterprise_annual_price_id,
            _ => return None,
        };
        Some(id).filter(|s| !s.is_empty()).map(String::as_str)
    }

    /// Reverse-map a Stripe price id back to its `(plan, interval)`. Used by the
    /// webhook (invoices carry the price id, not our metadata).
    pub fn plan_interval_for_price(&self, price_id: &str) -> Option<(&'static str, &'static str)> {
        if price_id.is_empty() {
            return None;
        }
        if price_id == self.business_monthly_price_id {
            Some(("business", "month"))
        } else if price_id == self.business_annual_price_id {
            Some(("business", "year"))
        } else if price_id == self.enterprise_monthly_price_id {
            Some(("enterprise", "month"))
        } else if price_id == self.enterprise_annual_price_id {
            Some(("enterprise", "year"))
        } else {
            None
        }
    }

    /// Credits granted per successful invoice for a `(plan, interval)`. Annual
    /// invoices grant 12× the monthly allotment.
    pub fn grant_credits(&self, plan: &str, interval: &str) -> i32 {
        let monthly = match plan {
            "enterprise" => self.enterprise_monthly_credits,
            _ => self.business_monthly_credits,
        };
        if interval == "year" {
            monthly * 12
        } else {
            monthly
        }
    }
}

/// AI-feature pricing and models. Costs are USD (same unit as `users.balance`),
/// configurable per feature without code changes. Env names follow the product
/// spec (`CREDITS_*`) even though values are decimal USD.
#[derive(Debug, Clone)]
pub struct AiConfig {
    /// Model for offline analysis (report, sentiment, email draft).
    pub report_model: String,
    /// Model used when the primary model errors (and for live suggestions).
    pub fallback_model: String,
    pub report_base: f64,
    pub report_per_minute: f64,
    pub sentiment_base: f64,
    pub sentiment_per_participant: f64,
    pub sentiment_per_minute: f64,
    pub email_draft: f64,
    pub suggestions_per_minute: f64,
    pub suggestions_interval_secs: u64,
    /// On-demand AI quiz (spec 0067): base + per-question credit rate.
    pub quiz_base: f64,
    pub quiz_per_question: f64,
    /// Transcript correction on export (spec 0068): base + per-event credit
    /// rate (per_event is per corrected text field; `both` mode doubles it).
    pub correction_base: f64,
    pub correction_per_event: f64,
    /// Chat document-upload translation: base + per-target-language credit rate.
    /// Groq 8B cost is tiny, so even a small charge keeps a margin (tune via env
    /// if the real margin drifts).
    pub upload_translate_base: f64,
    pub upload_translate_per_lang: f64,
}

/// Resend (transactional email) credentials. All-or-nothing like billing.
#[derive(Clone)]
pub struct ResendConfig {
    pub api_key: String,
    pub from_email: String,
    pub from_name: String,
}

/// Pricing — all values from env. The user-facing rate (cost × markup) and the
/// raw cost are NEVER serialized to clients.
#[derive(Debug, Clone)]
pub struct PricingConfig {
    pub cost_per_minute: f64,
    pub markup_percentage: f64,
    pub user_rate_per_minute: f64,
    pub user_rate_per_second: f64,
    pub free_credits: f64,
    pub low_balance_threshold: f64,
    pub min_balance_to_join: f64,
    pub usage_update_interval: u64,
    pub packages: Vec<CreditPackage>,
}

/// A purchasable credit package. `stripe_price_id` is read from env but never
/// sent to the client (`skip_serializing`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreditPackage {
    pub id: String,
    pub name: String,
    pub price_usd: f64,
    pub credits_usd: f64,
    #[serde(skip_serializing)]
    pub stripe_price_id: String,
}

impl Config {
    /// Load configuration from the process environment.
    pub fn from_env() -> Result<Self, String> {
        // Standard is the default + capacity-fallback engine and now runs on Qwen-Omni
        // Realtime, so DashScope is the key the server cannot boot without.
        match qwen_api_key() {
            None => return Err("missing DASHSCOPE_API_KEY (or QWEN_API_KEY)".into()),
            // A key with whitespace INSIDE it is always a broken paste — most often a
            // value wrapped across two lines in .env. It cannot be sent as an HTTP header,
            // so catch it at boot: the alternative is a socket that fails to open for
            // every speaker with a far less obvious error.
            Some(k) if k.bytes().any(|b| !b.is_ascii_graphic()) => {
                let at = k.bytes().position(|b| !b.is_ascii_graphic()).unwrap_or(0);
                return Err(format!(
                    "DASHSCOPE_API_KEY/QWEN_API_KEY contains whitespace at position {at} \
                     of {} — it is probably split across two lines in .env. \
                     Put the whole key on ONE line.",
                    k.len()
                ));
            }
            Some(_) => {}
        }
        // Deepgram no longer serves any live tier — it remains only for the BATCH
        // (prerecorded) transcription of uploaded files, recordings, and voice messages,
        // which degrade gracefully when unset. Optional, never `require`d.
        let deepgram_key = env::var("DEEPGRAM_API_KEY").unwrap_or_default();
        let groq_key = require("GROQ_API_KEY")?;
        let translation_model = env::var("GROQ_TRANSLATION_MODEL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "openai/gpt-oss-20b".into());
        let port = parse_or("PORT", 3001u16);
        let extension_origins: Vec<String> = env::var("EXTENSION_ORIGINS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|o| o.starts_with("chrome-extension://"))
            .map(str::to_string)
            .collect();
        let allowed_origins = env::var("ALLOWED_ORIGINS")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(|o| o.trim().to_string())
                    .filter(|o| !o.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        // Billing activates only when the three core values are present.
        let billing =
            if present("DATABASE_URL") && present("GOOGLE_CLIENT_ID") && present("JWT_SECRET") {
                // Sessions are HS256 JWTs signed with JWT_SECRET; a short/low-entropy
                // secret is offline-brute-forceable from any issued token, which would
                // let an attacker forge a token for any user (account takeover). Refuse
                // to enable billing with a weak secret rather than boot insecure. Use
                // `openssl rand -hex 32` (or -base64 32) to generate one.
                const MIN_JWT_SECRET_BYTES: usize = 32;
                let jwt_len = env::var("JWT_SECRET").unwrap_or_default().trim().len();
                if jwt_len < MIN_JWT_SECRET_BYTES {
                    return Err(format!(
                        "JWT_SECRET is too short ({jwt_len} bytes); require at least \
                         {MIN_JWT_SECRET_BYTES} bytes of entropy (e.g. `openssl rand -hex 32`)"
                    ));
                }
                Some(BillingConfig::from_env())
            } else {
                None
            };

        let resend = if present("RESEND_API_KEY")
            && present("RESEND_FROM_EMAIL")
            && present("RESEND_FROM_NAME")
        {
            Some(ResendConfig::from_env())
        } else {
            None
        };

        // Chat file upload (spec 0018) activates only when both Supabase Storage
        // values are present. The bucket name is optional (defaults below).
        let storage = if present("SUPABASE_URL") && present("SUPABASE_SERVICE_KEY") {
            Some(StorageConfig::from_env())
        } else {
            None
        };

        // TURN relay (spec 0026 / 0059): `TURN_URLS` plus a credential — either the
        // coturn `TURN_SECRET`, or a managed relay's `TURN_USERNAME` + `TURN_PASSWORD`.
        // `from_env` returns None when it's not fully configured.
        let turn = TurnConfig::from_env();
        // Restricted-network (GFW) profile: a `turns://…:443` TLS relay handed only to
        // clients that request `/api/ice?restricted=1`. Off unless `TURN_TLS_*` is set.
        let turn_restricted = TurnConfig::restricted_from_env();

        // Per-tier rollout flags (spec 0101): each tier is gated on BOTH its provider
        // key AND a `<PROVIDER>_<TIER>` enable flag, so operators can switch a tier on or
        // off from the environment without changing keys. The three optional tiers default
        // OFF (flag required); Standard is the base tier and defaults ON (see below).

        // Pro engine — OpenAI GPT-Realtime-Translate (spec 0093): `OPENAI_PRO` + key.
        let openai = if env_flag("OPENAI_PRO") && present("OPENAI_API_KEY") {
            Some(OpenAiConfig::from_env())
        } else {
            None
        };

        // Semantic transcript search (Business dashboard): OpenAI embeddings. Reuses
        // the OpenAI key but is DECOUPLED from `OPENAI_PRO` — present whenever the key
        // is set, so search works even when the realtime Pro tier is dark.
        let embeddings = if present("OPENAI_API_KEY") {
            Some(EmbeddingsConfig::from_env())
        } else {
            None
        };

        // B2B Voice Assistant: GPT-Realtime relay. Gated on BOTH the enable flag AND
        // the OpenAI key — ships dark until both are set. Decoupled from OPENAI_PRO so
        // the voice assistant can be activated without enabling the translation tier.
        let voice_assistant = if env_flag("VOICE_ASSISTANT_ENABLED") && present("OPENAI_API_KEY") {
            Some(VoiceAssistantConfig::from_env())
        } else {
            None
        };

        // Dashboard Help Assistant: static-prompt GPT-Realtime relay for ALL active
        // B2B members. Independent kill switch + pricing from the voice assistant.
        // Ships dark until HELP_ASSISTANT_ENABLED=true AND OPENAI_API_KEY is set.
        let help_assistant = if env_flag("HELP_ASSISTANT_ENABLED") && present("OPENAI_API_KEY") {
            Some(HelpAssistantConfig::from_env())
        } else {
            None
        };

        // Webinar Mode (SPEC "Webinar Mode"): the 1-to-many broadcast control plane.
        // Enabled once the off-box media server host AND its publish-auth secrets are
        // configured; ships dark otherwise (the /api/webinars, /api/w/{code}, and
        // /internal/media-auth routes are not registered).
        let webinar = if present("MEDIA_INGEST_HOST")
            && present("MEDIAMTX_AUTH_SECRET")
            && present("MEDIA_CALLER_SECRET")
        {
            Some(WebinarConfig::from_env())
        } else {
            None
        };

        // Premium engine — Gemini Live Translate (spec 0100): `GEMINI_PREMIUM` + key.
        let google = if env_flag("GEMINI_PREMIUM") && present("GOOGLE_AI_API_KEY") {
            Some(GeminiConfig::from_env())
        } else {
            None
        };

        // Enhanced engine — Cartesia (spec 0108): `CARTESIA_ENHANCED` + key.
        let cartesia = if env_flag("CARTESIA_ENHANCED") && present("CARTESIA_API_KEY") {
            Some(CartesiaConfig::from_env())
        } else {
            None
        };

        // Standard engine — Qwen-Omni Realtime (the base tier). Same flag pattern for
        // symmetry, but defaults ON when unset: Standard is the registry's default /
        // capacity-fallback engine, so the server force-registers it even when this is
        // off (with a warning) — you can't actually leave the app without a default.
        // Which is also why its key is `require`d above rather than gating an `Option`.
        let standard_enabled = env_flag_or("QWEN_STANDARD", true);
        let qwen = QwenConfig::from_env();

        Ok(Self {
            deepgram_key,
            groq_key,
            translation_model,
            port,
            allowed_origins,
            extension_origins,
            auto_detect_buffer_ms: parse_or("AUTO_DETECT_BUFFER_MS", 3000u64),
            billing,
            resend,
            storage,
            turn,
            turn_restricted,
            bug_report_to: env::var("BUG_REPORT_TO")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "micio86dev@gmail.com".into()),
            app_base_url: env::var("APP_BASE_URL")
                .ok()
                .map(|s| s.trim().trim_end_matches('/').to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "https://app.voxtranslate.app".into()),
            dashboard_base_url: env::var("DASHBOARD_BASE_URL")
                .ok()
                .map(|s| s.trim().trim_end_matches('/').to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "https://dashboard.voxtranslate.app".into()),
            push: PushConfig::from_env(),
            business_member_limit: parse_or("ORG_BUSINESS_MEMBER_LIMIT", 20i64),
            retention_sweep_enabled: env_flag("RETENTION_SWEEP_ENABLED"),
            retention_sweep_interval_secs: parse_or("RETENTION_SWEEP_INTERVAL_SECS", 21_600u64),
            retention_sweep_batch: parse_or("RETENTION_SWEEP_BATCH", 200i64),
            openai,
            google,
            cartesia,
            qwen,
            standard_enabled,
            pro_enabled: env_flag("PRO_TIER_ENABLED"),
            listener_pays: env_flag("LISTENER_PAYS"),
            language_first_ux: env_flag("LANGUAGE_FIRST_UX"),
            // Translation cache (spec 0107) — opt-in, fail-open. The flag only arms the
            // cache; the actual connect happens lazily in `AppState::init` and degrades
            // to "always miss" when the URL is unset or DragonflyDB is unreachable.
            cache_enabled: env_flag("TRANSLATION_CACHE_ENABLED"),
            cache_max_words: parse_or("TRANSLATION_CACHE_MAX_WORDS", 8usize),
            cache_ttl_secs: parse_or("TRANSLATION_CACHE_TTL_SECONDS", 604_800u64),
            dragonfly_url: env::var("DRAGONFLY_PRIVATE_URL")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            bench_secret: env::var("BENCH_SECRET")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            embeddings,
            embeddings_backfill_secret: env::var("EMBEDDINGS_BACKFILL_SECRET")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            voice_assistant,
            help_assistant,
            webinar,
        })
    }

    pub fn billing_enabled(&self) -> bool {
        self.billing.is_some()
    }
}

/// Web-push (VAPID) keys + contact subject. Keys are URL-safe base64 (no padding),
/// the format emitted by `web-push generate-vapid-keys` / online generators.
#[derive(Clone)]
pub struct PushConfig {
    pub vapid_public_key: String,
    pub vapid_private_key: String,
    /// VAPID `sub` claim — a `mailto:` or `https:` contact URL.
    pub vapid_subject: String,
}

impl PushConfig {
    /// `Some` only when both VAPID keys are present.
    fn from_env() -> Option<Self> {
        let public = env::var("VAPID_PUBLIC_KEY").unwrap_or_default();
        let private = env::var("VAPID_PRIVATE_KEY").unwrap_or_default();
        if public.trim().is_empty() || private.trim().is_empty() {
            return None;
        }
        Some(Self {
            vapid_public_key: public.trim().to_string(),
            vapid_private_key: private.trim().to_string(),
            vapid_subject: env::var("VAPID_SUBJECT")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "mailto:micio86dev@gmail.com".into()),
        })
    }
}

impl BillingConfig {
    /// True when the Google Calendar OAuth flow is fully configured (client secret +
    /// a valid 32-byte token-encryption key). Calendar/meeting features gate on this.
    pub fn calendar_enabled(&self) -> bool {
        !self.google_client_secret.is_empty() && self.google_token_enc_key.is_some()
    }

    fn from_env() -> Self {
        Self {
            database_url: env::var("DATABASE_URL").unwrap_or_default(),
            google_client_id: env::var("GOOGLE_CLIENT_ID").unwrap_or_default(),
            google_client_secret: env::var("GOOGLE_CLIENT_SECRET").unwrap_or_default(),
            google_token_enc_key: env::var("GOOGLE_TOKEN_ENC_KEY")
                .ok()
                .and_then(|s| decode_enc_key(s.trim())),
            google_calendar_scopes: env::var("GOOGLE_CALENDAR_SCOPES")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    "openid email profile https://www.googleapis.com/auth/calendar.events".into()
                }),
            jwt_secret: env::var("JWT_SECRET").unwrap_or_default(),
            jwt_expiry_hours: parse_or("JWT_EXPIRY_HOURS", 168i64),
            stripe_secret_key: env::var("STRIPE_SECRET_KEY").unwrap_or_default(),
            stripe_webhook_secret: env::var("STRIPE_WEBHOOK_SECRET").unwrap_or_default(),
            stripe_success_url: env::var("STRIPE_SUCCESS_URL").unwrap_or_default(),
            stripe_cancel_url: env::var("STRIPE_CANCEL_URL").unwrap_or_default(),
            // Cumulative guest STT cap (H1): defaults to a safe non-`None` value so a
            // fresh deployment never leaves unauthenticated speech-to-text uncapped.
            // `GUEST_MAX_MINUTES=0` explicitly disables the cap (opt-in unlimited).
            // Enforced per-IP over a rolling window (see `GuestUsage` in lib.rs), so
            // reconnecting no longer resets it.
            guest_max_minutes: match env::var("GUEST_MAX_MINUTES")
                .ok()
                .map(|s| s.trim().to_string())
            {
                Some(s) if s.is_empty() => Some(DEFAULT_GUEST_MAX_MINUTES),
                Some(s) => match s.parse::<u64>() {
                    Ok(0) => None, // explicit opt-out
                    Ok(n) => Some(n),
                    Err(_) => Some(DEFAULT_GUEST_MAX_MINUTES),
                },
                None => Some(DEFAULT_GUEST_MAX_MINUTES),
            },
            admin_api_secret: env::var("ADMIN_API_SECRET")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            pricing: PricingConfig::from_env(),
            glossary_max_entries: parse_or("GLOSSARY_MAX_ENTRIES", 200usize),
            ai: AiConfig::from_env(),
            // B2B billing activates only when its webhook secret is present.
            org_billing: present("ORG_STRIPE_WEBHOOK_SECRET").then(OrgBillingConfig::from_env),
        }
    }
}

impl AiConfig {
    fn from_env() -> Self {
        Self {
            report_model: env::var("GROQ_REPORT_MODEL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "openai/gpt-oss-120b".into()),
            fallback_model: env::var("GROQ_FALLBACK_MODEL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "openai/gpt-oss-20b".into()),
            report_base: parse_or("CREDITS_REPORT_BASE", 0.05f64),
            report_per_minute: parse_or("CREDITS_REPORT_PER_MINUTE", 0.002f64),
            sentiment_base: parse_or("CREDITS_SENTIMENT_BASE", 0.05f64),
            sentiment_per_participant: parse_or("CREDITS_SENTIMENT_PER_PARTICIPANT", 0.01f64),
            sentiment_per_minute: parse_or("CREDITS_SENTIMENT_PER_MINUTE", 0.002f64),
            email_draft: parse_or("CREDITS_EMAIL_DRAFT", 0.02f64),
            suggestions_per_minute: parse_or("CREDITS_SUGGESTIONS_PER_MINUTE", 0.005f64),
            suggestions_interval_secs: parse_or("SUGGESTIONS_INTERVAL_SECONDS", 15u64),
            quiz_base: parse_or("CREDITS_QUIZ_BASE", 0.03f64),
            quiz_per_question: parse_or("CREDITS_QUIZ_PER_QUESTION", 0.01f64),
            correction_base: parse_or("CREDITS_CORRECTION_BASE", 0.05f64),
            correction_per_event: parse_or("CREDITS_CORRECTION_PER_EVENT", 0.001f64),
            upload_translate_base: parse_or("CREDITS_UPLOAD_TRANSLATE_BASE", 0.01f64),
            upload_translate_per_lang: parse_or("CREDITS_UPLOAD_TRANSLATE_PER_LANG", 0.005f64),
        }
    }

    /// Defaults for tests (no env reads).
    #[doc(hidden)]
    pub fn test_default() -> Self {
        Self {
            report_model: "openai/gpt-oss-120b".into(),
            fallback_model: "openai/gpt-oss-20b".into(),
            report_base: 0.05,
            report_per_minute: 0.002,
            sentiment_base: 0.05,
            sentiment_per_participant: 0.01,
            sentiment_per_minute: 0.002,
            email_draft: 0.02,
            suggestions_per_minute: 0.005,
            suggestions_interval_secs: 15,
            quiz_base: 0.03,
            quiz_per_question: 0.01,
            correction_base: 0.05,
            correction_per_event: 0.001,
            upload_translate_base: 0.01,
            upload_translate_per_lang: 0.005,
        }
    }
}

impl ResendConfig {
    fn from_env() -> Self {
        Self {
            api_key: env::var("RESEND_API_KEY").unwrap_or_default(),
            from_email: env::var("RESEND_FROM_EMAIL").unwrap_or_default(),
            from_name: env::var("RESEND_FROM_NAME").unwrap_or_default(),
        }
    }
}

impl StorageConfig {
    fn from_env() -> Self {
        Self {
            // Tolerate a trailing slash in the configured URL.
            supabase_url: env::var("SUPABASE_URL")
                .unwrap_or_default()
                .trim()
                .trim_end_matches('/')
                .to_string(),
            service_key: env::var("SUPABASE_SERVICE_KEY")
                .unwrap_or_default()
                .trim()
                .to_string(),
            bucket: env::var("SUPABASE_BUCKET")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "chat-files".to_string()),
            max_bytes: parse_or("SUPABASE_MAX_UPLOAD_BYTES", 5 * 1024 * 1024usize),
            signed_ttl_secs: parse_or("SUPABASE_SIGNED_URL_TTL_SECS", 24 * 60 * 60u64),
        }
    }
}

impl PricingConfig {
    fn from_env() -> Self {
        let cost_per_minute = parse_or("COST_PER_MINUTE", 0.008f64);
        let markup_percentage = parse_or("MARKUP_PERCENTAGE", 0.25f64);
        let (user_rate_per_minute, user_rate_per_second) =
            compute_rate(cost_per_minute, markup_percentage);
        let packages = env::var("CREDIT_PACKAGES")
            .ok()
            .map(|s| parse_packages(&s))
            .unwrap_or_default();
        Self {
            cost_per_minute,
            markup_percentage,
            user_rate_per_minute,
            user_rate_per_second,
            free_credits: parse_or("FREE_CREDITS", 2.0f64),
            low_balance_threshold: parse_or("LOW_BALANCE_THRESHOLD", 0.5f64),
            min_balance_to_join: parse_or("MIN_BALANCE_TO_JOIN", 0.05f64),
            usage_update_interval: parse_or("USAGE_UPDATE_INTERVAL", 5u64),
            packages,
        }
    }
}

/// Computed user rate: cost × (1 + markup), per minute and per second.
fn compute_rate(cost_per_minute: f64, markup: f64) -> (f64, f64) {
    let per_minute = cost_per_minute * (1.0 + markup);
    (per_minute, per_minute / 60.0)
}

/// Parse the `CREDIT_PACKAGES` JSON array; returns empty on malformed input.
fn parse_packages(json: &str) -> Vec<CreditPackage> {
    serde_json::from_str(json).unwrap_or_default()
}

/// Parse `CARTESIA_STT_MODEL_BY_LANG` (a JSON object `{ lang: model }`, keys lowercased to
/// base language codes). Absent or malformed → default to routing English to Cartesia's
/// fastest STT (`ink-2`); every other language falls back to the multilingual `stt_model`.
fn parse_stt_model_map(json: Option<&str>) -> HashMap<String, String> {
    json.and_then(|s| serde_json::from_str::<HashMap<String, String>>(s).ok())
        .map(|m| {
            m.into_iter()
                .map(|(k, v)| (k.to_lowercase(), v))
                .collect::<HashMap<_, _>>()
        })
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| HashMap::from([("en".to_string(), "ink-2".to_string())]))
}

fn present(name: &str) -> bool {
    env::var(name)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// Decode the base64 `GOOGLE_TOKEN_ENC_KEY` into exactly 32 bytes (XChaCha20-Poly1305
/// key). Returns `None` for empty/invalid/wrong-length input so the calendar flow
/// simply stays disabled rather than panicking at startup.
fn decode_enc_key(raw: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    if raw.is_empty() {
        return None;
    }
    base64::engine::general_purpose::STANDARD
        .decode(raw)
        .ok()
        .filter(|bytes| bytes.len() == 32)
}

/// A boolean feature flag from the environment. Truthy = `1`/`true`/`yes`/`on`
/// (case-insensitive); anything else, or unset, is `false`.
fn env_flag(name: &str) -> bool {
    env_flag_or(name, false)
}

/// Like [`env_flag`] but with an explicit default when the variable is **unset**. A
/// variable that IS set but non-truthy (e.g. `0`/`off`) is still `false`. Used for the
/// Standard tier, which defaults ON.
fn env_flag_or(name: &str, default: bool) -> bool {
    match env::var(name) {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => default,
    }
}

fn parse_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn require(name: &str) -> Result<String, String> {
    match env::var(name) {
        Ok(v) if !v.trim().is_empty() => Ok(v),
        _ => Err(format!(
            "missing required environment variable `{name}` (set it in server/.env)"
        )),
    }
}

impl Config {
    /// Build a billing-enabled config for tests — defaults for everything except
    /// the database URL, JWT secret, and free-credit grant. Exposed (doc-hidden)
    /// so the integration-test crate can construct billing state.
    #[doc(hidden)]
    pub fn test_with_billing(database_url: &str, jwt_secret: &str, free_credits: f64) -> Self {
        let (user_rate_per_minute, user_rate_per_second) = compute_rate(0.008, 0.25);
        Self {
            deepgram_key: "dummy".into(),
            groq_key: "dummy".into(),
            translation_model: "openai/gpt-oss-20b".into(),
            port: 0,
            allowed_origins: vec![],
            extension_origins: vec![],
            auto_detect_buffer_ms: 3000,
            billing: Some(BillingConfig {
                database_url: database_url.into(),
                google_client_id: "test-client".into(),
                google_client_secret: String::new(),
                google_token_enc_key: None,
                google_calendar_scopes:
                    "openid email profile https://www.googleapis.com/auth/calendar.events".into(),
                jwt_secret: jwt_secret.into(),
                jwt_expiry_hours: 168,
                stripe_secret_key: String::new(),
                stripe_webhook_secret: String::new(),
                stripe_success_url: String::new(),
                stripe_cancel_url: String::new(),
                guest_max_minutes: None,
                admin_api_secret: Some("test-admin-secret".into()),
                pricing: PricingConfig {
                    cost_per_minute: 0.008,
                    markup_percentage: 0.25,
                    user_rate_per_minute,
                    user_rate_per_second,
                    free_credits,
                    low_balance_threshold: 0.5,
                    min_balance_to_join: 0.05,
                    usage_update_interval: 5,
                    packages: vec![],
                },
                glossary_max_entries: 200,
                ai: AiConfig::test_default(),
                org_billing: None,
            }),
            resend: None,
            storage: None,
            turn: None,
            turn_restricted: None,
            bug_report_to: "test@example.com".into(),
            app_base_url: "https://app.voxtranslate.app".into(),
            dashboard_base_url: "https://dashboard.voxtranslate.app".into(),
            push: None,
            business_member_limit: 20,
            retention_sweep_enabled: false,
            retention_sweep_interval_secs: 21_600,
            retention_sweep_batch: 200,
            openai: None,
            google: None,
            cartesia: None,
            qwen: QwenConfig {
                api_key: "dummy".into(),
                ..Default::default()
            },
            standard_enabled: true,
            pro_enabled: false,
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
}

// --- Redacting Debug impls (spec: secrets must never reach logs) ------------
// Hand-written instead of derived so an accidental `debug!("{config:?}")`, a
// `dbg!`, or a panic that formats a config struct can never dump API keys, JWT
// secrets, Stripe keys, or TURN credentials. Each impl prints only innocuous
// fields and marks the redacted remainder with `..`.

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("port", &self.port)
            .field("translation_model", &self.translation_model)
            .field("allowed_origins", &self.allowed_origins)
            .field("extension_origins", &self.extension_origins)
            .field("app_base_url", &self.app_base_url)
            .field("billing", &self.billing.is_some())
            .field("resend", &self.resend.is_some())
            .field("storage", &self.storage.is_some())
            .field("turn", &self.turn.is_some())
            .field("openai", &self.openai.is_some())
            .field("google", &self.google.is_some())
            .field("cartesia", &self.cartesia.is_some())
            // Standard's engine: the model id is innocuous, the key is not — never add it.
            .field("qwen_model", &self.qwen.model)
            .field("push", &self.push.is_some())
            .field("voice_assistant", &self.voice_assistant.is_some())
            .field("help_assistant", &self.help_assistant.is_some())
            .field("webinar", &self.webinar.is_some())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for VoiceAssistantConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // api_key is server-only and must never reach logs — redacted.
        f.debug_struct("VoiceAssistantConfig")
            .field("model", &self.model)
            .field("max_sessions", &self.max_sessions)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for HelpAssistantConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // api_key is server-only and must never reach logs — redacted.
        f.debug_struct("HelpAssistantConfig")
            .field("model", &self.model)
            .field("max_sessions", &self.max_sessions)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for BillingConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BillingConfig")
            .field("google_client_id", &self.google_client_id)
            .field("jwt_expiry_hours", &self.jwt_expiry_hours)
            .field("guest_max_minutes", &self.guest_max_minutes)
            .field(
                "admin_api_secret",
                &self.admin_api_secret.as_ref().map(|_| "***"),
            )
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for OpenAiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiConfig")
            .field("model", &self.model)
            .field("max_sessions", &self.max_sessions)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for EmbeddingsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddingsConfig")
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for GeminiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeminiConfig")
            .field("model", &self.model)
            .field("max_sessions", &self.max_sessions)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for QwenConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QwenConfig")
            .field("model", &self.model)
            .field("asr_model", &self.asr_model)
            .field("endpoint", &self.endpoint)
            // Endpoint only, never the key. Which REGION we ended up on is precisely
            // what an operator needs to read out of a log line.
            .field(
                "fallback_endpoint",
                &self.fallback.as_ref().map(|f| f.endpoint.as_str()),
            )
            .field("turn_detection", &self.turn_detection)
            .field("max_sessions", &self.max_sessions)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for CartesiaConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CartesiaConfig")
            .field("stt_model", &self.stt_model)
            .field("tts_model", &self.tts_model)
            .field("api_base", &self.api_base)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for TurnCred {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Every variant holds a secret — print only the variant name.
        let variant = match self {
            TurnCred::Secret { .. } => "Secret",
            TurnCred::Static { .. } => "Static",
            TurnCred::Cloudflare { .. } => "Cloudflare",
        };
        write!(f, "TurnCred::{variant}(..)")
    }
}

impl std::fmt::Debug for TurnConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnConfig")
            .field("urls", &self.urls)
            .field("cred", &self.cred) // redacted by TurnCred's own Debug
            .finish()
    }
}

impl std::fmt::Debug for StorageConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageConfig")
            .field("supabase_url", &self.supabase_url)
            .field("bucket", &self.bucket)
            .field("max_bytes", &self.max_bytes)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for OrgBillingConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrgBillingConfig")
            .field("success_url", &self.success_url)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ResendConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResendConfig")
            .field("from_email", &self.from_email)
            .field("from_name", &self.from_name)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for PushConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PushConfig")
            // vapid_public_key is public by design; the private key is omitted.
            .field("vapid_public_key", &self.vapid_public_key)
            .field("vapid_subject", &self.vapid_subject)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_math() {
        let (per_min, per_sec) = compute_rate(0.008, 0.25);
        assert!((per_min - 0.01).abs() < 1e-9);
        assert!((per_sec - 0.01 / 60.0).abs() < 1e-12);
    }

    #[test]
    fn packages_parse() {
        let json = r#"[{"id":"plus","name":"Plus","price_usd":15.0,"credits_usd":17.0,"stripe_price_id":"price_x"}]"#;
        let pkgs = parse_packages(json);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].id, "plus");
        assert_eq!(pkgs[0].stripe_price_id, "price_x");
        // stripe_price_id is never serialized to the client.
        let out = serde_json::to_string(&pkgs[0]).unwrap();
        assert!(!out.contains("stripe_price_id") && !out.contains("price_x"));
        assert!(parse_packages("not json").is_empty());
    }

    #[test]
    fn turn_cred_pick_prefers_cloudflare_then_secret_then_static_then_none() {
        // Cloudflare (key + token both present) wins over every other mode.
        match TurnCred::pick("cfkey", "cftoken", "s3cr3t", "user", "pass", 1800) {
            Some(TurnCred::Cloudflare {
                key_id,
                api_token,
                ttl_secs,
            }) => {
                assert_eq!(key_id, "cfkey");
                assert_eq!(api_token, "cftoken");
                assert_eq!(ttl_secs, 1800);
            }
            other => panic!("expected Cloudflare, got {other:?}"),
        }
        // Cloudflare needs BOTH halves; with only the key id it falls through to the
        // HMAC secret.
        match TurnCred::pick("cfkey", "", "s3cr3t", "user", "pass", 1800) {
            Some(TurnCred::Secret { secret, ttl_secs }) => {
                assert_eq!(secret, "s3cr3t");
                assert_eq!(ttl_secs, 1800);
            }
            other => panic!("expected Secret, got {other:?}"),
        }
        // No Cloudflare / secret → fall back to the managed relay's static creds.
        match TurnCred::pick("", "", "", "user", "pass", 3600) {
            Some(TurnCred::Static { username, password }) => {
                assert_eq!(username, "user");
                assert_eq!(password, "pass");
            }
            other => panic!("expected Static, got {other:?}"),
        }
        // Static needs BOTH halves; partial / empty config leaves TURN off.
        assert!(TurnCred::pick("", "", "", "user", "", 3600).is_none());
        assert!(TurnCred::pick("", "", "", "", "", 3600).is_none());
    }

    #[test]
    fn turn_restricted_profile_needs_urls_and_a_credential_no_cloudflare() {
        let urls = vec!["turns:relay.example.com:443?transport=tcp".to_string()];
        // Managed static creds → a Static profile that PRESERVES the :443 URLs (unlike
        // the Cloudflare default mode, which ignores TURN_URLS).
        match TurnConfig::restricted(urls.clone(), "", "user", "pass", 3600) {
            Some(TurnConfig {
                urls: u,
                cred: TurnCred::Static { username, password },
            }) => {
                assert_eq!(u, urls);
                assert_eq!(username, "user");
                assert_eq!(password, "pass");
            }
            other => panic!("expected Static restricted profile, got {other:?}"),
        }
        // A coturn HMAC secret wins over static — and Cloudflare is never chosen here.
        assert!(matches!(
            TurnConfig::restricted(urls.clone(), "s3cr3t", "user", "pass", 900),
            Some(TurnConfig {
                cred: TurnCred::Secret { .. },
                ..
            })
        ));
        // No URLs → no profile, even with valid creds (there'd be nothing to relay through).
        assert!(TurnConfig::restricted(vec![], "", "user", "pass", 3600).is_none());
        // URLs but no usable credential → no profile.
        assert!(TurnConfig::restricted(urls, "", "user", "", 3600).is_none());
    }

    #[test]
    fn env_flag_or_uses_default_when_unset() {
        // Reads a guaranteed-unset var (no env mutation → no race): the default wins.
        assert!(env_flag_or("VOX_UNSET_FLAG_FOR_TEST_ZZZ", true));
        assert!(!env_flag_or("VOX_UNSET_FLAG_FOR_TEST_ZZZ", false));
    }

    #[test]
    fn cartesia_config_from_env_defaults_and_urls() {
        // `from_env` reads process-global env. Assert only the hardcoded defaults +
        // derived URLs that hold regardless of the ambient environment (no env mutation,
        // so this can't race other tests in this binary).
        let cfg = CartesiaConfig::from_env();
        // Endpoints + version default to the Cartesia globals when unset.
        assert!(cfg.stt_endpoint.starts_with("wss://"));
        assert!(cfg.tts_endpoint.starts_with("wss://"));
        assert!(!cfg.version.is_empty());
        // URL builders compose off the REST base without a double slash.
        assert!(cfg.access_token_url().ends_with("/access-token"));
        assert!(cfg.clone_voice_url().ends_with("/voices/clone"));
        assert!(!cfg.access_token_url().contains("//access-token"));
        // STT model map defaults to English→ink-2 (fastest), others fall back to stt_model.
        assert_eq!(
            cfg.stt_model_by_lang.get("en").map(String::as_str),
            Some("ink-2")
        );
    }

    #[test]
    fn stt_model_map_defaults_and_overrides() {
        // Default (absent): English → ink-2, other languages absent (→ multilingual fallback).
        let def = parse_stt_model_map(None);
        assert_eq!(def.get("en").map(String::as_str), Some("ink-2"));
        assert!(!def.contains_key("it"));
        // Malformed JSON → the same default.
        assert_eq!(parse_stt_model_map(Some("not json")), def);
        assert_eq!(parse_stt_model_map(Some("{}")), def);
        // Valid override, keys lowercased to base codes.
        let m = parse_stt_model_map(Some(r#"{"EN":"ink-2","de":"ink-de"}"#));
        assert_eq!(m.get("en").map(String::as_str), Some("ink-2"));
        assert_eq!(m.get("de").map(String::as_str), Some("ink-de"));
    }

    // NOTE: `Config::from_env()` reads process-global env, so its guest-vs-billing
    // detection is tested in a *separate* binary (`tests/config_env.rs`) — mutating
    // `DATABASE_URL` here would race the DB-gated tests in this binary.
}
