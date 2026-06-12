//! Auth + billing + Stripe end-to-end tests over real HTTP/WebSocket.
//!
//! These live in their own integration binary (not in `lib.rs` unit tests) so
//! coverage from the spawned `axum::serve` tasks aggregates reliably. Every test
//! is **DB-gated**: it no-ops when `DATABASE_URL` is unset (e.g. CI without a
//! Postgres service). Locally, run against the Docker Postgres:
//! `DATABASE_URL=postgres://postgres:postgres@localhost:5433/voxtest cargo test --test billing`.

use std::sync::Arc;

use voxtranslate_server::auth::FakeVerifier;
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::Config;
use voxtranslate_server::safety::SafetyService;
use voxtranslate_server::{app, db, AppState};

/// Build a billing-mode `AppState` (FakeVerifier + test pool) and wire the pool.
async fn billing_state(secret: &str, free: f64) -> Option<(AppState, db::Pool)> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let config = Config::test_with_billing(&url, secret, free);
    let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);
    let mut state = AppState::new(config);
    state.billing = Some(BillingService::new(pool.clone(), min_join));
    state.safety = Some(SafetyService::new(pool.clone()));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);
    Some((state, pool))
}

mod auth_http {
    use super::*;
    use std::net::SocketAddr;
    use uuid::Uuid;

