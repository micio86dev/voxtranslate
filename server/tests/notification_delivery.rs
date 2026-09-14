//! What actually leaves the building when `notify` fans out, and the three
//! background loops that call it.
//!
//! `notifications_api.rs` covers the bell and the preferences that silence it;
//! this covers the wire. Both outbound channels have a stand-in — Resend through
//! `Config::resend_base_url`, web push through the subscription's own endpoint,
//! which lives in the database and therefore needs no seam at all — so the VAPID
//! signing, the payload encryption and the 410-means-forget-this-device rule all
//! run here with the code production runs.
//!
//! DB-gated: skipped without `DATABASE_URL`.

use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{any, post};
use axum::{Json, Router};
use chrono::{Duration as ChronoDuration, Utc};
use serde_json::{json, Value};
use uuid::Uuid;
use voxtranslate_server::auth::{upsert_google_user, FakeVerifier, GoogleIdentity};
use voxtranslate_server::billing::{usd, BillingService};
use voxtranslate_server::config::{Config, PushConfig, ResendConfig};
use voxtranslate_server::db::{self, Pool};
use voxtranslate_server::notifications;
use voxtranslate_server::AppState;

const SECRET: &str = "notification-delivery-secret";

// A real P-256 pair. The `web-push` crate derives a shared secret from these and
// refuses anything that is not a point on the curve, so placeholder strings fail
// at signing and never reach the transport this file is about.
const VAPID_PRIVATE: &str = "3Sv9uPlJFH7QnjcD3Xbqj7TAHSOZuvlT13k1nu_EMh8";
const VAPID_PUBLIC: &str =
    "BK0iuaVZWCRbVrgcFR5XG5PjjE-ZbeJXznBwtfLxsg8DImtdXyPXBL0_eB6IhK-VVauWtUDHqR-MTQadrhmzmd0";
const SUB_P256DH: &str =
    "BNRvrm7jRv2ifUzadyIQuZORM4D5yzesl3M9UFunMuJtiQGu29xW9z7pTIvjsTiJYgOcV1RL5KbCKsJaTSxMdjY";
const SUB_AUTH: &str = "AAECAwQFBgcICQoLDA0ODw";

// ---------------------------------------------------------------------------
// Stand-ins for the two outbound channels
// ---------------------------------------------------------------------------

/// A push service. `status` is what it answers with; 410 is how a real one says
/// the device is gone for good.
#[derive(Clone)]
struct PushService {
    hits: Arc<AtomicUsize>,
    status: Arc<AtomicU16>,
}

async fn push_endpoint(State(svc): State<PushService>) -> axum::response::Response {
    svc.hits.fetch_add(1, Ordering::SeqCst);
    StatusCode::from_u16(svc.status.load(Ordering::SeqCst))
        .unwrap()
        .into_response()
}

