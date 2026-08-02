//! RED tests for HelpAssistantConfig — written BEFORE the implementation.
//!
//! These tests reference `Config::help_assistant` and `HelpAssistantConfig` which
//! do not yet exist. They will fail to compile (RED) until task 1.3 implements them.
//!
//! Like `config_more.rs`, this mutates process-global env and must run in its own
//! binary to avoid racing other env-mutating tests.

use std::sync::Mutex;

use voxtranslate_server::config::Config;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Clear all vars this test file might set/read.
const OPTIONAL_VARS: &[&str] = &[
    "HELP_ASSISTANT_ENABLED",
    "HELP_ASSISTANT_MODEL",
    "HELP_ASSISTANT_COST_PER_MINUTE",
    "HELP_ASSISTANT_MARKUP_PERCENT",
    "HELP_ASSISTANT_MAX_SESSIONS",
    "OPENAI_API_KEY",
    // must keep required vars stable
    "PORT",
    "ALLOWED_ORIGINS",
    "GROQ_TRANSLATION_MODEL",
    "AUTO_DETECT_BUFFER_MS",
    "APP_BASE_URL",
    "DASHBOARD_BASE_URL",
    "BUG_REPORT_TO",
    // other optional engines — avoid bleed-in
    "VOICE_ASSISTANT_ENABLED",
    "OPENAI_PRO",
    "GEMINI_PREMIUM",
    "GOOGLE_AI_API_KEY",
    "CARTESIA_ENHANCED",
    "CARTESIA_API_KEY",
    "DATABASE_URL",
    "GOOGLE_CLIENT_ID",
    "JWT_SECRET",
    "TURN_URLS",
    "TURN_SECRET",
    "TURN_CLOUDFLARE_KEY_ID",
    "TURN_CLOUDFLARE_API_TOKEN",
    "RESEND_API_KEY",
    "RESEND_FROM_EMAIL",
    "RESEND_FROM_NAME",
    "SUPABASE_URL",
    "SUPABASE_SERVICE_KEY",
    "VAPID_PUBLIC_KEY",
    "VAPID_PRIVATE_KEY",
    "ORG_STRIPE_WEBHOOK_SECRET",
    "RETENTION_SWEEP_ENABLED",
    "LISTENER_PAYS",
    "LANGUAGE_FIRST_UX",
    "TRANSLATION_CACHE_ENABLED",
    "DRAGONFLY_PRIVATE_URL",
    "BENCH_SECRET",
    "ENGINE_DEFAULT_MARKUP_PERCENT",
    "EMBEDDINGS_MODEL",
    "EMBEDDINGS_BACKFILL_SECRET",
];

fn base_env() {
    // Qwen powers the Standard tier and is what `from_env` requires; Deepgram is now
    // optional (batch transcription only) but harmless to set.
    std::env::set_var("QWEN_API_KEY", "sk-test");
    std::env::set_var("DEEPGRAM_API_KEY", "dk");
    std::env::set_var("GROQ_API_KEY", "gk");
    for k in OPTIONAL_VARS {
        std::env::remove_var(k);
    }
}

// ---- Task 1.5 tests: HelpAssistantConfig from_env defaults ------------------

/// When HELP_ASSISTANT_ENABLED is unset (or false) → help_assistant is None.
#[test]
fn help_assistant_none_when_disabled() {
    let _g = lock();
    base_env();
    // Neither flag nor key → None.
    let c = Config::from_env().unwrap();
    assert!(
        c.help_assistant.is_none(),
        "help_assistant must be None when HELP_ASSISTANT_ENABLED is unset"
    );

    // Key present but flag off → still None.
    std::env::set_var("OPENAI_API_KEY", "sk-ha-key");
    let c = Config::from_env().unwrap();
    assert!(
        c.help_assistant.is_none(),
        "help_assistant must be None when HELP_ASSISTANT_ENABLED is falsy even with key"
    );
    base_env();
}

/// Flag on but no OPENAI_API_KEY → None.
#[test]
fn help_assistant_none_when_no_api_key() {
    let _g = lock();
    base_env();
    std::env::set_var("HELP_ASSISTANT_ENABLED", "true");
    // No OPENAI_API_KEY → None.
    let c = Config::from_env().unwrap();
    assert!(
        c.help_assistant.is_none(),
        "help_assistant must be None when OPENAI_API_KEY is absent"
    );
    base_env();
}

/// Flag on + key present → Some with all defaults applied.
#[test]
fn help_assistant_defaults() {
    let _g = lock();
    base_env();
    std::env::set_var("HELP_ASSISTANT_ENABLED", "true");
    std::env::set_var("OPENAI_API_KEY", "sk-ha-test");

    let c = Config::from_env().unwrap();
    let ha = c
        .help_assistant
        .expect("help_assistant must be Some when flag=true + key set");

    // Default model is gpt-realtime-2.
    assert_eq!(
        ha.model, "gpt-realtime-2",
        "default model should be gpt-realtime-2"
    );
    // Default cost per minute is 0.18.
    assert!(
        (ha.cost_per_minute - 0.18).abs() < 1e-9,
        "expected cost_per_minute 0.18, got {}",
        ha.cost_per_minute
    );
    // Default markup is 25% stored as fraction 0.25.
    assert!(
        (ha.markup - 0.25).abs() < 1e-9,
        "expected markup 0.25 (25%), got {}",
        ha.markup
    );
    // Default max_sessions is 10.
    assert_eq!(ha.max_sessions, 10, "expected max_sessions 10");
    // api_key captured from OPENAI_API_KEY.
    assert_eq!(ha.api_key, "sk-ha-test");
    base_env();
}

/// All five env vars present → parsed correctly.
#[test]
fn help_assistant_some_when_enabled() {
    let _g = lock();
    base_env();
    std::env::set_var("HELP_ASSISTANT_ENABLED", "1");
    std::env::set_var("OPENAI_API_KEY", "sk-ha-explicit");
    std::env::set_var("HELP_ASSISTANT_MODEL", "gpt-realtime-3.0");
    std::env::set_var("HELP_ASSISTANT_COST_PER_MINUTE", "0.20");
    std::env::set_var("HELP_ASSISTANT_MARKUP_PERCENT", "30");
    std::env::set_var("HELP_ASSISTANT_MAX_SESSIONS", "5");

    let c = Config::from_env().unwrap();
    let ha = c.help_assistant.expect("help_assistant present");

    assert_eq!(ha.api_key, "sk-ha-explicit");
    assert_eq!(ha.model, "gpt-realtime-3.0");
    assert!(
        (ha.cost_per_minute - 0.20).abs() < 1e-9,
        "cost_per_minute mismatch"
    );
    // 30 percent → stored as fraction 0.30.
    assert!(
        (ha.markup - 0.30).abs() < 1e-9,
        "markup should be 0.30 (30%), got {}",
        ha.markup
    );
    assert_eq!(ha.max_sessions, 5);
    base_env();
}

/// test_with_billing returns None for help_assistant (not enabled in the test
/// helper, just like voice_assistant).
#[test]
fn test_with_billing_returns_none_for_help_assistant() {
    let c = Config::test_with_billing("postgres://test", "test-jwt-secret-0123456789abcdef", 10.0);
    assert!(
        c.help_assistant.is_none(),
        "test_with_billing should not enable help_assistant"
    );
}