    async fn spawn() -> Option<(SocketAddr, f64)> {
        let (state, _pool) = billing_state("test-secret", 2.0).await?;
        let free = state.config.billing.as_ref().unwrap().pricing.free_credits;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app(state)).await;
        });
        Some((addr, free))
    }

    #[tokio::test]
    async fn google_login_me_and_unauthorized() {
        let Some((addr, free)) = spawn().await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        let http = reqwest::Client::new();

        // No token -> 401.
        let r = http
            .get(format!("http://{addr}/api/user/me"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401);

        // Public auth config carries the Google client id (for the GSI button).
        let cfg: serde_json::Value = http
            .get(format!("http://{addr}/api/auth/config"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(cfg["google_client_id"], "test-client");

        // Login: the fake credential is a JSON-encoded identity.
        let cred = serde_json::json!({
            "google_id": format!("g-{}", Uuid::new_v4()),
            "email": format!("{}@x.com", Uuid::new_v4()),
            "name": "Alice",
            "avatar_url": "http://img/a",
        })
        .to_string();
        let login: serde_json::Value = http
            .post(format!("http://{addr}/api/auth/google"))
            .json(&serde_json::json!({ "credential": cred }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        let token = login["token"].as_str().expect("token").to_string();
        assert_eq!(login["user"]["name"], "Alice");
        assert!((login["user"]["balance"].as_f64().unwrap() - free).abs() < 1e-6);
        // Pricing internals must never leak to the client.
        assert!(login["user"].get("cost_per_minute").is_none());
        assert!(login["user"].get("user_rate_per_minute").is_none());

        // /me with the token returns the same balance.
        let me: serde_json::Value = http
            .get(format!("http://{addr}/api/user/me"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(me["name"], "Alice");
        assert!((me["balance"].as_f64().unwrap() - free).abs() < 1e-6);

        // A bad credential is rejected with 401.
        let bad = http
            .post(format!("http://{addr}/api/auth/google"))
            .json(&serde_json::json!({ "credential": "bad" }))
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 401);
    }

    #[tokio::test]
    async fn login_is_rate_limited() {
        let Some((addr, _)) = spawn().await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        let http = reqwest::Client::new();
        // Same google_id each time (upsert, no new users). Limit is 20/min per
        // client; reqwest sends no forwarded IP so all share one bucket.
        let cred = serde_json::json!({
            "google_id": format!("g-{}", Uuid::new_v4()),
            "email": format!("{}@x.com", Uuid::new_v4()),
            "name": "Flood",
        })
        .to_string();
        let body = serde_json::json!({ "credential": cred });

        let mut saw_429 = false;
        for _ in 0..25 {
            let resp = http
                .post(format!("http://{addr}/api/auth/google"))
                .json(&body)
                .send()
                .await
                .unwrap();
            if resp.status() == 429 {
                saw_429 = true;
                break;
            }
        }
        assert!(saw_429, "login should be throttled after the limit");
    }
}

mod guest_mode {
    //! Billing routes must degrade gracefully when billing isn't configured.
    //! Not DB-gated — runs even without a Postgres service.

    use super::*;
    use std::net::SocketAddr;

    async fn spawn_guest() -> SocketAddr {
        let config = Config {
            deepgram_key: "d".into(),
            groq_key: "g".into(),
            port: 0,
            allowed_origins: vec![],
            auto_detect_buffer_ms: 3000,
            billing: None,
            resend: None,
            storage: None,
        };
        let state = AppState::new(config);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app(state)).await;
        });
        addr
    }

    #[tokio::test]
    async fn billing_routes_unavailable_without_billing() {
        let addr = spawn_guest().await;
        let http = reqwest::Client::new();

        // Catalog + auth config are unavailable (503) without billing.
        let pkgs = http
            .get(format!("http://{addr}/api/billing/packages"))
            .send()
            .await
            .unwrap();
        assert_eq!(pkgs.status(), 503);
        let cfg = http
            .get(format!("http://{addr}/api/auth/config"))
            .send()
            .await
            .unwrap();
        assert_eq!(cfg.status(), 503);

        // Protected route rejects with 401 (no billing -> no auth).
        let me = http
            .get(format!("http://{addr}/api/user/me"))
            .send()
            .await
            .unwrap();
        assert_eq!(me.status(), 401);
    }
}

mod ws_metering {
    use super::*;
    use futures::StreamExt;
    use std::net::SocketAddr;
    use std::time::Duration;
    use uuid::Uuid;
    use voxtranslate_server::auth::{issue_jwt, upsert_google_user, GoogleIdentity};
    use voxtranslate_server::db::Pool;

    struct Server {
        addr: SocketAddr,
        pool: Pool,
        secret: String,
    }

    async fn setup() -> Option<Server> {
        let secret = "ws-secret".to_string();
        let (state, pool) = billing_state(&secret, 2.0).await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app(state)).await;
        });
        Some(Server { addr, pool, secret })
    }

    type Ws = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    /// Connect and return the first JSON text frame (keeping the socket open).
    async fn connect_first(addr: SocketAddr, params: &str) -> (serde_json::Value, Ws) {
        let url = format!("ws://{addr}/ws?{params}");
        let (mut ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .expect("ws connect");
        let frame = loop {
            match tokio::time::timeout(Duration::from_secs(2), ws.next()).await {
                Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t)))) => {
                    break serde_json::from_str(t.as_str()).unwrap()
                }
                Ok(Some(Ok(_))) => continue,
                _ => panic!("no frame"),
            }
        };
        (frame, ws)
    }

    #[tokio::test]
    async fn guest_billed_and_auth_rejections() {
        let Some(srv) = setup().await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        let addr = srv.addr;

        // 1) Guest (no token) joins fine.
        let gid = Uuid::new_v4();
        let (frame, mut guest_ws) =
            connect_first(addr, &format!("room=wsr&lang=en&id=guest-{gid}")).await;
        assert_eq!(frame["type"], "room_joined");

        // 2) Billed user (valid token) joins and gets a usage session.
        let identity = GoogleIdentity {
            google_id: format!("g-{}", Uuid::new_v4()),
            email: format!("{}@x.com", Uuid::new_v4()),
            name: "Billed".into(),
            avatar_url: Some("https://img/billed=s96".into()),
        };
        let user = upsert_google_user(&srv.pool, &identity, rust_decimal::Decimal::new(200, 2))
            .await
            .unwrap();
        let jwt = issue_jwt(&srv.secret, &user.id, &user.email, &user.name, 168).unwrap();
        let (frame, _billed) = connect_first(
            addr,
            &format!("room=wsr&lang=it&id=u-{}&token={jwt}", user.id),
        )
        .await;
        assert_eq!(frame["type"], "room_joined");

        // The guest is told the billed peer joined, carrying its Google avatar.
        let pj = loop {
            match tokio::time::timeout(Duration::from_secs(2), guest_ws.next()).await {
                Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t)))) => {
                    let v: serde_json::Value = serde_json::from_str(t.as_str()).unwrap();
                    if v["type"] == "peer_joined" {
                        break v;
                    }
                }
                Ok(Some(Ok(_))) => continue,
                _ => panic!("no peer_joined"),
            }
        };
        assert_eq!(pj["avatar_url"], "https://img/billed=s96");

        // The billed join created exactly one usage session for this room.
        let mut sessions = 0i64;
        for _ in 0..20 {
            sessions = sqlx::query_scalar(
                "SELECT COUNT(*) FROM usage_sessions WHERE user_id = $1 AND room = 'wsr'",
            )
            .bind(user.id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
            if sessions >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(sessions, 1, "billed peer gets a usage session");

        // 3) Invalid token is rejected with code `invalid_token`.
        let (frame, _) = connect_first(addr, "room=wsr&lang=en&id=bad&token=not-a-jwt").await;
        assert_eq!(frame["type"], "error");
        assert_eq!(frame["code"], "invalid_token");

        // 4) A funded-below-threshold user is rejected with `insufficient_balance`.
        let broke_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (google_id, email, name, balance)
             VALUES ($1, $2, 'Broke', 0) RETURNING id",
        )
        .bind(format!("g-{}", Uuid::new_v4()))
        .bind(format!("{}@x.com", Uuid::new_v4()))
        .fetch_one(&srv.pool)
        .await
        .unwrap();
        let broke_jwt = issue_jwt(&srv.secret, &broke_id, "b@x.com", "Broke", 168).unwrap();
        let (frame, _) = connect_first(
            addr,
            &format!("room=wsr2&lang=en&id=broke&token={broke_jwt}"),
        )
        .await;
        assert_eq!(frame["type"], "error");
        assert_eq!(frame["code"], "insufficient_balance");
    }
}

