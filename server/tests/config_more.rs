//! Extra `Config::from_env` coverage for the OPTIONAL sub-configs — the engine
//! tiers (OpenAI / Gemini / Cartesia), embeddings, B2B org billing, web-push, and
//! the TURN credential modes. These are all driven through the PUBLIC
//! `Config::from_env`, since each sub-config's own `from_env` is private.
//!
//! Like `config_env.rs`, this mutates process-global env — so it lives in its own
//! integration binary (its own process; it can't race the DB-gated tests in other
//! binaries). A module mutex serializes the env-mutating tests WITHIN this binary.

use std::sync::Mutex;

use voxtranslate_server::config::Config;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Lock the env mutex, tolerating a previous test's panic (poisoned lock).
fn lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Every optional env var any test here touches — cleared before each test so
/// leftovers from a prior test can't bleed in. The two required provider keys are
/// (re)set by `base_env`.
const OPTIONAL_VARS: &[&str] = &[
    "PORT",
    "ALLOWED_ORIGINS",
    "GROQ_TRANSLATION_MODEL",
    "AUTO_DETECT_BUFFER_MS",
    "APP_BASE_URL",
    "DASHBOARD_BASE_URL",
    "BUG_REPORT_TO",
    "ENGINE_DEFAULT_MARKUP_PERCENT",
    // engines
    "OPENAI_PRO",
    "OPENAI_API_KEY",
    "OPENAI_REALTIME_MODEL",
    "OPENAI_COST_PER_MINUTE",
    "OPENAI_COST_MARKUP_PERCENT",
    "OPENAI_REALTIME_MAX_SESSIONS",
    "OPENAI_VOICE",
    "EMBEDDINGS_MODEL",
    "EMBEDDINGS_BACKFILL_SECRET",
    "GEMINI_PREMIUM",
    "GOOGLE_AI_API_KEY",
    "GEMINI_LIVE_TRANSLATE_MODEL",
    "GEMINI_COST_PER_MINUTE",
    "GEMINI_COST_MARKUP_PERCENT",
    "GEMINI_LIVE_MAX_SESSIONS",
    "GEMINI_VOICE",
    "CARTESIA_ENHANCED",
    "CARTESIA_API_KEY",
    "CARTESIA_STT_MODEL",
    "CARTESIA_STT_MODEL_BY_LANG",
    "CARTESIA_TTS_MODEL",
    "CARTESIA_COST_PER_MINUTE",
    "CARTESIA_COST_MARKUP_PERCENT",
    "CARTESIA_VOICE_CLONING_ENABLED",
    "CARTESIA_DEFAULT_VOICE_ID",
    "CARTESIA_API_BASE",
    "CARTESIA_STT_ENDPOINT",
    "CARTESIA_TTS_ENDPOINT",
    "CARTESIA_VERSION",
    // billing + org billing + calendar
    "DATABASE_URL",
    "GOOGLE_CLIENT_ID",
    "JWT_SECRET",
    "GOOGLE_CLIENT_SECRET",
    "GOOGLE_TOKEN_ENC_KEY",
    "GOOGLE_CALENDAR_SCOPES",
    "JWT_EXPIRY_HOURS",
    "COST_PER_MINUTE",
    "MARKUP_PERCENTAGE",
    "CREDIT_PACKAGES",
    "GLOSSARY_MAX_ENTRIES",
    "ADMIN_API_SECRET",
    "GUEST_MAX_MINUTES",
    "ORG_STRIPE_WEBHOOK_SECRET",
    "ORG_STRIPE_SUCCESS_URL",
    "ORG_STRIPE_CANCEL_URL",
    "ORG_STRIPE_PORTAL_RETURN_URL",
    "ORG_CREDIT_UNIT_CENTS",
    "ORG_PRICE_BUSINESS_MONTHLY",
    "ORG_PRICE_BUSINESS_ANNUAL",
    "ORG_PRICE_ENTERPRISE_MONTHLY",
    "ORG_PRICE_ENTERPRISE_ANNUAL",
    "ORG_CREDITS_BUSINESS_MONTHLY",
    "ORG_CREDITS_ENTERPRISE_MONTHLY",
    // push
    "VAPID_PUBLIC_KEY",
    "VAPID_PRIVATE_KEY",
    "VAPID_SUBJECT",
    // turn
    "TURN_URLS",
    "TURN_SECRET",
    "TURN_USERNAME",
    "TURN_PASSWORD",
    "TURN_CLOUDFLARE_KEY_ID",
    "TURN_CLOUDFLARE_API_TOKEN",
    "TURN_TTL_SECS",
    // resend + storage
    "RESEND_API_KEY",
    "RESEND_FROM_EMAIL",
    "RESEND_FROM_NAME",
    "SUPABASE_URL",
    "SUPABASE_SERVICE_KEY",
    // misc flags
    "RETENTION_SWEEP_ENABLED",
    "LISTENER_PAYS",
    "LANGUAGE_FIRST_UX",
    "TRANSLATION_CACHE_ENABLED",
    "DRAGONFLY_PRIVATE_URL",
    "BENCH_SECRET",
];

