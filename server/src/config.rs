//! Environment configuration, loaded from `.env` via dotenvy.
//!
//! Auth/billing is **optional**: it activates only when `DATABASE_URL`,
//! `GOOGLE_CLIENT_ID` and `JWT_SECRET` are all set. Otherwise the server runs in
//! guest-only mode (no accounts, no metering) — the original behavior.

use std::env;

use serde::{Deserialize, Serialize};

/// Runtime configuration for the server.
#[derive(Debug, Clone)]
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
    /// OpenAI GPT-Realtime-Translate "Pro" engine (spec 0093). Present only when the
    /// `OPENAI_PRO` rollout flag is truthy AND `OPENAI_API_KEY` is set — registered (and
    /// shown in the selector) iff this is `Some`, so the tier ships dark behind the flag.
    pub openai: Option<OpenAiConfig>,
    /// Google Gemini 3.5 Live Translate "Premium" engine (spec 0100). Present only when
    /// the `GEMINI_PREMIUM` rollout flag is truthy AND `GOOGLE_AI_API_KEY` is set —
    /// registered iff this is `Some`, so it ships dark behind the flag.
    pub google: Option<GeminiConfig>,
    /// Soniox "Enhanced" engine (spec 0101) — the client-direct tier between Standard
    /// and Pro. Present only when the rollout flag `SONIOX_ENHANCED` is truthy AND
    /// `SONIOX_API_KEY` is set; the engine is registered (and shown in the selector) iff
    /// this is `Some`, so the tier ships dark behind the flag (rollback = unset it).
    pub soniox: Option<SonioxConfig>,
    /// Whether the Standard (Deepgram + Groq) base tier is enabled (spec 0101 rollout
    /// flag `DEEPGRAM_STANDARD`, default ON). Standard is the registry's default and
    /// capacity-fallback engine, so it is force-registered even when this is `false`
    /// (with a warning) — the flag exists for symmetry with the optional tiers, not as a
    /// true kill switch.
    pub standard_enabled: bool,
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
}

/// OpenAI Realtime Translation credentials + pricing (spec 0093). All-or-nothing
/// like billing/Resend: activates only when `OPENAI_API_KEY` is present.
#[derive(Debug, Clone)]
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

/// Gemini Live Translate credentials + pricing (spec 0100). All-or-nothing like
/// the OpenAI engine: activates only when `GOOGLE_AI_API_KEY` is present.
#[derive(Debug, Clone)]
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

/// Soniox server→Soniox auth endpoint: mints scoped, single-use, expiring temporary
/// keys the browser then uses to connect DIRECTLY to Soniox (spec 0101). The raw
/// `SONIOX_API_KEY` never leaves the server — only the minted temp keys reach a client.
pub const SONIOX_TEMP_KEY_URL: &str = "https://api.soniox.com/v1/auth/temporary-api-key";

/// Default Soniox real-time endpoints (US region). Override per deployment via
/// `SONIOX_STT_ENDPOINT` / `SONIOX_TTS_ENDPOINT`.
const SONIOX_DEFAULT_STT_ENDPOINT: &str = "wss://stt-rt.soniox.com/transcribe-websocket";
const SONIOX_DEFAULT_TTS_ENDPOINT: &str = "wss://tts-rt.soniox.com/tts-websocket";

/// A Soniox data-residency region (spec 0101, data-residency guide). Only `Us` is live
/// today; `Eu`/`Jp` are scaffolded and fall back to `Us` until their regional projects
/// (and `SONIOX_API_KEY_EU` / `SONIOX_API_KEY_JP`) exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SonioxRegion {
    Us,
    Eu,
    Jp,
}

/// Per-region Soniox credentials + endpoints. The `api_key` is server-only and is never
/// serialized — the browser only receives the short-lived temp key minted from it.
#[derive(Debug, Clone)]
pub struct SonioxRegionConfig {
    pub api_key: String,
    pub stt_endpoint: String,
    pub tts_endpoint: String,
}

