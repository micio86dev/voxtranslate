//! RED tests for `help_assistant_minute_credits` markup math.
//!
//! These reference `voxtranslate_server::business::credits::help_assistant_minute_credits`
//! which does not exist yet. They will fail to compile until task 1.8 adds the function.

use voxtranslate_server::business::credits::help_assistant_minute_credits;
use voxtranslate_server::config::HelpAssistantConfig;

/// Build a config for testing without touching process env.
fn make_ha_config(
    cost_per_minute: f64,
    markup_fraction: f64,
    max_sessions: usize,
) -> HelpAssistantConfig {
    HelpAssistantConfig {
        api_key: "sk-test".to_string(),
        model: "gpt-realtime-2.1".to_string(),
        cost_per_minute,
        markup: markup_fraction,
        max_sessions,
    }
}

// ---- Default parameters (0.18 cost, 25% markup) ----------------------------

/// Formula: ceil(cost_per_minute × (1 + markup) × 100).
/// Default: ceil(0.18 × 1.25 × 100) = ceil(22.5) = 23.
#[test]
fn help_assistant_minute_credits_default_params() {
    let cfg = make_ha_config(0.18, 0.25, 10);
    let credits = help_assistant_minute_credits(&cfg);
    assert_eq!(
        credits, 23,
        "ceil(0.18 × 1.25 × 100) = ceil(22.5) = 23, got {}",
        credits
    );
}

/// Triangulation: different cost+markup produces expected ceiling.
/// ceil(0.20 × 1.30 × 100) = ceil(26.0) = 26.
#[test]
fn help_assistant_minute_credits_higher_markup() {
    let cfg = make_ha_config(0.20, 0.30, 10);
    let credits = help_assistant_minute_credits(&cfg);
    assert_eq!(
        credits, 26,
        "ceil(0.20 × 1.30 × 100) = ceil(26.0) = 26, got {}",
        credits
    );
}

/// Ceiling must round UP (not truncate): ceil(0.10 × 1.25 × 100) = ceil(12.5) = 13.
#[test]
fn help_assistant_minute_credits_ceiling_rounds_up() {
    let cfg = make_ha_config(0.10, 0.25, 10);
    let credits = help_assistant_minute_credits(&cfg);
    assert_eq!(
        credits, 13,
        "ceil(0.10 × 1.25 × 100) = ceil(12.5) = 13, got {}",
        credits
    );
}

/// Verify the ticks-per-60-seconds formula used in the relay:
/// credits_per_tick = credits_per_minute × TICK_SECS / 60.
/// With TICK_SECS=10: 23 × 10 / 60 = 3 (integer floor), then max(3, 1) = 3.
#[test]
fn help_assistant_tick_credits_formula() {
    let cfg = make_ha_config(0.18, 0.25, 10);
    let credits_per_minute = help_assistant_minute_credits(&cfg);
    const TICK_SECS: i32 = 10;
    let credits_per_tick = (credits_per_minute * TICK_SECS / 60).max(1);
    assert_eq!(
        credits_per_tick, 3,
        "23 credits/min × 10s / 60 = 3, got {}",
        credits_per_tick
    );
}