/// Set the two required provider keys and clear every optional var.
fn base_env() {
    std::env::set_var("DEEPGRAM_API_KEY", "dk");
    std::env::set_var("GROQ_API_KEY", "gk");
    for k in OPTIONAL_VARS {
        std::env::remove_var(k);
    }
}

const ENC_KEY_B64: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

#[test]
fn engines_present_with_explicit_overrides() {
    let _g = lock();
    base_env();
    // Pro (OpenAI) — flag + key + every override.
    std::env::set_var("OPENAI_PRO", "true");
    std::env::set_var("OPENAI_API_KEY", "sk-openai");
    std::env::set_var("OPENAI_REALTIME_MODEL", "gpt-rt-x");
    std::env::set_var("OPENAI_COST_PER_MINUTE", "0.40");
    std::env::set_var("OPENAI_COST_MARKUP_PERCENT", "80");
    std::env::set_var("OPENAI_REALTIME_MAX_SESSIONS", "9");
    std::env::set_var("OPENAI_VOICE", "cedar");
    std::env::set_var("EMBEDDINGS_MODEL", "text-embedding-3-large");
    // Premium (Gemini).
    std::env::set_var("GEMINI_PREMIUM", "1");
    std::env::set_var("GOOGLE_AI_API_KEY", "g-ai");
    std::env::set_var("GEMINI_LIVE_TRANSLATE_MODEL", "gemini-live-x");
    std::env::set_var("GEMINI_COST_PER_MINUTE", "0.05");
    std::env::set_var("GEMINI_COST_MARKUP_PERCENT", "60");
    std::env::set_var("GEMINI_LIVE_MAX_SESSIONS", "4");
    std::env::set_var("GEMINI_VOICE", "Kore");
    // Enhanced (Cartesia) — overrides incl. endpoints + per-lang STT map + clone.
    std::env::set_var("CARTESIA_ENHANCED", "yes");
    std::env::set_var("CARTESIA_API_KEY", "sk-car");
    std::env::set_var("CARTESIA_STT_MODEL", "ink-whisper");
    std::env::set_var(
        "CARTESIA_STT_MODEL_BY_LANG",
        r#"{"EN":"ink-2","de":"ink-de"}"#,
    );
    std::env::set_var("CARTESIA_TTS_MODEL", "sonic-x");
    std::env::set_var("CARTESIA_COST_PER_MINUTE", "0.04");
    std::env::set_var("CARTESIA_COST_MARKUP_PERCENT", "85");
    std::env::set_var("CARTESIA_VOICE_CLONING_ENABLED", "on");
    std::env::set_var("CARTESIA_DEFAULT_VOICE_ID", "voice-123");
    std::env::set_var("CARTESIA_API_BASE", "https://car.example/");
    std::env::set_var("CARTESIA_STT_ENDPOINT", "wss://car.example/stt");
    std::env::set_var("CARTESIA_TTS_ENDPOINT", "wss://car.example/tts");
    std::env::set_var("CARTESIA_VERSION", "2099-01-01");

    let c = Config::from_env().expect("config builds");

    let o = c.openai.expect("openai present");
    assert_eq!(o.api_key, "sk-openai");
    assert_eq!(o.model, "gpt-rt-x");
    assert!((o.cost_per_minute - 0.40).abs() < 1e-9);
    assert!((o.markup - 0.80).abs() < 1e-9);
    assert_eq!(o.max_sessions, 9);
    assert_eq!(o.voice.as_deref(), Some("cedar"));

    // Embeddings is decoupled from the Pro flag but reuses the key.
    let e = c.embeddings.expect("embeddings present");
    assert_eq!(e.api_key, "sk-openai");
    assert_eq!(e.model, "text-embedding-3-large");

    let g = c.google.expect("gemini present");
    assert_eq!(g.model, "gemini-live-x");
    assert!((g.markup - 0.60).abs() < 1e-9);
    assert_eq!(g.max_sessions, 4);
    assert_eq!(g.voice.as_deref(), Some("Kore"));

    let ca = c.cartesia.expect("cartesia present");
    assert_eq!(ca.tts_model, "sonic-x");
    assert!((ca.markup - 0.85).abs() < 1e-9);
    assert!(ca.voice_cloning_enabled);
    assert_eq!(ca.default_voice_id.as_deref(), Some("voice-123"));
    assert_eq!(
        ca.stt_model_by_lang.get("de").map(String::as_str),
        Some("ink-de")
    );
    // Endpoint overrides flow through to the URL builders (trailing slash trimmed).
    assert_eq!(ca.access_token_url(), "https://car.example/access-token");
    assert_eq!(ca.clone_voice_url(), "https://car.example/voices/clone");
    assert_eq!(ca.version, "2099-01-01");

    base_env();
}