/// Soniox "Enhanced" credentials + pricing + region map (spec 0101). All-or-nothing like
/// the other engine configs, but additionally gated behind the `SONIOX_ENHANCED` flag.
/// Cost/markup follow the OpenAI/Gemini pattern exactly; nothing here touches billing
/// logic — the values flow into `EngineMetadata` and the existing meter does the rest.
#[derive(Debug, Clone)]
pub struct SonioxConfig {
    /// Real-time STT+translation model id (`SONIOX_STT_MODEL`, e.g. `stt-rt-v5`).
    pub stt_model: String,
    /// Raw server cost per minute, USD (`SONIOX_COST_PER_MINUTE`). Never serialized.
    pub cost_per_minute: f64,
    /// Markup as a FRACTION (0.85 = 85%). From `SONIOX_COST_MARKUP_PERCENT`, falling
    /// back to `ENGINE_DEFAULT_MARKUP_PERCENT`, divided by 100. Never serialized.
    pub markup: f64,
    /// Always populated (the live region).
    pub us: SonioxRegionConfig,
    /// Scaffolded; empty `api_key` until the EU project exists → falls back to US.
    pub eu: SonioxRegionConfig,
    /// Scaffolded; empty `api_key` until the JP project exists → falls back to US.
    pub jp: SonioxRegionConfig,
}

impl SonioxConfig {
    fn from_env() -> Self {
        // Markup is configured in PERCENT (e.g. 85); store it as a fraction. Prefer the
        // engine-specific override, then the global engine default, then 50%.
        let percent = env::var("SONIOX_COST_MARKUP_PERCENT")
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .or_else(|| {
                env::var("ENGINE_DEFAULT_MARKUP_PERCENT")
                    .ok()
                    .and_then(|v| v.trim().parse::<f64>().ok())
            })
            .unwrap_or(50.0);
        let stt_endpoint = env::var("SONIOX_STT_ENDPOINT")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| SONIOX_DEFAULT_STT_ENDPOINT.into());
        let tts_endpoint = env::var("SONIOX_TTS_ENDPOINT")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| SONIOX_DEFAULT_TTS_ENDPOINT.into());
        // EU/JP reuse the US endpoints for now (regional endpoints land with the
        // regional projects, spec 0101 TODO). They fall back to US whenever keyless.
        let region = |key: &str| SonioxRegionConfig {
            api_key: env::var(key).unwrap_or_default().trim().to_string(),
            stt_endpoint: stt_endpoint.clone(),
            tts_endpoint: tts_endpoint.clone(),
        };
        Self {
            stt_model: env::var("SONIOX_STT_MODEL")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "stt-rt-v5".into()),
            cost_per_minute: parse_or("SONIOX_COST_PER_MINUTE", 0.015f64),
            markup: percent / 100.0,
            us: region("SONIOX_API_KEY"),
            eu: region("SONIOX_API_KEY_EU"),
            jp: region("SONIOX_API_KEY_JP"),
        }
    }

    /// Resolve a region's credentials/endpoints, falling back to US when that region has
    /// no key configured yet (EU/JP are scaffolded; only US is live today).
    pub fn region(&self, region: SonioxRegion) -> &SonioxRegionConfig {
        let cfg = match region {
            SonioxRegion::Us => &self.us,
            SonioxRegion::Eu => &self.eu,
            SonioxRegion::Jp => &self.jp,
        };
        if cfg.api_key.trim().is_empty() {
            &self.us
        } else {
            cfg
        }
    }
}

