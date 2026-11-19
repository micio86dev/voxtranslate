//! Live Connect billing (spec 0121, S10): a tick is charged at most once, a wallet can never be
//! overspent however many conversations draw on it, and an unaffordable tick costs nothing.
//!
//! These drive `charge_tick` directly against a real database, because the guarantees are
//! database guarantees — a unit test with a mock could not fail the way a lost race fails.
//!
//! DB-gated like the other integration suites. Run against the local pgvector Postgres:
//! `DATABASE_URL=postgres://vox:vox_local_dev@127.0.0.1:55432/voxtranslate_voip_test \
//!   cargo test --test live_billing`. A run of a few milliseconds skipped everything.

use rust_decimal::Decimal;
use uuid::Uuid;
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::db::{self, Pool};
use voxtranslate_server::live::billing::{charge_tick, Charge, LivePayer, Tick};

async fn setup() -> Option<Pool> {
    let url = db::test_database_url()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    Some(pool)
}

macro_rules! pool {
    () => {
        match setup().await {
            Some(p) => p,
            None => {
                eprintln!("skipping — no DATABASE_URL");
                return;
            }
        }
    };
}

async fn make_user(pool: &Pool, balance: Decimal) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO users (google_id, email, name, balance) VALUES ($1, $2, 'Payer', $3) RETURNING id",
    )
    .bind(format!("g-{}", Uuid::new_v4()))
    .bind(format!("{}@x.com", Uuid::new_v4()))
    .bind(balance)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn make_org(pool: &Pool, owner: Uuid, credits: i32) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id, credits_balance) VALUES ('Hotel', $1, $2, $3) RETURNING id",
    )
    .bind(format!("slug-{}", Uuid::new_v4().simple()))
    .bind(owner)
    .bind(credits)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A live session with one participant, owned by a user or an org. Returns
/// `(live session, peer, call session)` — the call session is what an org ledger row references.
async fn make_session(pool: &Pool, user: Option<Uuid>, org: Option<Uuid>) -> (Uuid, Uuid, Uuid) {
    let call: Uuid = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO call_sessions (id, room, kind, org_id) VALUES ($1, $2, 'live_connect', $3)",
    )
    .bind(call)
    .bind(format!("live-{call}"))
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
    let session: Uuid = sqlx::query_scalar(
        "INSERT INTO live_sessions (call_session_id, owner_user_id, org_id, host_lang, status, expires_at)
         VALUES ($1, $2, $3, 'it', 'active', now() + interval '1 hour') RETURNING id",
    )
    .bind(call)
    .bind(user)
    .bind(org)
    .fetch_one(pool)
    .await
    .unwrap();
    let peer: Uuid = sqlx::query_scalar(
        "INSERT INTO live_participants (session_id, role, display_name, lang) VALUES ($1, 'host', 'P', 'it') RETURNING peer_id",
    )
    .bind(session)
    .fetch_one(pool)
    .await
    .unwrap();
    (session, peer, call)
}

async fn a_second_peer(pool: &Pool, session: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO live_participants (session_id, role, display_name, lang) VALUES ($1, 'guest', 'G', 'es') RETURNING peer_id",
    )
    .bind(session)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn user_payer(pool: &Pool, user: Uuid) -> LivePayer {
    let billing = BillingService::new(pool.clone(), usd(0.05));
    let usage_session = billing
        .create_session(user, "live-test", "standard")
        .await
        .unwrap();
    LivePayer::User {
        user_id: user,
        usage_session,
    }
}