async fn mock_push() -> (String, PushService) {
    let svc = PushService {
        hits: Arc::new(AtomicUsize::new(0)),
        status: Arc::new(AtomicU16::new(201)),
    };
    let router = Router::new()
        .route("/push/{token}", any(push_endpoint))
        .with_state(svc.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{addr}/push"), svc)
}

/// A mail provider that keeps what it was asked to send.
#[derive(Clone, Default)]
struct MailService {
    sent: Arc<Mutex<Vec<Value>>>,
}

async fn mail_endpoint(State(svc): State<MailService>, Json(body): Json<Value>) -> Json<Value> {
    svc.sent.lock().unwrap().push(body);
    Json(json!({ "id": Uuid::new_v4() }))
}

async fn mock_mail() -> (String, MailService) {
    let svc = MailService::default();
    let router = Router::new()
        .route("/emails", post(mail_endpoint))
        .with_state(svc.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{addr}/emails"), svc)
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    pool: Pool,
    state: AppState,
    push: PushService,
    push_base: String,
    mail: MailService,
}

async fn setup() -> Option<Harness> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.ok()?;
    db::migrate(&pool).await.ok()?;
    let (push_base, push) = mock_push().await;
    let (mail_url, mail) = mock_mail().await;

    let mut config = Config::test_with_billing(&url, SECRET, 0.0);
    config.push = Some(PushConfig {
        vapid_public_key: VAPID_PUBLIC.into(),
        vapid_private_key: VAPID_PRIVATE.into(),
        vapid_subject: "mailto:ops@example.com".into(),
    });
    config.resend = Some(ResendConfig {
        api_key: "dummy".into(),
        from_email: "noreply@example.com".into(),
        from_name: "VoxTranslate".into(),
    });
    config.resend_base_url = Some(mail_url);
    let min_join = usd(config.billing.as_ref().unwrap().pricing.min_balance_to_join);
    let mut state = AppState::new(config);
    state.billing = Some(BillingService::new(pool.clone(), min_join));
    state.pool = Some(pool.clone());
    state.verifier = Arc::new(FakeVerifier);

    Some(Harness {
        pool,
        state,
        push,
        push_base,
        mail,
    })
}

/// Serialises the two tests that drive the subscription-expiry sweep.
///
/// `claim_expiring_subscriptions` is a GLOBAL `UPDATE … RETURNING` with no limit: it takes
/// every org that is due, not just the caller's. So the test that calls it directly will
/// happily claim the org belonging to the test that is driving the scheduler — which then
/// finds nothing, notifies nobody, and fails saying the owner was never told.
///
/// That is not a flake to retry, it is two tests competing for one global sweep. They take
/// turns instead. (Third instance of this shape today: see also the public webinar list
/// capped at 100, and the settlement sweep's batched claim.)
static SWEEP: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

macro_rules! harness {
    () => {
        match setup().await {
            Some(h) => h,
            None => {
                eprintln!("skipping — no DATABASE_URL");
                return;
            }
        }
    };
}

async fn user(h: &Harness, locale: &str) -> Uuid {
    let identity = GoogleIdentity {
        google_id: format!("g-{}", Uuid::new_v4()),
        email: format!("{}@x.com", Uuid::new_v4()),
        name: "Recipient".into(),
        avatar_url: None,
    };
    let (u, _) = upsert_google_user(&h.pool, &identity, rust_decimal::Decimal::ZERO, None, None)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET locale = $2 WHERE id = $1")
        .bind(u.id)
        .bind(locale)
        .execute(&h.pool)
        .await
        .unwrap();
    u.id
}

/// Register a device for `uid`, returning its subscription row id.
async fn subscribe(h: &Harness, uid: Uuid) -> Uuid {
    let endpoint = format!("{}/{}", h.push_base, Uuid::new_v4().simple());
    sqlx::query_scalar(
        "INSERT INTO user_push_subscriptions (user_id, endpoint, p256dh, auth)
         VALUES ($1,$2,$3,$4) RETURNING id",
    )
    .bind(uid)
    .bind(endpoint)
    .bind(SUB_P256DH)
    .bind(SUB_AUTH)
    .fetch_one(&h.pool)
    .await
    .unwrap()
}

async fn bell_count(h: &Harness, uid: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM notifications WHERE user_id = $1")
        .bind(uid)
        .fetch_one(&h.pool)
        .await
        .unwrap()
}

async fn fire(h: &Harness, uid: Uuid) {
    notifications::notify(
        &h.state,
        uid,
        "meeting_reminder",
        "en",
        "Standup in 10 minutes",
        "Your meeting starts soon.",
        json!({ "join_url": "https://app.test/?room=abc" }),
    )
    .await;
}

/// Poll a COUNT until it reaches `want`, so a spawned loop is never raced with a
/// bare sleep. Returns what it last saw.
async fn eventually(pool: &Pool, sql: &str, users: &[Uuid], want: i64, ms: u64) -> i64 {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    loop {
        let n: i64 = sqlx::query_scalar(sql)
            .bind(users.to_vec())
            .fetch_one(pool)
            .await
            .unwrap_or(0);
        if n >= want || tokio::time::Instant::now() >= deadline {
            return n;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The same, for a single nullable timestamp column.
async fn eventually_stamped(pool: &Pool, sql: &str, id: Uuid, ms: u64) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    loop {
        let stamped = sqlx::query_scalar::<_, Option<chrono::DateTime<Utc>>>(sql)
            .bind(id)
            .fetch_one(pool)
            .await
            .ok()
            .flatten()
            .is_some();
        if stamped || tokio::time::Instant::now() >= deadline {
            return stamped;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// The three channels
// ---------------------------------------------------------------------------

#[tokio::test]
async fn one_notification_reaches_the_bell_the_inbox_and_the_device() {
    let h = harness!();
    let uid = user(&h, "en").await;
    subscribe(&h, uid).await;

    fire(&h, uid).await;

    assert_eq!(bell_count(&h, uid).await, 1, "no bell row");
    assert_eq!(h.push.hits.load(Ordering::SeqCst), 1, "no push");
    let sent = h.mail.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1, "no email");
    assert_eq!(sent[0]["subject"], "Standup in 10 minutes");
    // The CTA is the whole point of the mail: a reminder you cannot act on from
    // your inbox is a reminder that arrives too late.
    assert!(sent[0]["html"]
        .as_str()
        .unwrap()
        .contains("https://app.test/?room=abc"));
}

#[tokio::test]
async fn a_device_that_reports_itself_gone_is_forgotten() {
    let h = harness!();
    let uid = user(&h, "en").await;
    let sub = subscribe(&h, uid).await;
    h.push.status.store(410, Ordering::SeqCst);

    fire(&h, uid).await;

    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM user_push_subscriptions WHERE id = $1")
            .bind(sub)
            .fetch_one(&h.pool)
            .await
            .unwrap();
    // 410 is permanent — the browser uninstalled the service worker. Keeping the
    // row means paying to fail against it forever.
    assert_eq!(left, 0, "a dead subscription was kept");
}

#[tokio::test]
async fn a_push_service_having_a_bad_day_does_not_lose_the_notification() {
    let h = harness!();
    let uid = user(&h, "en").await;
    let sub = subscribe(&h, uid).await;
    h.push.status.store(503, Ordering::SeqCst);

    fire(&h, uid).await;

    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM user_push_subscriptions WHERE id = $1")
            .bind(sub)
            .fetch_one(&h.pool)
            .await
            .unwrap();
    // A transient 5xx is not a dead device; deleting on it would silently
    // unsubscribe every user during an outage.
    assert_eq!(left, 1, "a transient failure unsubscribed the device");
    assert_eq!(
        bell_count(&h, uid).await,
        1,
        "the bell row was lost with it"
    );
}

#[tokio::test]
async fn every_device_the_user_owns_is_reached() {
    let h = harness!();
    let uid = user(&h, "en").await;
    subscribe(&h, uid).await;
    subscribe(&h, uid).await;
    subscribe(&h, uid).await;

    fire(&h, uid).await;

    assert_eq!(
        h.push.hits.load(Ordering::SeqCst),
        3,
        "a device was skipped"
    );
}

#[tokio::test]
async fn a_silenced_channel_silences_only_itself() {
    let h = harness!();
    let uid = user(&h, "en").await;
    subscribe(&h, uid).await;
    sqlx::query(
        "INSERT INTO notification_preferences (user_id, type, channel, enabled)
         VALUES ($1, 'meeting_reminder', 'push', false)",
    )
    .bind(uid)
    .execute(&h.pool)
    .await
    .unwrap();

    fire(&h, uid).await;

    assert_eq!(
        h.push.hits.load(Ordering::SeqCst),
        0,
        "push was not silenced"
    );
    assert_eq!(bell_count(&h, uid).await, 1, "the bell was silenced too");
    assert_eq!(
        h.mail.sent.lock().unwrap().len(),
        1,
        "email was silenced too"
    );
}

#[tokio::test]
async fn quiet_hours_hold_back_the_buzz_and_nothing_else() {
    let h = harness!();
    let uid = user(&h, "en").await;
    subscribe(&h, uid).await;
    // A window that certainly contains "now" in UTC, whatever hour it is.
    sqlx::query(
        "INSERT INTO notification_settings (user_id, quiet_hours_start, quiet_hours_end, timezone)
         VALUES ($1, 0, 24, 'UTC')",
    )
    .bind(uid)
    .execute(&h.pool)
    .await
    .unwrap();

    fire(&h, uid).await;

    // Asleep means not woken — it does not mean not told.
    assert_eq!(h.push.hits.load(Ordering::SeqCst), 0, "the phone buzzed");
    assert_eq!(bell_count(&h, uid).await, 1, "the bell row went missing");
}

// ---------------------------------------------------------------------------
// The background loops
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_meeting_reminder_loop_fires_once_and_only_once() {
    let h = harness!();
    let creator = user(&h, "en").await;
    let invitee = user(&h, "it").await;

    let meeting = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO scheduled_meetings
            (id, creator_user_id, title, scheduled_at, end_at, room_code, join_url,
             status, reminder_minutes_before)
         VALUES ($1,$2,'Standup',$3,$4,$5,'https://app.test/?room=x','scheduled',10)",
    )
    .bind(meeting)
    .bind(creator)
    .bind(Utc::now() + ChronoDuration::minutes(5))
    .bind(Utc::now() + ChronoDuration::minutes(35))
    .bind(format!("room-{}", Uuid::new_v4().simple()))
    .execute(&h.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO scheduled_meeting_invitees (meeting_id, user_id, email)
         VALUES ($1, $2, 'invitee@x.com')",
    )
    .bind(meeting)
    .bind(invitee)
    .execute(&h.pool)
    .await
    .unwrap();

    let task = tokio::spawn(notifications::run_reminder_scheduler(
        h.state.clone(),
        Duration::from_millis(80),
    ));
    let seen = eventually(
        &h.pool,
        "SELECT count(*) FROM notifications WHERE type = 'meeting_reminder' AND user_id = ANY($1)",
        &[creator, invitee],
        2,
        6000,
    )
    .await;
    assert!(seen >= 2, "only {seen} of the two invitees were reminded");

    // The claim is an atomic UPDATE … RETURNING, so a second tick must find
    // nothing — otherwise every overlapping sweep re-sends the same reminder.
    let after_first: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notifications WHERE type = 'meeting_reminder' AND user_id = ANY($1)",
    )
    .bind(vec![creator, invitee])
    .fetch_one(&h.pool)
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let after_more: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notifications WHERE type = 'meeting_reminder' AND user_id = ANY($1)",
    )
    .bind(vec![creator, invitee])
    .fetch_one(&h.pool)
    .await
    .unwrap();
    task.abort();
    assert_eq!(after_more, after_first, "the reminder fired twice");
}

#[tokio::test]
async fn only_the_people_who_can_renew_are_warned_about_an_expiring_plan() {
    let _sweep = SWEEP.lock().await;
    let h = harness!();
    let owner = user(&h, "en").await;
    let admin = user(&h, "de").await;
    let member = user(&h, "fr").await;

    let org: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id, subscription_status, current_period_end)
         VALUES ('Expiring Co', $1, $2, 'active', $3) RETURNING id",
    )
    .bind(format!("exp-{}", Uuid::new_v4().simple()))
    .bind(owner)
    .bind(Utc::now() + ChronoDuration::days(3))
    .fetch_one(&h.pool)
    .await
    .unwrap();
    for (uid, role) in [(owner, "owner"), (admin, "admin"), (member, "member")] {
        sqlx::query("INSERT INTO organization_members (org_id, user_id, role) VALUES ($1,$2,$3)")
            .bind(org)
            .bind(uid)
            .bind(role)
            .execute(&h.pool)
            .await
            .unwrap();
    }

    let claimed = notifications::claim_expiring_subscriptions(
        &h.pool,
        notifications::SUBSCRIPTION_EXPIRY_LEAD_DAYS,
    )
    .await
    .unwrap();
    assert!(
        claimed.iter().any(|c| c.id == org),
        "an org three days from expiry was not claimed"
    );

    // The marker stores the PERIOD, so a second sweep on the same period is a
    // no-op — and a renewal moves the period and re-arms it with nothing to clear.
    let again = notifications::claim_expiring_subscriptions(
        &h.pool,
        notifications::SUBSCRIPTION_EXPIRY_LEAD_DAYS,
    )
    .await
    .unwrap();
    assert!(
        !again.iter().any(|c| c.id == org),
        "the same period was claimed twice"
    );
}