mod stripe_api {
    use super::*;
    use std::net::SocketAddr;
    use uuid::Uuid;
    use voxtranslate_server::auth::issue_jwt;
    use voxtranslate_server::config::CreditPackage;
    use voxtranslate_server::db::Pool;
    use voxtranslate_server::stripe_handler::sign_payload;

    struct Server {
        addr: SocketAddr,
        pool: Pool,
        secret: String,
        webhook_secret: String,
    }

    async fn setup() -> Option<Server> {
        let url = std::env::var("DATABASE_URL").ok()?;
        let pool = db::connect(&url).await.ok()?;
        db::migrate(&pool).await.ok()?;

        let secret = "stripe-test-secret".to_string();
        let webhook_secret = "whsec_test".to_string();
        let mut config = Config::test_with_billing(&url, &secret, 0.0);
        {
            let b = config.billing.as_mut().unwrap();
            b.stripe_webhook_secret = webhook_secret.clone();
            b.pricing.packages = vec![CreditPackage {
                id: "starter".into(),
                name: "Starter".into(),
                price_usd: 5.0,
                credits_usd: 5.0,
                stripe_price_id: "price_secret_xxx".into(),
            }];
        }
        let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);
        let mut state = AppState::new(config);
        state.billing = Some(BillingService::new(pool.clone(), min_join));
        state.pool = Some(pool.clone());
        state.verifier = Arc::new(FakeVerifier);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app(state)).await;
        });
        Some(Server {
            addr,
            pool,
            secret,
            webhook_secret,
        })
    }

    async fn make_user(pool: &Pool, balance: i64) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO users (google_id, email, name, balance)
             VALUES ($1, $2, 'U', $3) RETURNING id",
        )
        .bind(format!("g-{}", Uuid::new_v4()))
        .bind(format!("{}@x.com", Uuid::new_v4()))
        .bind(rust_decimal::Decimal::new(balance, 2))
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn packages_omit_stripe_price_id() {
        let Some(srv) = setup().await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        let http = reqwest::Client::new();
        let pkgs: serde_json::Value = http
            .get(format!("http://{}/api/billing/packages", srv.addr))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let arr = pkgs.as_array().expect("array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], "starter");
        assert_eq!(arr[0]["price_usd"], 5.0);
        // The Stripe price id must never reach the client.
        assert!(arr[0].get("stripe_price_id").is_none());
    }

    #[tokio::test]
    async fn checkout_rejects_unknown_package() {
        let Some(srv) = setup().await else {
            return;
        };
        let uid = make_user(&srv.pool, 0).await;
        let jwt = issue_jwt(&srv.secret, &uid, "u@x.com", "U", 168).unwrap();
        let http = reqwest::Client::new();
        let resp = http
            .post(format!("http://{}/api/billing/checkout", srv.addr))
            .bearer_auth(&jwt)
            .json(&serde_json::json!({ "package_id": "does-not-exist" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);

        // Known package but no Stripe key configured -> 503 (payments off).
        let no_stripe = http
            .post(format!("http://{}/api/billing/checkout", srv.addr))
            .bearer_auth(&jwt)
            .json(&serde_json::json!({ "package_id": "starter" }))
            .send()
            .await
            .unwrap();
        assert_eq!(no_stripe.status(), 503);

        // Missing token -> 401.
        let unauth = http
            .post(format!("http://{}/api/billing/checkout", srv.addr))
            .json(&serde_json::json!({ "package_id": "starter" }))
            .send()
            .await
            .unwrap();
        assert_eq!(unauth.status(), 401);
    }

    #[tokio::test]
    async fn webhook_verifies_signature_and_credits_once() {
        let Some(srv) = setup().await else {
            return;
        };
        let uid = make_user(&srv.pool, 0).await;
        let http = reqwest::Client::new();
        let url = format!("http://{}/api/billing/webhook", srv.addr);

        let event_id = format!("evt_{}", Uuid::new_v4());
        let payload = serde_json::json!({
            "id": event_id,
            "type": "checkout.session.completed",
            "data": { "object": { "metadata": {
                "user_id": uid.to_string(),
                "credits_usd": "5.000000",
                "package_id": "starter",
            }}}
        })
        .to_string()
        .into_bytes();

        // Bad signature is rejected, nothing credited.
        let bad = http
            .post(&url)
            .header("stripe-signature", "t=1,v1=deadbeef")
            .body(payload.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 400);

        let sig = sign_payload(&srv.webhook_secret, 1_700_000_000, &payload);

        // First delivery credits 5.00.
        let ok1 = http
            .post(&url)
            .header("stripe-signature", &sig)
            .body(payload.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(ok1.status(), 200);
        assert_eq!(
            balance(&srv.pool, uid).await,
            rust_decimal::Decimal::new(500, 2)
        );

        // Replay of the same event id credits nothing more (idempotent).
        let ok2 = http
            .post(&url)
            .header("stripe-signature", &sig)
            .body(payload.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(ok2.status(), 200);
        assert_eq!(
            balance(&srv.pool, uid).await,
            rust_decimal::Decimal::new(500, 2)
        );

        // Exactly one purchase ledger row for this event.
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM credit_transactions WHERE stripe_event_id = $1",
        )
        .bind(&event_id)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
        assert_eq!(count, 1);
    }

    async fn balance(pool: &Pool, uid: Uuid) -> rust_decimal::Decimal {
        sqlx::query_scalar("SELECT balance FROM users WHERE id = $1")
            .bind(uid)
            .fetch_one(pool)
            .await
            .unwrap()
    }
}

mod account_api {
    use super::*;
    use std::net::SocketAddr;
    use uuid::Uuid;
    use voxtranslate_server::auth::{issue_jwt, upsert_google_user, GoogleIdentity};
    use voxtranslate_server::db::Pool;

    struct Server {
        addr: SocketAddr,
        pool: Pool,
        secret: String,
    }

    async fn setup() -> Option<Server> {
        let secret = "acct-secret".to_string();
        let (state, pool) = billing_state(&secret, 2.0).await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app(state)).await;
        });
        Some(Server { addr, pool, secret })
    }

    #[tokio::test]
    async fn history_and_usage_sessions() {
        let Some(srv) = setup().await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        let http = reqwest::Client::new();

        // First login grants free credits -> one ledger row.
        let identity = GoogleIdentity {
            google_id: format!("g-{}", Uuid::new_v4()),
            email: format!("{}@x.com", Uuid::new_v4()),
            name: "Acct".into(),
            avatar_url: None,
        };
        let user = upsert_google_user(&srv.pool, &identity, rust_decimal::Decimal::new(200, 2))
            .await
            .unwrap();
        let jwt = issue_jwt(&srv.secret, &user.id, &user.email, &user.name, 168).unwrap();

        // Unauthenticated history -> 401.
        let unauth = http
            .get(format!("http://{}/api/billing/history", srv.addr))
            .send()
            .await
            .unwrap();
        assert_eq!(unauth.status(), 401);

        let history: serde_json::Value = http
            .get(format!("http://{}/api/billing/history", srv.addr))
            .bearer_auth(&jwt)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let rows = history.as_array().expect("array");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["kind"], "free_credit");
        assert!((rows[0]["amount"].as_f64().unwrap() - 2.0).abs() < 1e-6);

        // Seed a usage session and read it back.
        sqlx::query(
            "INSERT INTO usage_sessions (user_id, room, speaking_seconds, cost)
             VALUES ($1, 'room-a', 42, 0.123456)",
        )
        .bind(user.id)
        .execute(&srv.pool)
        .await
        .unwrap();

        let sessions: serde_json::Value = http
            .get(format!("http://{}/api/usage/sessions", srv.addr))
            .bearer_auth(&jwt)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let rows = sessions.as_array().expect("array");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["room"], "room-a");
        assert_eq!(rows[0]["speaking_seconds"], 42);
        assert!((rows[0]["cost"].as_f64().unwrap() - 0.123456).abs() < 1e-6);
    }
}

