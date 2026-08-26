//! Integration tests for the Dashboard Help Assistant WebSocket handler.
//!
//! Tests that don't need a live DB or OpenAI are unconditional.
//! Handler/DB tests are `#[ignore]`d — run them with:
//!   cargo test --test help_assistant_integration -- --ignored --nocapture
//!
//! The handler tests reference `business::help_assistant::ws_handler` which is
//! implemented in task 1.10. Until then some tests here compile but the handler
//! ones are exercised only through the Axum test client.

use voxtranslate_server::business::credits::help_assistant_minute_credits;
use voxtranslate_server::config::HelpAssistantConfig;
use voxtranslate_server::engine::help_assistant::{capacity_full_error, try_acquire};

use std::sync::Arc;
use tokio::sync::Semaphore;

// ---------------------------------------------------------------------------
// Config helper
// ---------------------------------------------------------------------------

fn test_cfg(cost_per_minute: f64, markup: f64, max_sessions: usize) -> HelpAssistantConfig {
    HelpAssistantConfig {
        api_key: "test-key".into(),
        model: "gpt-realtime-2.1".into(),
        cost_per_minute,
        markup,
        max_sessions,
    }
}

// ---------------------------------------------------------------------------
// Unit tests — no DB, no network
// ---------------------------------------------------------------------------

/// Any member role must be accepted (help assistant has NO role restriction).
/// This verifies the design decision: role_rank("member") = MEMBER = 1 is enough.
#[test]
fn any_role_rank_satisfies_help_assistant_gate() {
    use voxtranslate_server::business::{role_rank, MEMBER};
    // member rank (the lowest authenticated role) must be ≥ MEMBER.
    assert!(
        role_rank("member") >= MEMBER,
        "member should satisfy the help assistant gate (no role restriction)"
    );
    // admin and owner also pass.
    assert!(role_rank("admin") >= MEMBER);
    assert!(role_rank("owner") >= MEMBER);
    // unknown role does NOT pass.
    assert!(role_rank("guest") < MEMBER);
}

/// Semaphore full → try_acquire returns None.
#[tokio::test]
async fn semaphore_full_try_acquire_returns_none() {
    let sem = Arc::new(Semaphore::new(1));
    // Acquire the only permit so the semaphore is exhausted.
    let _permit = sem.clone().try_acquire_owned().unwrap();
    let result = try_acquire(&sem).await;
    assert!(
        result.is_none(),
        "try_acquire on a full semaphore must return None"
    );
}

/// capacity_full_error JSON has the expected shape.
#[test]
fn capacity_full_error_json_shape() {
    let json: serde_json::Value = serde_json::from_str(&capacity_full_error()).expect("valid JSON");
    assert_eq!(json["type"], "error");
    assert_eq!(json["code"], "capacity_full");
    assert!(
        json["message"].as_str().is_some_and(|m| !m.is_empty()),
        "capacity_full error must have a non-empty message"
    );
}

/// Semaphore with capacity=2: first two acquires succeed, third fails.
#[tokio::test]
async fn semaphore_cap_is_enforced() {
    let sem = Arc::new(Semaphore::new(2));
    let p1 = try_acquire(&sem).await;
    let p2 = try_acquire(&sem).await;
    let p3 = try_acquire(&sem).await;
    assert!(p1.is_some(), "first acquire should succeed");
    assert!(p2.is_some(), "second acquire should succeed");
    assert!(p3.is_none(), "third acquire on cap=2 semaphore should fail");
}

/// credits_formula: cost_per_minute × (1 + markup) × 100 ceiling.
/// Default params: ceil(0.18 × 1.25 × 100) = ceil(22.5) = 23.
#[test]
fn credits_formula_default() {
    let cfg = test_cfg(0.18, 0.25, 10);
    assert_eq!(help_assistant_minute_credits(&cfg), 23);
}

/// Triangulation: ceil(0.15 × 1.25 × 100) = ceil(18.75) = 19.
#[test]
fn credits_formula_triangulation() {
    let cfg = test_cfg(0.15, 0.25, 10);
    assert_eq!(help_assistant_minute_credits(&cfg), 19);
}

// ---------------------------------------------------------------------------
// DB integration tests (ignored by default — need local PG on :5433)
// ---------------------------------------------------------------------------

/// Member role → 101 Switching Protocols (WS upgrade accepted).
/// Non-member → 403.
/// No active subscription → 402.
/// Semaphore full → error JSON with code=capacity_full.
/// Feature flag disabled → 404 (route absent).
/// After session: interaction_kind = 'help_assistant' in DB.
#[ignore]
#[tokio::test]
async fn help_assistant_handler_db_integration() {
    // Placeholder — exercised by the hermetic runner (task 4.1).
    // Full implementation uses the same axum test client pattern as
    // `voice_assistant_integration.rs` + `business_schema.rs`.
    // Requires: local PG on :5433, migration 035 applied.
    todo!("implement after local DB runner is available (task 4.1)")
}

/// interaction_kind column has value 'help_assistant' after a completed session.
#[ignore]
#[tokio::test]
async fn interaction_kind_persisted_as_help_assistant() {
    todo!("implement after local DB runner is available (task 4.1)")
}