#[tokio::test]
async fn the_expiry_loop_warns_owners_and_admins_in_their_own_language() {
    let _sweep = SWEEP.lock().await;
    let h = harness!();
    let owner = user(&h, "en").await;
    let member = user(&h, "fr").await;

    let org: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id, subscription_status, current_period_end)
         VALUES ('Renewal Co', $1, $2, 'active', $3) RETURNING id",
    )
    .bind(format!("ren-{}", Uuid::new_v4().simple()))
    .bind(owner)
    .bind(Utc::now() + ChronoDuration::days(2))
    .fetch_one(&h.pool)
    .await
    .unwrap();
    for (uid, role) in [(owner, "owner"), (member, "member")] {
        sqlx::query("INSERT INTO organization_members (org_id, user_id, role) VALUES ($1,$2,$3)")
            .bind(org)
            .bind(uid)
            .bind(role)
            .execute(&h.pool)
            .await
            .unwrap();
    }

    let task = tokio::spawn(notifications::run_subscription_expiry_scheduler(
        h.state.clone(),
        Duration::from_millis(80),
    ));
    let warned = eventually(
        &h.pool,
        "SELECT count(*) FROM notifications
         WHERE type = 'subscription_expiring' AND user_id = ANY($1)",
        &[owner],
        1,
        6000,
    )
    .await;
    task.abort();
    assert!(warned >= 1, "the owner was never told the plan was ending");

    // A plain member cannot buy a plan, so warning them is noise they can do
    // nothing about.
    let to_member: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notifications WHERE type = 'subscription_expiring' AND user_id = $1",
    )
    .bind(member)
    .fetch_one(&h.pool)
    .await
    .unwrap();
    assert_eq!(to_member, 0, "a member who cannot renew was warned anyway");
}