/// Map a Cloudflare `CF-IPCountry` ISO code to a Soniox region (spec 0101). EU/Africa/
/// Middle East → EU, Asia/Oceania → JP, Americas/unknown → US. Region resolution then
/// falls back to US for any region without a configured key, so this is safe to enable
/// before the EU/JP projects exist. Pure → unit-tested.
pub fn soniox_region_for_country(cc: Option<&str>) -> SonioxRegion {
    // ISO-3166 alpha-2, uppercased. `XX`/`T1` (Tor) and unknowns default to US.
    let cc = match cc {
        Some(c) => c.trim().to_ascii_uppercase(),
        None => return SonioxRegion::Us,
    };
    // Europe + Africa + Middle East → EU project.
    const EU: &[&str] = &[
        "GB", "IE", "FR", "DE", "ES", "PT", "IT", "NL", "BE", "LU", "CH", "AT", "DK", "SE", "NO",
        "FI", "IS", "PL", "CZ", "SK", "HU", "RO", "BG", "GR", "HR", "SI", "RS", "BA", "ME", "MK",
        "AL", "EE", "LV", "LT", "UA", "BY", "MD", "RU", "TR", "CY", "MT", // Middle East
        "IL", "PS", "JO", "LB", "SY", "IQ", "SA", "AE", "QA", "BH", "KW", "OM", "YE", "IR",
        // Africa
        "MA", "DZ", "TN", "LY", "EG", "SD", "NG", "GH", "CI", "SN", "ET", "KE", "TZ", "UG", "ZA",
        "ZW", "ZM", "AO", "MZ", "CM", "CD", "CG",
    ];
    // Asia + Oceania → JP project.
    const JP: &[&str] = &[
        "JP", "KR", "CN", "HK", "MO", "TW", "MN", "IN", "PK", "BD", "LK", "NP", "TH", "VN", "LA",
        "KH", "MM", "MY", "SG", "ID", "PH", "BN", "TL", "AU", "NZ", "FJ", "PG", "KZ", "UZ", "TM",
        "KG", "TJ", "AF",
    ];
    if EU.contains(&cc.as_str()) {
        SonioxRegion::Eu
    } else if JP.contains(&cc.as_str()) {
        SonioxRegion::Jp
    } else {
        SonioxRegion::Us
    }
}

/// How `/api/ice` authenticates a client to the TURN relay (spec 0026 / 0059).
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
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
}

/// Supabase Storage credentials for chat file upload (spec 0018). All-or-nothing
/// like billing — the feature activates only when both URL and key are present.
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
pub struct BillingConfig {
    pub database_url: String,
    pub google_client_id: String,
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
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
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
        let deepgram_key = require("DEEPGRAM_API_KEY")?;
        let groq_key = require("GROQ_API_KEY")?;
        let translation_model = env::var("GROQ_TRANSLATION_MODEL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "openai/gpt-oss-20b".into());
        let port = parse_or("PORT", 3001u16);
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

        // Premium engine — Gemini Live Translate (spec 0100): `GEMINI_PREMIUM` + key.
        let google = if env_flag("GEMINI_PREMIUM") && present("GOOGLE_AI_API_KEY") {
            Some(GeminiConfig::from_env())
        } else {
            None
        };

        // Enhanced engine — Soniox (spec 0101): `SONIOX_ENHANCED` + key.
        let soniox = if env_flag("SONIOX_ENHANCED") && present("SONIOX_API_KEY") {
            Some(SonioxConfig::from_env())
        } else {
            None
        };

        // Standard engine — Deepgram + Groq (the base tier). Same flag pattern for
        // symmetry, but defaults ON when unset: Standard is the registry's default /
        // capacity-fallback engine, so the server force-registers it even when this is
        // off (with a warning) — you can't actually leave the app without a default.
        let standard_enabled = env_flag_or("DEEPGRAM_STANDARD", true);

        Ok(Self {
            deepgram_key,
            groq_key,
            translation_model,
            port,
            allowed_origins,
            auto_detect_buffer_ms: parse_or("AUTO_DETECT_BUFFER_MS", 3000u64),
            billing,
            resend,
            storage,
            turn,
            bug_report_to: env::var("BUG_REPORT_TO")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "micio86dev@gmail.com".into()),
            app_base_url: env::var("APP_BASE_URL")
                .ok()
                .map(|s| s.trim().trim_end_matches('/').to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "https://voxtranslate.app".into()),
            dashboard_base_url: env::var("DASHBOARD_BASE_URL")
                .ok()
                .map(|s| s.trim().trim_end_matches('/').to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "https://dashboard.voxtranslate.app".into()),
            openai,
            google,
            soniox,
            standard_enabled,
            listener_pays: env_flag("LISTENER_PAYS"),
            language_first_ux: env_flag("LANGUAGE_FIRST_UX"),
        })
    }

    pub fn billing_enabled(&self) -> bool {
        self.billing.is_some()
    }
}