#[test]
fn engine_markup_falls_back_to_global_default() {
    let _g = lock();
    base_env();
    // No engine-specific markup → the ENGINE_DEFAULT_MARKUP_PERCENT branch is taken.
    std::env::set_var("ENGINE_DEFAULT_MARKUP_PERCENT", "30");
    std::env::set_var("OPENAI_PRO", "on");
    std::env::set_var("OPENAI_API_KEY", "k");
    std::env::set_var("GEMINI_PREMIUM", "on");
    std::env::set_var("GOOGLE_AI_API_KEY", "k");
    std::env::set_var("CARTESIA_ENHANCED", "on");
    std::env::set_var("CARTESIA_API_KEY", "k");

    let c = Config::from_env().unwrap();
    assert!((c.openai.unwrap().markup - 0.30).abs() < 1e-9);
    assert!((c.google.unwrap().markup - 0.30).abs() < 1e-9);
    assert!((c.cartesia.unwrap().markup - 0.30).abs() < 1e-9);
    base_env();
}

#[test]
fn engines_absent_when_flag_off_even_with_key() {
    let _g = lock();
    base_env();
    // Keys set, but the rollout flags are off → the optional engines stay None.
    std::env::set_var("OPENAI_API_KEY", "k"); // (embeddings WILL be Some — decoupled)
    std::env::set_var("GOOGLE_AI_API_KEY", "k");
    std::env::set_var("CARTESIA_API_KEY", "k");
    let c = Config::from_env().unwrap();
    assert!(c.openai.is_none());
    assert!(c.google.is_none());
    assert!(c.cartesia.is_none());
    assert!(
        c.embeddings.is_some(),
        "embeddings ride the key, not OPENAI_PRO"
    );
    base_env();
}