async fn balance(pool: &Pool, user: Uuid) -> Decimal {
    sqlx::query_scalar("SELECT balance FROM users WHERE id = $1")
        .bind(user)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn credits(pool: &Pool, org: Uuid) -> i32 {
    sqlx::query_scalar("SELECT credits_balance FROM organizations WHERE id = $1")
        .bind(org)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn count(pool: &Pool, sql: &str, id: Uuid) -> i64 {
    sqlx::query_scalar(sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

// ---- idempotency -----------------------------------------------------------------------

#[tokio::test]
async fn a_tick_raced_by_many_meters_is_charged_exactly_once() {
    let pool = pool!();
    let user = make_user(&pool, usd(1.0)).await;
    let (session, peer, _) = make_session(&pool, Some(user), None).await;
    let payer = user_payer(&pool, user).await;

    // Eight meters (a phone that reconnected several times) all fire the same tick at once.
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let (pool, payer) = (pool.clone(), payer);
        tasks.push(tokio::spawn(async move {
            charge_tick(
                &pool,
                &payer,
                session,
                peer,
                42,
                Charge {
                    seconds: 5,
                    usd: usd(0.01),
                    credits: 0,
                },
            )
            .await
            .unwrap()
        }));
    }
    let mut charged = 0;
    let mut duplicates = 0;
    for t in tasks {
        match t.await.unwrap() {
            Tick::Charged { .. } => charged += 1,
            Tick::Duplicate => duplicates += 1,
            Tick::Insufficient => panic!("funds were plentiful"),
        }
    }
    assert_eq!((charged, duplicates), (1, 7));
    assert_eq!(balance(&pool, user).await, usd(0.99), "debited once");
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM credit_transactions WHERE user_id = $1 AND kind = 'usage'",
            user
        )
        .await,
        1,
        "one ledger row"
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM live_usage_ticks WHERE session_id = $1",
            session
        )
        .await,
        1
    );
}

#[tokio::test]
async fn distinct_ticks_each_charge_and_each_peer_has_its_own() {
    let pool = pool!();
    let user = make_user(&pool, usd(1.0)).await;
    let (session, host, _) = make_session(&pool, Some(user), None).await;
    let guest = a_second_peer(&pool, session).await;
    let payer = user_payer(&pool, user).await;

    for seq in [1, 2, 3] {
        for peer in [host, guest] {
            let t = charge_tick(
                &pool,
                &payer,
                session,
                peer,
                seq,
                Charge {
                    seconds: 5,
                    usd: usd(0.01),
                    credits: 0,
                },
            )
            .await
            .unwrap();
            assert!(
                matches!(t, Tick::Charged { .. }),
                "seq {seq} peer {peer}: {t:?}"
            );
        }
    }
    // Six distinct (peer, tick) pairs, all billed to the owner.
    assert_eq!(balance(&pool, user).await, usd(0.94));
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM live_usage_ticks WHERE session_id = $1",
            session
        )
        .await,
        6
    );
}

#[tokio::test]
async fn an_unaffordable_tick_charges_nothing_and_can_be_retried_after_a_top_up() {
    let pool = pool!();
    let user = make_user(&pool, usd(0.005)).await;
    let (session, peer, _) = make_session(&pool, Some(user), None).await;
    let payer = user_payer(&pool, user).await;

    let t = charge_tick(
        &pool,
        &payer,
        session,
        peer,
        9,
        Charge {
            seconds: 5,
            usd: usd(0.01),
            credits: 0,
        },
    )
    .await
    .unwrap();
    assert_eq!(t, Tick::Insufficient);
    assert_eq!(balance(&pool, user).await, usd(0.005), "balance untouched");
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM live_usage_ticks WHERE session_id = $1",
            session
        )
        .await,
        0,
        "a refused tick must not be recorded, or a retry would look like a duplicate"
    );

    sqlx::query("UPDATE users SET balance = 1 WHERE id = $1")
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
    let t = charge_tick(
        &pool,
        &payer,
        session,
        peer,
        9,
        Charge {
            seconds: 5,
            usd: usd(0.01),
            credits: 0,
        },
    )
    .await
    .unwrap();
    assert!(
        matches!(t, Tick::Charged { .. }),
        "same tick, after a top-up: {t:?}"
    );
}

// ---- nobody can overspend a wallet ------------------------------------------------------

