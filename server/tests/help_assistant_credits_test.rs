//! What a Help Assistant session costs the organization, from config to credits.
//!
//! The chain is `HELP_ASSISTANT_COST_PER_MINUTE` and `HELP_ASSISTANT_MARKUP_PERCENT`
//! → `MinuteRateMeter` → whole credits off the org pool, at 100 credits = $1.
//!
//! It used to `ceil` a whole minute into credits and then integer-divide by the
//! ticks in a minute. Both steps rounded the customer's way — 18 credits charged
//! for a minute worth 22.5 — and the test that lived here asserted the result,
//! `23 × 10 / 60 = 3` credits a tick, as though it were correct. That is how a
//! 20% under-charge stayed put. These tests pin the PRICE, not the arithmetic
//! that used to approximate it.

use voxtranslate_server::business::credits::MinuteRateMeter;
use voxtranslate_server::config::HelpAssistantConfig;

/// The relay's credit tick, in seconds (`engine::help_assistant::TICK_SECS`).
const TICK_SECS: u64 = 10;

fn make_ha_config(cost_per_minute: f64, markup_fraction: f64) -> HelpAssistantConfig {
    HelpAssistantConfig {
        api_key: "sk-test".to_string(),
        model: "gpt-realtime-2.1".to_string(),
        cost_per_minute,
        markup: markup_fraction,
        max_sessions: 10,
    }
}

/// Drive the meter the way the relay does — once per tick, off the session
/// clock — and return the credits charged over `minutes`.
fn charge_for(cfg: &HelpAssistantConfig, minutes: u64) -> i32 {
    let mut meter = MinuteRateMeter::new(cfg.cost_per_minute, cfg.markup);
    let ticks = minutes * 60 / TICK_SECS;
    (1..=ticks).map(|t| meter.due(t * TICK_SECS)).sum()
}

// ---- Default parameters (0.18 cost, 25% markup) ----------------------------

/// $0.18 × 1.25 = $0.225 a minute = 22.5 credits. A minute charges the 22 whole
/// credits earned; the half is carried, neither rounded up nor dropped.
#[test]
fn a_minute_costs_what_the_config_says_it_costs() {
    assert_eq!(charge_for(&make_ha_config(0.18, 0.25), 1), 22);
}

/// The carried half is not forgiven: the second minute collects it, so two
/// minutes cost the full 45 credits rather than 44.
#[test]
fn the_carried_remainder_is_collected_not_forgiven() {
    assert_eq!(charge_for(&make_ha_config(0.18, 0.25), 2), 45);
}

// ---- Triangulation ---------------------------------------------------------

/// A price landing on a whole credit bills exactly, with nothing left over.
/// $0.20 × 1.30 = $0.26 a minute = 26 credits.
///
/// This is the case that killed the obvious implementation: deriving a fixed
/// per-tick charge means dividing $0.26 by six, which has no exact decimal form,
/// so the six ticks sum to just under the minute and quietly bill 25.
#[test]
fn a_whole_credit_price_carries_nothing() {
    assert_eq!(charge_for(&make_ha_config(0.20, 0.30), 1), 26);
    assert_eq!(charge_for(&make_ha_config(0.20, 0.30), 5), 130);
}

/// A cheaper tier follows the same rule — it is the price that governs, not the
/// number. $0.10 × 1.25 = $0.125 a minute = 12.5 credits.
#[test]
fn a_cheaper_tier_follows_the_same_rule() {
    assert_eq!(charge_for(&make_ha_config(0.10, 0.25), 1), 12);
    assert_eq!(charge_for(&make_ha_config(0.10, 0.25), 2), 25);
}

// ---- The regression --------------------------------------------------------

/// The old chain charged 18 credits for a minute worth 22.5, and nothing noticed
/// for as long as this file asserted that result.
#[test]
fn the_old_per_tick_arithmetic_under_charged_by_a_fifth() {
    let cfg = make_ha_config(0.18, 0.25);
    let old_ceiled_minute = (cfg.cost_per_minute * (1.0 + cfg.markup) * 100.0).ceil() as i64; // 23
    let old_per_tick = (old_ceiled_minute * TICK_SECS as i64 / 60).max(1); // 3
    let old_per_minute = (old_per_tick * 60 / TICK_SECS as i64) as i32; // 18

    assert_eq!(old_per_minute, 18);
    assert_eq!(charge_for(&cfg, 1), 22);
    assert!(
        charge_for(&cfg, 1) > old_per_minute,
        "the fix must charge more than the leak it replaced"
    );
}