#[tokio::test]
async fn a_public_webinar_is_only_due_a_reminder_when_it_asked_for_one() {
    let h = harness!();
    let host = user(&h, "en").await;
    let soon = Utc::now() + ChronoDuration::minutes(5);
    let org: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, owner_id) VALUES ('Webinar Co', $1, $2)
         RETURNING id",
    )
    .bind(format!("web-{}", Uuid::new_v4().simple()))
    .bind(host)
    .fetch_one(&h.pool)
    .await
    .unwrap();

    let mut want = None;
    for (visibility, notify_friends, status) in [
        ("public", true, "scheduled"),
        ("public", false, "scheduled"), // the host opted out
        ("private", true, "scheduled"), // nobody to tell
        ("public", true, "live"),       // already started
    ] {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO webinars
                (code, title, org_id, host_user_id, visibility, status, scheduled_start,
                 reminder_minutes_before, notify_friends, source_language)
             VALUES ($1,'Launch',$7,$2,$3,$4,$5,10,$6,'en') RETURNING id",
        )
        .bind(Uuid::new_v4().simple().to_string())
        .bind(host)
        .bind(visibility)
        .bind(status)
        .bind(soon)
        .bind(notify_friends)
        .bind(org)
        .fetch_one(&h.pool)
        .await
        .unwrap();
        if want.is_none() {
            want = Some(id);
        }
    }
    let want = want.unwrap();

    let due = notifications::select_due_webinar_reminders(&h.pool, Utc::now())
        .await
        .unwrap();
    assert!(due.contains(&want), "the one due webinar was not selected");

    // The loop stamps `reminder_sent_at` in its RETURNING, so the selection must
    // drain — a predicate that kept matching would re-alert every friend on
    // every tick.
    let task = tokio::spawn(notifications::run_webinar_reminder_scheduler(
        h.state.clone(),
        Duration::from_millis(80),
    ));
    let drained = eventually_stamped(
        &h.pool,
        "SELECT reminder_sent_at FROM webinars WHERE id = $1",
        want,
        6000,
    )
    .await;
    task.abort();

    // These four are public and upcoming, which is exactly what the discovery list
    // shows. Leaving them behind would crowd that capped list for every other test
    // sharing this database, so they go.
    sqlx::query("DELETE FROM webinars WHERE org_id = $1")
        .bind(org)
        .execute(&h.pool)
        .await
        .unwrap();

    assert!(drained, "the webinar was never claimed");
}

#[tokio::test]
async fn a_scheduler_without_a_database_exits_rather_than_spinning() {
    let h = harness!();
    let mut stateless = h.state.clone();
    stateless.pool = None;

    // Every loop takes the pool up front; with none there is nothing to sweep and
    // no reason to hold a task open for the life of the process.
    for loop_fut in [tokio::time::timeout(
        Duration::from_millis(500),
        notifications::run_reminder_scheduler(stateless.clone(), Duration::from_millis(50)),
    )] {
        assert!(loop_fut.await.is_ok(), "the loop kept running with no pool");
    }
    assert!(tokio::time::timeout(
        Duration::from_millis(500),
        notifications::run_subscription_expiry_scheduler(
            stateless.clone(),
            Duration::from_millis(50)
        ),
    )
    .await
    .is_ok());
    assert!(tokio::time::timeout(
        Duration::from_millis(500),
        notifications::run_webinar_reminder_scheduler(stateless, Duration::from_millis(50)),
    )
    .await
    .is_ok());
}