mod safety_http {
    //! Reports, consent, GDPR export/delete over HTTP. DB-gated.

    use super::*;
    use std::net::SocketAddr;
    use uuid::Uuid;
    use voxtranslate_server::auth::{issue_jwt, upsert_google_user, GoogleIdentity};
    use voxtranslate_server::db::Pool;

    struct Server {
        addr: SocketAddr,
        pool: Pool,
        secret: String,
    }

    async fn setup() -> Option<Server> {
        let secret = "safety-secret".to_string();
        let (state, pool) = billing_state(&secret, 2.0).await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app(state)).await;
        });
        Some(Server { addr, pool, secret })
    }

    async fn login(srv: &Server) -> (Uuid, String) {
        let identity = GoogleIdentity {
            google_id: format!("g-{}", Uuid::new_v4()),
            email: format!("{}@x.com", Uuid::new_v4()),
            name: "Sam".into(),
            avatar_url: None,
        };
        let user = upsert_google_user(&srv.pool, &identity, rust_decimal::Decimal::new(200, 2))
            .await
            .unwrap();
        let jwt = issue_jwt(&srv.secret, &user.id, &user.email, &user.name, 168).unwrap();
        (user.id, jwt)
    }

    #[tokio::test]
    async fn report_consent_export_then_delete() {
        let Some(srv) = setup().await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        let (_uid, jwt) = login(&srv).await;
        let http = reqwest::Client::new();
        let base = format!("http://{}", srv.addr);

        // Report a peer.
        let r = http
            .post(format!("{base}/api/report"))
            .bearer_auth(&jwt)
            .json(&serde_json::json!({ "room": "r1", "reported_peer_id": "p9", "reported_name": "Bob", "reason": "harassment" }))
            .send().await.unwrap();
        assert_eq!(r.status(), 201);

        // Consent gate: /me shows not consented; false age -> 403; true -> 200.
        let me: serde_json::Value = http
            .get(format!("{base}/api/user/me"))
            .bearer_auth(&jwt)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(me["consent_given"], false);

        let bad = http
            .post(format!("{base}/api/user/consent"))
            .bearer_auth(&jwt)
            .json(&serde_json::json!({ "age_confirmed": false }))
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 403);

        let ok = http
            .post(format!("{base}/api/user/consent"))
            .bearer_auth(&jwt)
            .json(&serde_json::json!({ "age_confirmed": true }))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), 200);

        let me2: serde_json::Value = http
            .get(format!("{base}/api/user/me"))
            .bearer_auth(&jwt)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(me2["consent_given"], true);

        // GDPR export includes the profile + the report we filed.
        let data: serde_json::Value = http
            .get(format!("{base}/api/user/data"))
            .bearer_auth(&jwt)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(data["profile"]["email"].is_string());
        assert_eq!(data["reports_filed"][0]["reason"], "harassment");

        // GDPR erasure: delete -> /me now 404 (user gone).
        let del = http
            .delete(format!("{base}/api/user"))
            .bearer_auth(&jwt)
            .send()
            .await
            .unwrap();
        assert_eq!(del.status(), 200);
        let gone = http
            .get(format!("{base}/api/user/me"))
            .bearer_auth(&jwt)
            .send()
            .await
            .unwrap();
        assert_eq!(gone.status(), 404);

        // Endpoints require auth.
        assert_eq!(
            http.post(format!("{base}/api/report"))
                .json(&serde_json::json!({"room":"r","reason":"x"}))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
    }
}