#[tokio::test]
async fn concurrent_conversations_cannot_overspend_a_consumer_balance() {
    let pool = pool!();
    // $0.010 buys exactly ten $0.001 ticks.
    let user = make_user(&pool, usd(0.010)).await;
    let payer = user_payer(&pool, user).await;
    // Five conversations by one owner, drawing on the same balance at the same time.
    let mut sessions = Vec::new();
    for _ in 0..5 {
        sessions.push(make_session(&pool, Some(user), None).await);
    }

    let mut tasks = Vec::new();
    for (session, peer, _) in sessions {
        for seq in 0..6 {
            let (pool, payer) = (pool.clone(), payer);
            tasks.push(tokio::spawn(async move {
                charge_tick(
                    &pool,
                    &payer,
                    session,
                    peer,
                    seq,
                    Charge {
                        seconds: 1,
                        usd: usd(0.001),
                        credits: 0,
                    },
                )
                .await
                .unwrap()
            }));
        }
    }
    let mut charged = 0;
    for t in tasks {
        if matches!(t.await.unwrap(), Tick::Charged { .. }) {
            charged += 1;
        }
    }
    assert_eq!(charged, 10, "thirty attempts, ten affordable");
    assert_eq!(
        balance(&pool, user).await,
        Decimal::ZERO,
        "spent to the cent, never below"
    );
}

#[tokio::test]
async fn concurrent_conversations_cannot_overspend_an_org_wallet() {
    let pool = pool!();
    let owner = make_user(&pool, usd(0.0)).await;
    let org = make_org(&pool, owner, 10).await;
    let payer_for = |call| LivePayer::Org {
        org_id: org,
        call_session: call,
    };

    // Four desks' conversations, five one-credit ticks each, all at once, against 10 credits.
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let (session, peer, call) = make_session(&pool, None, Some(org)).await;
        for seq in 0..5 {
            let (pool, payer) = (pool.clone(), payer_for(call));
            tasks.push(tokio::spawn(async move {
                charge_tick(
                    &pool,
                    &payer,
                    session,
                    peer,
                    seq,
                    Charge {
                        seconds: 5,
                        usd: Decimal::ZERO,
                        credits: 1,
                    },
                )
                .await
                .unwrap()
            }));
        }
    }
    let mut charged = 0;
    let mut refused = 0;
    for t in tasks {
        match t.await.unwrap() {
            Tick::Charged { .. } => charged += 1,
            Tick::Insufficient => refused += 1,
            Tick::Duplicate => panic!("every tick here is distinct"),
        }
    }
    assert_eq!((charged, refused), (10, 10));
    assert_eq!(
        credits(&pool, org).await,
        0,
        "exactly spent, never negative"
    );
    let ledger: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM organization_credits_transactions WHERE org_id = $1 AND type = 'live_connect'",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        ledger, 10,
        "one ledger row per debit, attributed to Live Connect"
    );
}

#[tokio::test]
async fn an_org_charge_is_attributed_to_the_conversation_and_is_idempotent() {
    let pool = pool!();
    let owner = make_user(&pool, usd(0.0)).await;
    let org = make_org(&pool, owner, 100).await;
    let (session, peer, call) = make_session(&pool, None, Some(org)).await;
    let payer = LivePayer::Org {
        org_id: org,
        call_session: call,
    };

    assert!(matches!(
        charge_tick(
            &pool,
            &payer,
            session,
            peer,
            1,
            Charge {
                seconds: 5,
                usd: Decimal::ZERO,
                credits: 3
            }
        )
        .await
        .unwrap(),
        Tick::Charged { balance: None }
    ));
    assert_eq!(
        charge_tick(
            &pool,
            &payer,
            session,
            peer,
            1,
            Charge {
                seconds: 5,
                usd: Decimal::ZERO,
                credits: 3
            }
        )
        .await
        .unwrap(),
        Tick::Duplicate
    );
    assert_eq!(credits(&pool, org).await, 97, "charged once");
    let (amount, attributed): (i32, Option<Uuid>) = sqlx::query_as(
        "SELECT amount, session_id FROM organization_credits_transactions WHERE org_id = $1 AND type = 'live_connect'",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((amount, attributed), (-3, Some(call)));
}
