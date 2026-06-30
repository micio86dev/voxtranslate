//! Integration tests for opt-in user location (`/api/user/location`). DB-gated —
//! skipped without `DATABASE_URL`. Exercises the store/clear handlers, coordinate
//! validation, the auth gate, and the best-effort PostGIS geometry setup.

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::Client;
use serde_json::json;
use uuid::Uuid;
use voxtranslate_server::auth::{issue_jwt, upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::config::Config;
use voxtranslate_server::{app, db, location, AppState};

const SECRET: &str = "location-test-secret";

struct Server {
    addr: SocketAddr,
    pool: db::Pool,
}

async fn setup() -> Option<Server> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let config = Config::test_with_billing(&url, SECRET, 0.0);
    let mut state = AppState::new(config);
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    Some(Server { addr, pool })
}

async fn make_user(srv: &Server) -> (Uuid, String) {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: "Geo Tester".into(),
        avatar_url: None,
    };
    let u = upsert_google_user(&srv.pool, &identity, rust_decimal::Decimal::ZERO, None, None)
        .await
        .unwrap();
    let jwt = issue_jwt(SECRET, &u.id, &u.email, &u.name, 168).unwrap();
    (u.id, jwt)
}

macro_rules! skip_without_db {
    ($e:expr) => {
        match $e {
            Some(s) => s,
            None => {
                eprintln!("skipping — no DATABASE_URL");
                return;
            }
        }
    };
}

async fn consent_row(srv: &Server, id: Uuid) -> (Option<f64>, Option<f64>, bool) {
    sqlx::query_as::<_, (Option<f64>, Option<f64>, bool)>(
        "SELECT latitude, longitude, location_consent FROM users WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&srv.pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn store_then_clear_location() {
    let srv = skip_without_db!(setup().await);
    let (id, jwt) = make_user(&srv).await;
    let base = format!("http://{}/api/user/location", srv.addr);
    let http = Client::new();

    // Store valid coordinates → 204 + row updated, consent set.
    let res = http
        .post(&base)
        .bearer_auth(&jwt)
        .json(&json!({ "latitude": 45.46, "longitude": 9.19, "accuracy": 12.5 }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
    let (lat, lng, consent) = consent_row(&srv, id).await;
    assert!((lat.unwrap() - 45.46).abs() < 1e-6);
    assert!((lng.unwrap() - 9.19).abs() < 1e-6);
    assert!(consent);

    // Clear → 204 + coordinates forgotten, consent withdrawn.
    let res = http.delete(&base).bearer_auth(&jwt).send().await.unwrap();
    assert_eq!(res.status(), 204);
    let (lat, lng, consent) = consent_row(&srv, id).await;
    assert!(lat.is_none() && lng.is_none() && !consent);
}

#[tokio::test]
async fn rejects_out_of_range_coordinates() {
    let srv = skip_without_db!(setup().await);
    let (_id, jwt) = make_user(&srv).await;
    let base = format!("http://{}/api/user/location", srv.addr);
    let http = Client::new();

    for bad in [
        json!({ "latitude": 200.0, "longitude": 9.0 }),
        json!({ "latitude": 45.0, "longitude": -999.0 }),
    ] {
        let res = http.post(&base).bearer_auth(&jwt).json(&bad).send().await.unwrap();
        assert_eq!(res.status(), 400, "out-of-range coords must be rejected");
    }
}

#[tokio::test]
async fn requires_authentication() {
    let srv = skip_without_db!(setup().await);
    let base = format!("http://{}/api/user/location", srv.addr);
    let http = Client::new();
    let res = http
        .post(&base)
        .json(&json!({ "latitude": 1.0, "longitude": 2.0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
}

#[tokio::test]
async fn ensure_geometry_is_non_fatal() {
    let srv = skip_without_db!(setup().await);
    // Best-effort: succeeds where PostGIS is available, logs a warning and returns
    // otherwise. Either way it must never panic.
    location::ensure_geometry(&srv.pool).await;
}