mod safety_ws {
    //! WS gates: banned users rejected, public rooms require login. DB-gated.

    use super::*;
    use futures::StreamExt;
    use std::net::SocketAddr;
    use std::time::Duration;
    use uuid::Uuid;
    use voxtranslate_server::auth::{issue_jwt, upsert_google_user, GoogleIdentity};
    use voxtranslate_server::db::Pool;
    use voxtranslate_server::safety::SafetyService;

    struct Server {
        addr: SocketAddr,
        pool: Pool,
        secret: String,
    }

    async fn setup() -> Option<Server> {
        let secret = "safetyws-secret".to_string();
        let (state, pool) = billing_state(&secret, 2.0).await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app(state)).await;
        });
        Some(Server { addr, pool, secret })
    }

    async fn first_frame(addr: SocketAddr, params: &str) -> serde_json::Value {
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws?{params}"))
            .await
            .expect("ws connect");
        loop {
            match tokio::time::timeout(Duration::from_secs(2), ws.next()).await {
                Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t)))) => {
                    return serde_json::from_str(t.as_str()).unwrap()
                }
                Ok(Some(Ok(_))) => continue,
                _ => panic!("no frame"),
            }
        }
    }

    #[tokio::test]
    async fn banned_rejected_and_public_requires_login() {
        let Some(srv) = setup().await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        let addr = srv.addr;

        // A banned user is rejected with code `banned`.
        let identity = GoogleIdentity {
            google_id: format!("g-{}", Uuid::new_v4()),
            email: format!("{}@x.com", Uuid::new_v4()),
            name: "Bad".into(),
            avatar_url: None,
        };
        let user = upsert_google_user(&srv.pool, &identity, rust_decimal::Decimal::new(200, 2))
            .await
            .unwrap();
        SafetyService::new(srv.pool.clone())
            .ban_user(user.id, "abuse", None)
            .await
            .unwrap();
        let jwt = issue_jwt(&srv.secret, &user.id, &user.email, &user.name, 168).unwrap();
        let f = first_frame(addr, &format!("room=b&lang=en&id=ban&token={jwt}")).await;
        assert_eq!(f["type"], "error");
        assert_eq!(f["code"], "banned");

        // A guest can't join a PUBLIC room (accountability)...
        let pub_guest = first_frame(addr, "room=pubr&lang=en&id=g1&public=true").await;
        assert_eq!(pub_guest["type"], "error");
        assert_eq!(pub_guest["code"], "login_required");

        // ...but a guest CAN join a PRIVATE room.
        let priv_guest = first_frame(addr, "room=privr&lang=en&id=g2&public=false").await;
        assert_eq!(priv_guest["type"], "room_joined");
    }
}

