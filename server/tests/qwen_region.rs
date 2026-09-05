//! Region-migration knobs for the Standard tier: the overridable realtime ASR model and
//! the optional SECOND Model Studio route (`QWEN_FALLBACK_*`).
//!
//! Both are read by `QwenConfig::from_env`, which mutates process-global env, so this
//! lives in its own integration binary rather than racing the other env-reading tests.

use voxtranslate_server::config::QwenConfig;

/// Every variable this file touches, cleared before and after each block so one
/// assertion can never inherit another's environment.
const VARS: [&str; 7] = [
    "QWEN_ASR_MODEL",
    "QWEN_REALTIME_ENDPOINT",
    "QWEN_FALLBACK_ENDPOINT",
    "QWEN_FALLBACK_API_KEY",
    "QWEN_FALLBACK_WORKSPACE_ID",
    "DASHSCOPE_API_KEY",
    "QWEN_API_KEY",
];

/// Serialises the whole file. `QwenConfig::from_env` reads process-global env, and
/// cargo runs the tests in this binary on parallel THREADS of one process — so without
/// this, one test's `set_var` is read by another's `from_env` and the failure looks like
/// a config bug instead of a test-harness one.
static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take the env lock and start from a clean slate. Recovers from poisoning so one
/// failing assertion doesn't cascade into five misleading ones.
fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    let guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
    clear();
    guard
}

fn clear() {
    for k in VARS {
        std::env::remove_var(k);
    }
}

#[test]
fn asr_model_defaults_to_the_shipped_value_and_is_overridable() {
    let _env = env_guard();

    // Unset ⇒ exactly what the hardcoded constant used to be. Existing deployments must
    // not notice this became configurable.
    assert_eq!(
        QwenConfig::from_env().asr_model,
        "qwen3-asr-flash-realtime",
        "default ASR model changed — existing deployments would silently switch model"
    );

    // Set ⇒ wins, trimmed, same treatment as QWEN_REALTIME_MODEL.
    std::env::set_var("QWEN_ASR_MODEL", "  qwen4-asr-flash-realtime  ");
    assert_eq!(QwenConfig::from_env().asr_model, "qwen4-asr-flash-realtime");

    // Blank ⇒ as good as unset, so an empty Railway variable can't dial model "".
    std::env::set_var("QWEN_ASR_MODEL", "   ");
    assert_eq!(QwenConfig::from_env().asr_model, "qwen3-asr-flash-realtime");

    clear();
}

#[test]
fn fallback_route_is_absent_unless_configured() {
    let _env = env_guard();
    assert!(
        QwenConfig::from_env().fallback.is_none(),
        "a server with no QWEN_FALLBACK_* must behave exactly as before"
    );
    clear();
}

#[test]
fn fallback_route_reads_endpoint_key_and_workspace() {
    let _env = env_guard();
    std::env::set_var("DASHSCOPE_API_KEY", "sk-primary");
    std::env::set_var(
        "QWEN_FALLBACK_ENDPOINT",
        "wss://dashscope-intl.aliyuncs.com/api-ws/v1/realtime",
    );
    std::env::set_var("QWEN_FALLBACK_API_KEY", "sk-singapore");
    std::env::set_var("QWEN_FALLBACK_WORKSPACE_ID", "llm-sg-1");

    let fb = QwenConfig::from_env()
        .fallback
        .expect("fallback configured");
    assert_eq!(
        fb.endpoint,
        "wss://dashscope-intl.aliyuncs.com/api-ws/v1/realtime"
    );
    assert_eq!(fb.api_key, "sk-singapore");
    assert_eq!(fb.workspace_id.as_deref(), Some("llm-sg-1"));

    clear();
}

#[test]
fn fallback_inherits_whichever_half_the_operator_left_out() {
    let _env = env_guard();
    std::env::set_var("DASHSCOPE_API_KEY", "sk-primary");
    std::env::set_var(
        "QWEN_REALTIME_ENDPOINT",
        "wss://eu.example/api-ws/v1/realtime",
    );

    // Endpoint only ⇒ same account, different host (the workspace-template case).
    std::env::set_var(
        "QWEN_FALLBACK_ENDPOINT",
        "wss://sg.example/api-ws/v1/realtime",
    );
    let fb = QwenConfig::from_env()
        .fallback
        .expect("endpoint alone arms the fallback");
    assert_eq!(fb.endpoint, "wss://sg.example/api-ws/v1/realtime");
    assert_eq!(fb.api_key, "sk-primary", "should inherit the primary key");
    std::env::remove_var("QWEN_FALLBACK_ENDPOINT");

    // Key only ⇒ same host, different credential. Setting a key and getting silence is
    // exactly the trap `qwen_api_key` exists to avoid, so it must arm the fallback too.
    std::env::set_var("QWEN_FALLBACK_API_KEY", "sk-singapore");
    let fb = QwenConfig::from_env()
        .fallback
        .expect("key alone arms the fallback");
    assert_eq!(fb.api_key, "sk-singapore");
    assert_eq!(
        fb.endpoint, "wss://eu.example/api-ws/v1/realtime",
        "should inherit the primary endpoint"
    );

    clear();
}

#[test]
fn fallback_never_carries_a_fallback_of_its_own() {
    let _env = env_guard();
    std::env::set_var("DASHSCOPE_API_KEY", "sk-primary");
    std::env::set_var(
        "QWEN_FALLBACK_ENDPOINT",
        "wss://sg.example/api-ws/v1/realtime",
    );

    let c = QwenConfig::from_env();
    let derived = c
        .fallback_config()
        .expect("fallback derives a dialable config");
    assert_eq!(derived.endpoint, "wss://sg.example/api-ws/v1/realtime");
    // Model ids, VAD and pricing are tier-wide, not regional — they must carry over.
    assert_eq!(derived.model, c.model);
    assert_eq!(derived.asr_model, c.asr_model);
    assert_eq!(derived.max_sessions, c.max_sessions);
    // …and the derived config must be a LEAF, or a retry loop could recurse forever.
    assert!(derived.fallback.is_none());

    clear();
}

#[test]
fn debug_never_prints_either_api_key() {
    let _env = env_guard();
    std::env::set_var("DASHSCOPE_API_KEY", "sk-primary-secret");
    std::env::set_var(
        "QWEN_FALLBACK_ENDPOINT",
        "wss://sg.example/api-ws/v1/realtime",
    );
    std::env::set_var("QWEN_FALLBACK_API_KEY", "sk-fallback-secret");

    let rendered = format!("{:?}", QwenConfig::from_env());
    assert!(
        !rendered.contains("sk-primary-secret"),
        "leaked primary key: {rendered}"
    );
    assert!(
        !rendered.contains("sk-fallback-secret"),
        "leaked fallback key: {rendered}"
    );
    assert!(
        rendered.contains("sg.example"),
        "the fallback endpoint SHOULD be visible — it is how we spot a silent region switch: {rendered}"
    );

    clear();
}
