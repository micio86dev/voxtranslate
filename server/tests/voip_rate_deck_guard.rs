//! The boot check that says a VoIP deployment cannot price a call.
//!
//! This exists because production ran with `VOIP_ENABLED=true` and an empty `voip_rates`.
//! Every call was refused with `rate_unavailable`, the server booted clean, the logs were
//! quiet, and the first report came from a person trying to make a call. What is asserted
//! here is the three states the operator has to be able to tell apart — missing, expiring,
//! expired — because the fix differs and only the first is an emergency.
//!
//! DB-gated: skipped without `DATABASE_URL`.

use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use uuid::Uuid;
use voxtranslate_server::db::{self, Pool};
use voxtranslate_server::voip::service::rate_deck_health;

/// A provider id unique to each test: `voip_rates` is global and the suite runs in
/// parallel, so a shared id would measure another test's deck.
fn provider() -> String {
    format!("t-{}", Uuid::new_v4().simple())
}

async fn pool() -> Option<Pool> {
    let url = voxtranslate_server::db::test_database_url()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    Some(pool)
}

macro_rules! db {
    () => {
        match pool().await {
            Some(p) => p,
            None => {
                eprintln!("skipping — no DATABASE_URL");
                return;
            }
        }
    };
}

async fn seed(pool: &Pool, provider: &str, prefix: &str, age: Duration) {
    sqlx::query(
        "INSERT INTO voip_rates
            (provider, prefix, description, cost_per_minute, currency, number_type, fetched_at)
         VALUES ($1, $2, 'seeded', $3, 'USD', 'other', $4)",
    )
    .bind(provider)
    .bind(prefix)
    .bind(Decimal::new(61, 4))
    .bind(Utc::now() - age)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn an_empty_deck_is_reported_as_missing_not_as_zero_rows() {
    let pool = db!();
    // `None`, not `Some((0, …))`. "No deck" and "a deck with nothing in it" are the same
    // situation to an operator, and collapsing them means one fewer branch to get wrong.
    assert!(rate_deck_health(&pool, &provider())
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn a_fresh_deck_reports_its_size_and_age() {
    let pool = db!();
    let p = provider();
    seed(&pool, &p, "39", Duration::minutes(5)).await;
    seed(&pool, &p, "393", Duration::minutes(5)).await;

    let (rows, age) = rate_deck_health(&pool, &p).await.unwrap().expect("a deck");
    assert_eq!(rows, 2);
    assert!(age < Duration::hours(1), "age was {age}");
}

#[tokio::test]
async fn the_age_is_the_newest_row_not_the_oldest() {
    let pool = db!();
    let p = provider();
    // A deck imported in one transaction shares a timestamp, but a partially repaired one
    // does not — and what decides whether calls are refused is the row a lookup can still
    // use, which is the newest.
    seed(&pool, &p, "44", Duration::days(30)).await;
    seed(&pool, &p, "442", Duration::minutes(2)).await;

    let (rows, age) = rate_deck_health(&pool, &p).await.unwrap().expect("a deck");
    assert_eq!(rows, 2);
    assert!(age < Duration::hours(1), "took the oldest row: {age}");
}

#[tokio::test]
async fn an_expired_deck_is_visible_as_an_age_past_the_window() {
    let pool = db!();
    let p = provider();
    seed(&pool, &p, "1", Duration::hours(36)).await;

    let (_, age) = rate_deck_health(&pool, &p).await.unwrap().expect("a deck");
    // 36 h against the 24 h default: this is the exact state that refuses every call.
    assert!(age > Duration::hours(24), "age was {age}");
}

#[tokio::test]
async fn one_providers_deck_is_not_another_providers() {
    let pool = db!();
    let (a, b) = (provider(), provider());
    seed(&pool, &a, "39", Duration::minutes(1)).await;

    assert!(rate_deck_health(&pool, &a).await.unwrap().is_some());
    // Sharing a deck across providers would price calls from a carrier that never quoted
    // them — and would hide a missing deck behind another provider's rows.
    assert!(rate_deck_health(&pool, &b).await.unwrap().is_none());
}