mod admin_api {
    //! Secret-guarded backoffice actions (ban/unban/credit/resolve/delete) + audit.
    //! DB-gated. The shared secret matches `Config::test_with_billing`.

    use super::*;
    use std::net::SocketAddr;
    use uuid::Uuid;
    use voxtranslate_server::auth::{upsert_google_user, GoogleIdentity};
    use voxtranslate_server::db::Pool;

    const SECRET: &str = "test-admin-secret";

    struct Server {
        addr: SocketAddr,
        pool: Pool,
    }

    async fn setup() -> Option<Server> {
        let (state, pool) = billing_state("admin-secret", 2.0).await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app(state)).await;
        });
        Some(Server { addr, pool })
    }

    async fn make_user(pool: &Pool) -> Uuid {
        let identity = GoogleIdentity {
            google_id: format!("g-{}", Uuid::new_v4()),
            email: format!("{}@x.com", Uuid::new_v4()),
            name: "Adm".into(),
            avatar_url: None,
        };
        upsert_google_user(pool, &identity, rust_decimal::Decimal::new(200, 2))
            .await
            .unwrap()
            .id
    }

    #[tokio::test]
    async fn secret_guard_rejects_then_ban_unban_credit_resolve_delete() {
        let Some(srv) = setup().await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        let http = reqwest::Client::new();
        let base = format!("http://{}", srv.addr);
        let uid = make_user(&srv.pool).await;

        // No / wrong secret -> 403.
        let no_secret = http
            .post(format!("{base}/api/admin/ban"))
            .json(&serde_json::json!({ "user_id": uid, "reason": "x" }))
            .send()
            .await
            .unwrap();
        assert_eq!(no_secret.status(), 403);

        // The secret is checked BEFORE the body is parsed: a malformed body with
        // no secret is still 403 (not a 422 body-validation error).
        let malformed_no_secret = http
            .post(format!("{base}/api/admin/ban"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(malformed_no_secret.status(), 403);

        let wrong = http
            .post(format!("{base}/api/admin/ban"))
            .header("x-admin-secret", "nope")
            .json(&serde_json::json!({ "user_id": uid, "reason": "x" }))
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), 403);

        // Ban (7 days) with the right secret -> 200, and the user is banned.
        let ban = http
            .post(format!("{base}/api/admin/ban"))
            .header("x-admin-secret", SECRET)
            .json(&serde_json::json!({ "user_id": uid, "days": 7, "reason": "spam", "actor": "mod@vox" }))
            .send()
            .await
            .unwrap();
        assert_eq!(ban.status(), 200);
        let until: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT banned_until FROM users WHERE id = $1")
                .bind(uid)
                .fetch_one(&srv.pool)
                .await
                .unwrap();
        assert!(until.is_some());

        // Unban -> 200, ban cleared.
        let unban = http
            .post(format!("{base}/api/admin/unban"))
            .header("x-admin-secret", SECRET)
            .json(&serde_json::json!({ "user_id": uid }))
            .send()
            .await
            .unwrap();
        assert_eq!(unban.status(), 200);
        let until2: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT banned_until FROM users WHERE id = $1")
                .bind(uid)
                .fetch_one(&srv.pool)
                .await
                .unwrap();
        assert!(until2.is_none());

        // Credit +5.00 -> balance 2.00 + 5.00 = 7.00.
        let credit = http
            .post(format!("{base}/api/admin/credit"))
            .header("x-admin-secret", SECRET)
            .json(&serde_json::json!({ "user_id": uid, "amount": 5.0, "reason": "goodwill" }))
            .send()
            .await
            .unwrap();
        assert_eq!(credit.status(), 200);
        let balance: rust_decimal::Decimal =
            sqlx::query_scalar("SELECT balance FROM users WHERE id = $1")
                .bind(uid)
                .fetch_one(&srv.pool)
                .await
                .unwrap();
        assert_eq!(balance, rust_decimal::Decimal::new(700, 2));

        // A report exists -> resolve it (unknown id 404, bad action 400, ok 200).
        let report_id: Uuid = sqlx::query_scalar(
            "INSERT INTO reports (reporter_user_id, room, reason) VALUES ($1, 'r', 'spam') RETURNING id",
        )
        .bind(uid)
        .fetch_one(&srv.pool)
        .await
        .unwrap();

        let missing = http
            .post(format!("{base}/api/admin/report/resolve"))
            .header("x-admin-secret", SECRET)
            .json(&serde_json::json!({ "report_id": Uuid::new_v4(), "action": "resolved" }))
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), 404);

        let bad_action = http
            .post(format!("{base}/api/admin/report/resolve"))
            .header("x-admin-secret", SECRET)
            .json(&serde_json::json!({ "report_id": report_id, "action": "whatever" }))
            .send()
            .await
            .unwrap();
        assert_eq!(bad_action.status(), 400);

        let resolve = http
            .post(format!("{base}/api/admin/report/resolve"))
            .header("x-admin-secret", SECRET)
            .json(&serde_json::json!({ "report_id": report_id, "action": "dismissed", "note": "no abuse" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resolve.status(), 200);
        let status: String = sqlx::query_scalar("SELECT status FROM reports WHERE id = $1")
            .bind(report_id)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
        assert_eq!(status, "dismissed");

        // Every action wrote an audit row.
        let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_audit WHERE target = $1")
            .bind(uid.to_string())
            .fetch_one(&srv.pool)
            .await
            .unwrap();
        assert!(audits >= 3); // ban + unban + credit

        // GDPR delete via admin -> 200, user gone.
        let del = http
            .post(format!("{base}/api/admin/user/delete"))
            .header("x-admin-secret", SECRET)
            .json(&serde_json::json!({ "user_id": uid }))
            .send()
            .await
            .unwrap();
        assert_eq!(del.status(), 200);
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE id = $1")
            .bind(uid)
            .fetch_one(&srv.pool)
            .await
            .unwrap();
        assert_eq!(left, 0);
    }
}