#[test]
fn billing_org_billing_and_calendar() {
    let _g = lock();
    base_env();
    std::env::set_var("DATABASE_URL", "postgres://x");
    std::env::set_var("GOOGLE_CLIENT_ID", "gid");
    std::env::set_var("JWT_SECRET", "secret");
    std::env::set_var("GOOGLE_CLIENT_SECRET", "gsecret");
    std::env::set_var("GOOGLE_TOKEN_ENC_KEY", ENC_KEY_B64);
    std::env::set_var(
        "CREDIT_PACKAGES",
        r#"[{"id":"p","name":"P","price_usd":10,"credits_usd":12,"stripe_price_id":"price_p"}]"#,
    );
    std::env::set_var("GUEST_MAX_MINUTES", "30");
    std::env::set_var("ADMIN_API_SECRET", "admin");
    // org billing
    std::env::set_var("ORG_STRIPE_WEBHOOK_SECRET", "whsec");
    std::env::set_var("ORG_PRICE_BUSINESS_MONTHLY", "price_bm");
    std::env::set_var("ORG_PRICE_BUSINESS_ANNUAL", "price_ba");
    std::env::set_var("ORG_PRICE_ENTERPRISE_MONTHLY", "price_em");
    std::env::set_var("ORG_PRICE_ENTERPRISE_ANNUAL", "price_ea");
    std::env::set_var("ORG_CREDITS_BUSINESS_MONTHLY", "1500");
    std::env::set_var("ORG_CREDITS_ENTERPRISE_MONTHLY", "6000");
    std::env::set_var("ORG_CREDIT_UNIT_CENTS", "150");

    let c = Config::from_env().unwrap();
    let b = c.billing.as_ref().expect("billing");
    assert_eq!(b.guest_max_minutes, Some(30));
    assert_eq!(b.admin_api_secret.as_deref(), Some("admin"));
    assert!(b.google_token_enc_key.is_some());
    assert!(b.calendar_enabled(), "secret + valid enc key → calendar on");
    assert_eq!(b.pricing.packages.len(), 1);
    assert_eq!(b.pricing.packages[0].stripe_price_id, "price_p");

    let ob = b.org_billing.as_ref().expect("org billing");
    assert_eq!(ob.credit_unit_amount_cents, 150);
    assert_eq!(ob.price_id("business", "month"), Some("price_bm"));
    assert_eq!(ob.price_id("enterprise", "year"), Some("price_ea"));
    assert_eq!(ob.price_id("business", "weekly"), None);
    assert_eq!(
        ob.plan_interval_for_price("price_em"),
        Some(("enterprise", "month"))
    );
    assert_eq!(
        ob.plan_interval_for_price("price_ba"),
        Some(("business", "year"))
    );
    assert_eq!(ob.plan_interval_for_price("nope"), None);
    assert_eq!(ob.plan_interval_for_price(""), None);
    // monthly vs annual (12×) grants per plan.
    assert_eq!(ob.grant_credits("business", "month"), 1500);
    assert_eq!(ob.grant_credits("business", "year"), 1500 * 12);
    assert_eq!(ob.grant_credits("enterprise", "month"), 6000);
    assert_eq!(ob.grant_credits("enterprise", "year"), 6000 * 12);
    base_env();
}

#[test]
fn calendar_disabled_without_valid_enc_key() {
    let _g = lock();
    base_env();
    std::env::set_var("DATABASE_URL", "postgres://x");
    std::env::set_var("GOOGLE_CLIENT_ID", "gid");
    std::env::set_var("JWT_SECRET", "secret");
    std::env::set_var("GOOGLE_CLIENT_SECRET", "gsecret");
    // A wrong-length / invalid key decodes to None → calendar stays off.
    std::env::set_var("GOOGLE_TOKEN_ENC_KEY", "not-base64-or-wrong-length");
    let c = Config::from_env().unwrap();
    let b = c.billing.as_ref().unwrap();
    assert!(b.google_token_enc_key.is_none());
    assert!(!b.calendar_enabled());
    base_env();
}