impl BillingConfig {
    fn from_env() -> Self {
        Self {
            database_url: env::var("DATABASE_URL").unwrap_or_default(),
            google_client_id: env::var("GOOGLE_CLIENT_ID").unwrap_or_default(),
            jwt_secret: env::var("JWT_SECRET").unwrap_or_default(),
            jwt_expiry_hours: parse_or("JWT_EXPIRY_HOURS", 168i64),
            stripe_secret_key: env::var("STRIPE_SECRET_KEY").unwrap_or_default(),
            stripe_webhook_secret: env::var("STRIPE_WEBHOOK_SECRET").unwrap_or_default(),
            stripe_success_url: env::var("STRIPE_SUCCESS_URL").unwrap_or_default(),
            stripe_cancel_url: env::var("STRIPE_CANCEL_URL").unwrap_or_default(),
            guest_max_minutes: env::var("GUEST_MAX_MINUTES")
                .ok()
                .and_then(|s| s.parse().ok()),
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

fn present(name: &str) -> bool {
    env::var(name)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
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
            auto_detect_buffer_ms: 3000,
            billing: Some(BillingConfig {
                database_url: database_url.into(),
                google_client_id: "test-client".into(),
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
            bug_report_to: "test@example.com".into(),
            app_base_url: "https://voxtranslate.app".into(),
            dashboard_base_url: "https://dashboard.voxtranslate.app".into(),
            openai: None,
            google: None,
            soniox: None,
            standard_enabled: true,
            listener_pays: false,
            language_first_ux: false,
        }
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
    fn env_flag_or_uses_default_when_unset() {
        // Reads a guaranteed-unset var (no env mutation → no race): the default wins.
        assert!(env_flag_or("VOX_UNSET_FLAG_FOR_TEST_ZZZ", true));
        assert!(!env_flag_or("VOX_UNSET_FLAG_FOR_TEST_ZZZ", false));
    }

    #[test]
    fn soniox_region_routing_and_fallback() {
        // Europe / Middle East / Africa → EU; Asia / Oceania → JP; Americas → US.
        assert_eq!(soniox_region_for_country(Some("DE")), SonioxRegion::Eu);
        assert_eq!(soniox_region_for_country(Some("ae")), SonioxRegion::Eu); // case-insensitive
        assert_eq!(soniox_region_for_country(Some("JP")), SonioxRegion::Jp);
        assert_eq!(soniox_region_for_country(Some("AU")), SonioxRegion::Jp);
        assert_eq!(soniox_region_for_country(Some("US")), SonioxRegion::Us);
        assert_eq!(soniox_region_for_country(Some("BR")), SonioxRegion::Us);
        // Unknown / Tor / missing default to US.
        assert_eq!(soniox_region_for_country(Some("XX")), SonioxRegion::Us);
        assert_eq!(soniox_region_for_country(None), SonioxRegion::Us);

        // A region with no key configured falls back to the US credentials.
        let cfg = SonioxConfig {
            stt_model: "stt-rt-v5".into(),
            cost_per_minute: 0.015,
            markup: 0.85,
            us: SonioxRegionConfig {
                api_key: "us-key".into(),
                stt_endpoint: "wss://stt".into(),
                tts_endpoint: "wss://tts".into(),
            },
            eu: SonioxRegionConfig {
                api_key: String::new(), // not provisioned yet
                stt_endpoint: "wss://stt".into(),
                tts_endpoint: "wss://tts".into(),
            },
            jp: SonioxRegionConfig {
                api_key: "jp-key".into(),
                stt_endpoint: "wss://stt".into(),
                tts_endpoint: "wss://tts".into(),
            },
        };
        assert_eq!(cfg.region(SonioxRegion::Eu).api_key, "us-key"); // keyless EU → US
        assert_eq!(cfg.region(SonioxRegion::Jp).api_key, "jp-key"); // keyed JP → JP
        assert_eq!(cfg.region(SonioxRegion::Us).api_key, "us-key");
    }

    // NOTE: `Config::from_env()` reads process-global env, so its guest-vs-billing
    // detection is tested in a *separate* binary (`tests/config_env.rs`) — mutating
    // `DATABASE_URL` here would race the DB-gated tests in this binary.
}