mod content_api {
    //! Public managed content (i18n + legal) and the DB blocklist loader. DB-gated.

    use super::*;
    use std::net::SocketAddr;
    use uuid::Uuid;
    use voxtranslate_server::content::load_blocklist_terms;
    use voxtranslate_server::db::Pool;
    use voxtranslate_server::moderation::{Moderator, Severity};

    struct Server {
        addr: SocketAddr,
        pool: Pool,
    }

    async fn setup() -> Option<Server> {
        let (state, pool) = billing_state("content-secret", 2.0).await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app(state)).await;
        });
        Some(Server { addr, pool })
    }

    async fn ensure_lang(pool: &Pool, code: &str) {
        sqlx::query("INSERT INTO languages (code, name) VALUES ($1, $1) ON CONFLICT DO NOTHING")
            .bind(code)
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn i18n_and_legal_served_from_db() {
        let Some(srv) = setup().await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        let http = reqwest::Client::new();
        let base = format!("http://{}", srv.addr);
        ensure_lang(&srv.pool, "en").await;

        // A managed UI string override.
        let key = format!("greeting_{}", Uuid::new_v4().simple());
        let string_id: Uuid =
            sqlx::query_scalar("INSERT INTO i18n_strings (key) VALUES ($1) RETURNING id")
                .bind(&key)
                .fetch_one(&srv.pool)
                .await
                .unwrap();
        sqlx::query(
            "INSERT INTO i18n_translations (string_id, language, value) VALUES ($1, 'en', 'Hi there')",
        )
        .bind(string_id)
        .execute(&srv.pool)
        .await
        .unwrap();

        let i18n_resp = http
            .get(format!("{base}/api/content/i18n"))
            .send()
            .await
            .unwrap();
        // Short cache window so the client doesn't refetch the map every boot.
        assert!(i18n_resp
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("max-age")));
        let i18n: serde_json::Value = i18n_resp.json().await.unwrap();
        assert_eq!(i18n["en"][&key], "Hi there");

        // A managed legal page.
        let slug = format!("terms-{}", Uuid::new_v4().simple());
        let page_id: Uuid = sqlx::query_scalar(
            "INSERT INTO legal_pages (slug, version) VALUES ($1, 'v1') RETURNING id",
        )
        .bind(&slug)
        .fetch_one(&srv.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO legal_translations (page_id, language, title, body)
             VALUES ($1, 'en', 'Terms', '# Hello')",
        )
        .bind(page_id)
        .execute(&srv.pool)
        .await
        .unwrap();

        let legal: serde_json::Value = http
            .get(format!("{base}/api/content/legal/{slug}?lang=en"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(legal["title"], "Terms");
        assert_eq!(legal["version"], "v1");
        assert_eq!(legal["body"], "# Hello");

        // Unknown slug -> 404.
        let missing = http
            .get(format!("{base}/api/content/legal/does-not-exist"))
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), 404);
    }

    #[tokio::test]
    async fn blocklist_loads_from_db_and_flags() {
        let Some(srv) = setup().await else {
            return;
        };
        let term = format!("zzbad{}", Uuid::new_v4().simple());
        sqlx::query("INSERT INTO blocklist_terms (term) VALUES ($1)")
            .bind(&term)
            .execute(&srv.pool)
            .await
            .unwrap();

        let terms = load_blocklist_terms(&srv.pool).await;
        assert!(terms.iter().any(|t| t == &term));

        let m = Moderator::from_terms(std::iter::empty::<&str>()).with_terms(terms);
        assert_eq!(m.severity(&format!("you {term}!")), Severity::Severe);
        assert_eq!(m.severity("totally fine"), Severity::None);
    }
}