#[test]
fn push_config_present_with_subject_override() {
    let _g = lock();
    base_env();
    std::env::set_var("VAPID_PUBLIC_KEY", "pub-key");
    std::env::set_var("VAPID_PRIVATE_KEY", "priv-key");
    std::env::set_var("VAPID_SUBJECT", "mailto:ops@vox.example");
    let c = Config::from_env().unwrap();
    let p = c.push.expect("push present");
    assert_eq!(p.vapid_public_key, "pub-key");
    assert_eq!(p.vapid_subject, "mailto:ops@vox.example");

    // Missing one key → push disabled; default subject when only keys set.
    std::env::remove_var("VAPID_PRIVATE_KEY");
    assert!(Config::from_env().unwrap().push.is_none());
    std::env::set_var("VAPID_PRIVATE_KEY", "priv-key");
    std::env::remove_var("VAPID_SUBJECT");
    let dflt = Config::from_env().unwrap().push.unwrap();
    assert!(dflt.vapid_subject.starts_with("mailto:"));
    base_env();
}

#[test]
fn turn_secret_static_and_cloudflare_modes() {
    let _g = lock();
    base_env();

    // Secret (coturn) mode: URLs + TURN_SECRET.
    std::env::set_var(
        "TURN_URLS",
        "turn:relay.example:3478, ,turn:relay2.example:3478",
    );
    std::env::set_var("TURN_SECRET", "hmac-secret");
    std::env::set_var("TURN_TTL_SECS", "1200");
    let t = Config::from_env().unwrap().turn.expect("turn present");
    assert_eq!(t.urls.len(), 2, "blank entries are filtered out");

    // Static (managed relay) mode: URLs + username/password, no secret.
    std::env::remove_var("TURN_SECRET");
    std::env::set_var("TURN_USERNAME", "u");
    std::env::set_var("TURN_PASSWORD", "p");
    assert!(Config::from_env().unwrap().turn.is_some());

    // Cloudflare mode: key id + token, NO TURN_URLS needed.
    base_env();
    std::env::set_var("TURN_CLOUDFLARE_KEY_ID", "cf-key");
    std::env::set_var("TURN_CLOUDFLARE_API_TOKEN", "cf-token");
    let cf = Config::from_env()
        .unwrap()
        .turn
        .expect("cloudflare turn present");
    assert!(cf.urls.is_empty());

    // URLs present but no usable credential → TURN stays off.
    base_env();
    std::env::set_var("TURN_URLS", "turn:relay.example:3478");
    assert!(Config::from_env().unwrap().turn.is_none());
    base_env();
}

#[test]
fn core_overrides_origins_and_flags() {
    let _g = lock();
    base_env();
    std::env::set_var("PORT", "8080");
    std::env::set_var("ALLOWED_ORIGINS", "https://a.example, ,https://b.example");
    std::env::set_var("APP_BASE_URL", "https://app.example/");
    std::env::set_var("DASHBOARD_BASE_URL", "https://dash.example/");
    std::env::set_var("GROQ_TRANSLATION_MODEL", "my-model");
    std::env::set_var("LISTENER_PAYS", "true");
    std::env::set_var("LANGUAGE_FIRST_UX", "true");
    std::env::set_var("TRANSLATION_CACHE_ENABLED", "true");
    std::env::set_var("DRAGONFLY_PRIVATE_URL", "redis://dragon:6379");
    std::env::set_var("BENCH_SECRET", "bench");
    std::env::set_var("RETENTION_SWEEP_ENABLED", "true");

    let c = Config::from_env().unwrap();
    assert_eq!(c.port, 8080);
    assert_eq!(
        c.allowed_origins,
        vec!["https://a.example", "https://b.example"]
    );
    assert_eq!(c.app_base_url, "https://app.example"); // trailing slash trimmed
    assert_eq!(c.dashboard_base_url, "https://dash.example");
    assert_eq!(c.translation_model, "my-model");
    assert!(c.listener_pays);
    assert!(c.language_first_ux);
    assert!(c.cache_enabled);
    assert_eq!(c.dragonfly_url.as_deref(), Some("redis://dragon:6379"));
    assert_eq!(c.bench_secret.as_deref(), Some("bench"));
    assert!(c.retention_sweep_enabled);
    base_env();
}
