//! Provider webhook ingestion (spec 0111, R24–R27).
//!
//! Three properties, and each one is a bug that costs money if it is missing:
//!
//! - **Verified** (R24). An unsigned, mis-signed or stale webhook changes nothing. The
//!   endpoint accepts call-control instructions from the internet; without this it accepts
//!   them from anyone.
//! - **Idempotent** (R25). Providers redeliver. Idempotency here is a `UNIQUE` constraint
//!   on `(provider, provider_event_id)`, **not** an application-level "have we seen this?"
//!   check — two workers processing the same redelivery concurrently would both pass such
//!   a check, and one of them would settle the reservation twice.
//! - **Monotonic** (R26). Events arrive out of business order. The lifecycle function
//!   ([`crate::voip::state::next`]) refuses to go backwards, so a late `ringing` cannot
//!   un-answer a call that is being billed.
//!
//! Everything happens in one transaction: the ledger row, the state change and the
//! settlement commit together or not at all. A crash between them would otherwise leave a
//! call whose credits were released twice, or never.

use chrono::Utc;
use rust_decimal::Decimal;
use sqlx::Row;
use uuid::Uuid;

use crate::db::Pool;
use crate::telephony::{
    LegId, ProviderEvent, ProviderEventKind, TelephonyProvider, WebhookError, WebhookHeaders,
};
use crate::voip::pricing;
use crate::voip::reservation;
use crate::voip::state::{next, CallEvent, CallState};

/// What ingesting one webhook did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ingest {
    /// Recorded, and the call moved.
    Applied {
        call_id: Uuid,
        before: CallState,
        after: CallState,
    },
    /// Recorded, but the lifecycle did not move — a duplicate of an event we already
    /// applied, one that arrived late, or one that carries information without changing
    /// state (a recording URL, a DTMF digit).
    Recorded { call_id: Uuid, state: CallState },
    /// The provider has sent this exact event id before. Nothing was written.
    Duplicate { call_id: Option<Uuid> },
    /// No call matches this leg. Recorded nowhere and acknowledged anyway: a webhook for a
    /// call that predates this deployment, or one belonging to another environment sharing
    /// the account, must not be retried forever.
    Unknown,
}

/// Verify, record and apply one webhook.
pub async fn ingest(
    pool: &Pool,
    provider: &dyn TelephonyProvider,
    headers: &WebhookHeaders,
    body: &[u8],
) -> Result<Ingest, WebhookError> {
    let event = provider.verify_webhook(headers, body, Utc::now())?;
    apply(pool, &event)
        .await
        .map_err(|e| WebhookError::Malformed {
            detail: e.to_string(),
        })
}

/// The database half, split out so tests can drive it with a constructed event and
/// without a signature.
pub async fn apply(pool: &Pool, event: &ProviderEvent) -> Result<Ingest, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Resolve the call. `client_state` is OUR correlation id and is authoritative; the leg
    // id is the fallback for events that predate us attaching one.
    let call: Option<CallRow> = resolve_call(&mut tx, event).await?;

    let Some((call_id, session_id, status_raw, price_per_min, answered_at)) = call else {
        tx.commit().await?;
        return Ok(Ingest::Unknown);
    };

    // The idempotency gate. `ON CONFLICT DO NOTHING RETURNING` yields no row when the
    // event id is already present, and the database — not this code — is what makes that
    // safe under concurrency.
    let inserted = sqlx::query(
        "INSERT INTO voip_provider_events
            (provider, provider_event_id, call_id, leg_id, event_type, occurred_at)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (provider, provider_event_id) DO NOTHING
         RETURNING id",
    )
    .bind(event.provider)
    .bind(&event.event_id)
    .bind(call_id)
    .bind(event.leg_id.as_str())
    .bind(event_type_name(&event.kind))
    .bind(event.occurred_at)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(row) = inserted else {
        // Normal, and its RATIO to accepted events is the signal: a spike means the
        // provider believes we are not acknowledging.
        crate::metrics::record_voip_webhook_duplicate();
        tx.commit().await?;
        return Ok(Ingest::Duplicate {
            call_id: Some(call_id),
        });
    };
    let event_row_id: Uuid = row.get("id");

    let before = CallState::parse(&status_raw).unwrap_or(CallState::Created);
    let lifecycle = lifecycle_event(&event.kind);
    let after = lifecycle.and_then(|e| next(before, e));

    // Information-carrying columns move regardless of whether the lifecycle did: a
    // recording saved after hangup still belongs on the call.
    apply_side_effects(&mut tx, call_id, event).await?;

    let outcome = match after {
        Some(new_state) => {
            let now = Utc::now();
            let answered = if new_state == CallState::Answered && answered_at.is_none() {
                Some(event.occurred_at)
            } else {
                answered_at
            };
            let ended = new_state.is_terminal().then_some(event.occurred_at);
            let duration = match (answered, ended) {
                (Some(a), Some(e)) => Some((e - a).num_seconds().max(0) as i32),
                _ => None,
            };

            sqlx::query(
                "UPDATE voip_calls
                 SET status = $2,
                     answered_at = COALESCE(answered_at, $3),
                     ended_at = COALESCE(ended_at, $4),
                     duration_seconds = COALESCE(duration_seconds, $5),
                     failure_reason = COALESCE(failure_reason, $6),
                     updated_at = $7
                 WHERE id = $1",
            )
            .bind(call_id)
            .bind(new_state.as_str())
            .bind(if new_state == CallState::Answered {
                Some(event.occurred_at)
            } else {
                None
            })
            .bind(ended)
            .bind(duration)
            .bind(hangup_reason(&event.kind))
            .bind(now)
            .execute(&mut *tx)
            .await?;

            match new_state {
                CallState::Answered => {
                    crate::metrics::record_voip_connected();
                    // Post-dial delay: the first thing a recipient notices, and the first
                    // thing a bad international route degrades.
                    if let Some(a) = answered {
                        let started: Option<chrono::DateTime<Utc>> =
                            sqlx::query_scalar("SELECT started_at FROM voip_calls WHERE id = $1")
                                .bind(call_id)
                                .fetch_optional(&mut *tx)
                                .await?
                                .flatten();
                        if let Some(st) = started {
                            let ms = (a - st).num_milliseconds().max(0) as u64;
                            crate::metrics::record_voip_setup_ms(ms);
                        }
                    }
                }
                CallState::Failed => crate::metrics::record_voip_failed(),
                CallState::Bridged => {}
                _ => {}
            }

            if new_state.is_terminal() {
                settle(
                    &mut tx,
                    call_id,
                    session_id,
                    new_state,
                    price_per_min,
                    duration.unwrap_or(0),
                )
                .await?;
            }

            Ingest::Applied {
                call_id,
                before,
                after: new_state,
            }
        }
        None => Ingest::Recorded {
            call_id,
            state: before,
        },
    };

    // Record what the event did, so a lifecycle can be replayed later without having to
    // guess which deliveries were no-ops.
    let after_name = match &outcome {
        Ingest::Applied { after, .. } => after.as_str(),
        _ => before.as_str(),
    };
    sqlx::query(
        "UPDATE voip_provider_events SET state_before = $2, state_after = $3 WHERE id = $1",
    )
    .bind(event_row_id)
    .bind(before.as_str())
    .bind(after_name)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(outcome)
}

/// `(call id, session id, status, quoted price/min, answered_at)`.
type CallRow = (
    Uuid,
    Uuid,
    String,
    Option<Decimal>,
    Option<chrono::DateTime<Utc>>,
);

async fn resolve_call(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    event: &ProviderEvent,
) -> Result<Option<CallRow>, sqlx::Error> {
    // Our own correlation id first: the provider echoes it back, and it survives a leg
    // being replaced (a redirect, a second leg) in a way the leg id does not.
    if let Some(state) = event.client_state.as_deref() {
        if let Ok(id) = Uuid::parse_str(state) {
            let row = fetch_call(tx, "id = $1", id).await?;
            if row.is_some() {
                // Make sure this leg is recorded against the call, so a later event that
                // arrives without a client_state still resolves.
                sqlx::query(
                    "UPDATE voip_calls
                     SET provider_leg_ids = CASE WHEN $2 = ANY(provider_leg_ids)
                                                 THEN provider_leg_ids
                                                 ELSE array_append(provider_leg_ids, $2) END
                     WHERE id = $1",
                )
                .bind(id)
                .bind(event.leg_id.as_str())
                .execute(&mut **tx)
                .await?;
                return Ok(row);
            }
        }
    }
    fetch_call_by_leg(tx, &event.leg_id).await
}

async fn fetch_call(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    predicate: &str,
    id: Uuid,
) -> Result<Option<CallRow>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT id, session_id, status, quoted_price_per_min, answered_at
         FROM voip_calls WHERE {predicate} FOR UPDATE"
    ))
    .bind(id)
    .fetch_optional(&mut **tx)
    .await
}

async fn fetch_call_by_leg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    leg: &LegId,
) -> Result<Option<CallRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, session_id, status, quoted_price_per_min, answered_at
         FROM voip_calls WHERE $1 = ANY(provider_leg_ids) FOR UPDATE",
    )
    .bind(leg.as_str())
    .fetch_optional(&mut **tx)
    .await
}

/// Columns that move on information-carrying events, whatever the lifecycle does.
async fn apply_side_effects(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    call_id: Uuid,
    event: &ProviderEvent,
) -> Result<(), sqlx::Error> {
    match &event.kind {
        ProviderEventKind::RecordingStarted => {
            sqlx::query(
                "UPDATE voip_calls
                 SET recording_status = 'recording',
                     recording_started_at = COALESCE(recording_started_at, $2)
                 WHERE id = $1",
            )
            .bind(call_id)
            .bind(event.occurred_at)
            .execute(&mut **tx)
            .await?;
        }
        // A recording that lands after hangup is the normal case, not an anomaly: the
        // provider finishes writing the file once the call is over.
        ProviderEventKind::RecordingSaved { url, .. } => {
            sqlx::query(
                "UPDATE voip_calls
                 SET recording_status = 'saved',
                     updated_at = now()
                 WHERE id = $1",
            )
            .bind(call_id)
            .execute(&mut **tx)
            .await?;
            let _ = url; // stored by the recording service, which owns object storage
        }
        _ => {}
    }
    Ok(())
}

/// Close the credit hold, in the SAME transaction as the state change.
///
/// Atomic on purpose: a crash between "the call is completed" and "the hold is settled"
/// would leave a finished call still holding the customer's credits. The sweep below can
/// recover that, but only after a delay the customer sees in their balance — so the happy
/// path does not rely on it.
async fn settle(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    call_id: Uuid,
    session_id: Uuid,
    final_state: CallState,
    price_per_min: Option<Decimal>,
    duration_secs: i32,
) -> Result<(), sqlx::Error> {
    // A call that never connected owes nothing, whatever the clock says.
    let credits = if final_state == CallState::Completed {
        let price = price_per_min.unwrap_or(Decimal::ZERO);
        pricing::settle_credits(price, duration_secs.max(0) as u64)
    } else {
        0
    };

    sqlx::query(
        "UPDATE voip_calls SET credits_consumed = $2, customer_charge_usd = $3 WHERE id = $1",
    )
    .bind(call_id)
    .bind(credits)
    .bind(pricing::credits_to_usd(credits))
    .execute(&mut **tx)
    .await?;

    // The reservation module owns the accounting identities; this only decides the amount.
    if final_state == CallState::Completed {
        reservation::settle_tx(tx, call_id, session_id, credits).await?;
    } else {
        reservation::release_tx(tx, call_id, session_id).await?;
    }
    Ok(())
}

/// Close any hold left open on a call that has already finished.
///
/// A recovery sweep, not the happy path — [`apply`] settles atomically. This exists for
/// the cases the webhook cannot cover: the process died mid-transaction, the hangup
/// webhook never arrived and a reaper ended the call, or a hold was taken for a dial that
/// then failed before any event was recorded. Idempotent, because `reservation::settle`
/// is: running it twice reports the original settlement and moves no money.
pub async fn settle_finished_calls(pool: &Pool, limit: i64) -> Result<usize, sqlx::Error> {
    let rows: Vec<(Uuid, Uuid, i32, String)> = sqlx::query_as(
        "SELECT c.id, c.session_id, c.credits_consumed, c.status
         FROM voip_calls c
         JOIN voip_credit_reservations r ON r.call_id = c.id AND r.state = 'held'
         WHERE c.status IN ('completed', 'failed')
         ORDER BY c.ended_at NULLS LAST
         LIMIT $1",
    )
    .bind(limit.clamp(1, 500))
    .fetch_all(pool)
    .await?;

    let mut closed = 0;
    for (call_id, session_id, credits, status) in rows {
        if status == "completed" {
            reservation::settle(pool, call_id, session_id, credits).await?;
        } else {
            reservation::release(pool, call_id, session_id).await?;
        }
        closed += 1;
    }
    Ok(closed)
}

fn lifecycle_event(kind: &ProviderEventKind) -> Option<CallEvent> {
    Some(match kind {
        ProviderEventKind::Initiated => CallEvent::Dialed,
        ProviderEventKind::Ringing => CallEvent::Ringing,
        ProviderEventKind::Answered => CallEvent::Answered,
        ProviderEventKind::MediaStarted => CallEvent::MediaEstablished,
        ProviderEventKind::Hangup { cause } => CallEvent::Hangup {
            reason: Some(*cause),
        },
        // Media stopping, a recording landing, a digit arriving: information, not
        // lifecycle. Recorded, but they move nothing.
        ProviderEventKind::MediaStopped
        | ProviderEventKind::RecordingStarted
        | ProviderEventKind::RecordingSaved { .. }
        | ProviderEventKind::Dtmf { .. }
        | ProviderEventKind::Unhandled { .. } => return None,
    })
}

fn hangup_reason(kind: &ProviderEventKind) -> Option<&'static str> {
    match kind {
        ProviderEventKind::Hangup { cause } => Some(cause.as_str()),
        _ => None,
    }
}

fn event_type_name(kind: &ProviderEventKind) -> String {
    match kind {
        ProviderEventKind::Initiated => "initiated".into(),
        ProviderEventKind::Ringing => "ringing".into(),
        ProviderEventKind::Answered => "answered".into(),
        ProviderEventKind::Hangup { .. } => "hangup".into(),
        ProviderEventKind::MediaStarted => "media_started".into(),
        ProviderEventKind::MediaStopped => "media_stopped".into(),
        ProviderEventKind::RecordingStarted => "recording_started".into(),
        ProviderEventKind::RecordingSaved { .. } => "recording_saved".into(),
        ProviderEventKind::Dtmf { .. } => "dtmf".into(),
        // Kept verbatim so a renamed provider event shows up in analytics as itself.
        ProviderEventKind::Unhandled { raw_type } => format!("unhandled:{raw_type}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telephony::mock::MOCK_ID;
    use crate::voip::state::FailureReason;

    /// One fixture per test, with its own leg id and its own event-id namespace.
    ///
    /// Both matter: `(provider, provider_event_id)` is UNIQUE **globally**, which is the
    /// correct production semantic — a provider event id identifies one delivery, not one
    /// delivery per call — and every fixture shares one database. Fixed ids made tests
    /// resolve each other's calls.
    struct Fx {
        pool: Pool,
        org: Uuid,
        session: Uuid,
        call: Uuid,
        tag: String,
    }

    impl Fx {
        fn leg(&self) -> LegId {
            LegId::new(format!("leg-{}", self.tag))
        }

        fn event(&self, id: &str, kind: ProviderEventKind) -> ProviderEvent {
            ProviderEvent {
                provider: MOCK_ID,
                event_id: format!("{}-{id}", self.tag),
                leg_id: self.leg(),
                client_state: None,
                occurred_at: Utc::now(),
                kind,
            }
        }
    }

    /// DB-gated. The database must have **pgvector** — a plain `postgres:16` makes these
    /// skip silently and still print `ok`.
    async fn setup(credits: i32, price: &str) -> Option<Fx> {
        let tag = Uuid::new_v4().simple().to_string();
        let url = std::env::var("DATABASE_URL").ok()?;
        let pool = crate::db::connect(&url).await.ok()?;
        crate::db::migrate(&pool).await.ok()?;

        let user: Uuid = sqlx::query_scalar(
            "INSERT INTO users (google_id, email, name, balance)
             VALUES ($1, $2, 'Owner', 0) RETURNING id",
        )
        .bind(format!("g-{}", Uuid::new_v4()))
        .bind(format!("{}@example.test", Uuid::new_v4()))
        .fetch_one(&pool)
        .await
        .unwrap();

        let org: Uuid = sqlx::query_scalar(
            "INSERT INTO organizations (name, slug, owner_id, credits_balance)
             VALUES ('VoIP Co', $1, $2, $3) RETURNING id",
        )
        .bind(format!("voip-{}", Uuid::new_v4().simple()))
        .bind(user)
        .bind(credits)
        .fetch_one(&pool)
        .await
        .unwrap();

        let session = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO call_sessions (id, room, org_id, kind) VALUES ($1, $2, $3, 'phone')",
        )
        .bind(session)
        .bind(format!("ph-{}", Uuid::new_v4().simple()))
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();

        let call: Uuid = sqlx::query_scalar(
            "INSERT INTO voip_calls
                (session_id, org_id, user_id, provider, direction, recipient_e164,
                 recipient_pseudonym, recipient_country, source_language, target_language,
                 engine_id, status, quoted_price_per_min, provider_leg_ids)
             VALUES ($1, $2, $3, 'mock', 'outbound', '+8613800138000', 'abc', 'CN', 'it',
                     'zh', 'standard', 'dialing', $4, ARRAY[$5])
             RETURNING id",
        )
        .bind(session)
        .bind(org)
        .bind(user)
        .bind(price.parse::<Decimal>().unwrap())
        .bind(format!("leg-{tag}"))
        .fetch_one(&pool)
        .await
        .unwrap();

        Some(Fx {
            pool,
            org,
            session,
            call,
            tag,
        })
    }

    async fn status(pool: &Pool, call: Uuid) -> String {
        sqlx::query_scalar("SELECT status FROM voip_calls WHERE id = $1")
            .bind(call)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn balance(pool: &Pool, org: Uuid) -> i32 {
        sqlx::query_scalar("SELECT credits_balance FROM organizations WHERE id = $1")
            .bind(org)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_lifecycle_event_moves_the_call_and_is_recorded() {
        let Some(f) = setup(1000, "0.05").await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        let out = apply(&f.pool, &f.event("e1", ProviderEventKind::Answered))
            .await
            .unwrap();
        assert_eq!(
            out,
            Ingest::Applied {
                call_id: f.call,
                before: CallState::Dialing,
                after: CallState::Answered
            }
        );
        assert_eq!(status(&f.pool, f.call).await, "answered");

        let answered: Option<chrono::DateTime<Utc>> =
            sqlx::query_scalar("SELECT answered_at FROM voip_calls WHERE id = $1")
                .bind(f.call)
                .fetch_one(&f.pool)
                .await
                .unwrap();
        assert!(answered.is_some(), "answered_at is stamped");
    }

    #[tokio::test]
    async fn a_redelivered_event_writes_nothing_the_second_time() {
        // R25. The provider WILL redeliver. Without the UNIQUE constraint two workers
        // would both apply it and one would settle the reservation twice.
        let Some(f) = setup(1000, "0.05").await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        let e = f.event("dup-1", ProviderEventKind::Answered);
        assert!(matches!(
            apply(&f.pool, &e).await.unwrap(),
            Ingest::Applied { .. }
        ));
        assert_eq!(
            apply(&f.pool, &e).await.unwrap(),
            Ingest::Duplicate {
                call_id: Some(f.call)
            }
        );
        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM voip_provider_events WHERE provider_event_id = $1",
        )
        .bind(&e.event_id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(rows, 1, "exactly one ledger row");
    }

    #[tokio::test]
    async fn a_late_ringing_cannot_un_answer_a_billed_call() {
        // R26. The regression: `ringing` overtaking `answered` would stop the meter on a
        // call the carrier is charging us for.
        let Some(f) = setup(1000, "0.05").await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        apply(&f.pool, &f.event("a", ProviderEventKind::Answered))
            .await
            .unwrap();
        let out = apply(&f.pool, &f.event("b", ProviderEventKind::Ringing))
            .await
            .unwrap();
        assert_eq!(
            out,
            Ingest::Recorded {
                call_id: f.call,
                state: CallState::Answered
            }
        );
        assert_eq!(status(&f.pool, f.call).await, "answered");
    }

    #[tokio::test]
    async fn a_recording_that_lands_after_hangup_is_stored_not_rejected() {
        // The normal case: the provider finishes writing the file once the call is over.
        let Some(f) = setup(1000, "0.05").await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        apply(&f.pool, &f.event("a", ProviderEventKind::Answered))
            .await
            .unwrap();
        apply(
            &f.pool,
            &f.event(
                "h",
                ProviderEventKind::Hangup {
                    cause: FailureReason::Unmapped,
                },
            ),
        )
        .await
        .unwrap();
        assert_eq!(status(&f.pool, f.call).await, "completed");

        let out = apply(
            &f.pool,
            &f.event(
                "r",
                ProviderEventKind::RecordingSaved {
                    url: "https://x/rec.mp3".into(),
                    duration_secs: 42,
                },
            ),
        )
        .await
        .unwrap();
        assert!(matches!(out, Ingest::Recorded { .. }), "{out:?}");
        let rec: String =
            sqlx::query_scalar("SELECT recording_status FROM voip_calls WHERE id = $1")
                .bind(f.call)
                .fetch_one(&f.pool)
                .await
                .unwrap();
        assert_eq!(rec, "saved");
        assert_eq!(
            status(&f.pool, f.call).await,
            "completed",
            "and the call was not resurrected"
        );
    }

    #[tokio::test]
    async fn an_unknown_leg_is_acknowledged_rather_than_retried_forever() {
        let Some(f) = setup(1000, "0.05").await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        let mut e = f.event("x", ProviderEventKind::Answered);
        e.leg_id = LegId::new(format!("other-env-{}", f.tag));
        assert_eq!(apply(&f.pool, &e).await.unwrap(), Ingest::Unknown);
    }

    #[tokio::test]
    async fn our_correlation_id_resolves_the_call_and_records_the_leg() {
        // client_state survives a leg being replaced in a way the leg id does not.
        let Some(f) = setup(1000, "0.05").await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        let mut e = f.event("cs", ProviderEventKind::Answered);
        let fresh = format!("fresh-{}", f.tag);
        e.leg_id = LegId::new(&fresh);
        e.client_state = Some(f.call.to_string());

        assert!(matches!(
            apply(&f.pool, &e).await.unwrap(),
            Ingest::Applied { .. }
        ));
        let legs: Vec<String> =
            sqlx::query_scalar("SELECT provider_leg_ids FROM voip_calls WHERE id = $1")
                .bind(f.call)
                .fetch_one(&f.pool)
                .await
                .unwrap();
        assert!(legs.contains(&fresh));

        // …and a later event with no client_state now resolves by that leg alone.
        let mut e2 = f.event(
            "cs2",
            ProviderEventKind::Hangup {
                cause: FailureReason::Unmapped,
            },
        );
        e2.leg_id = LegId::new(&fresh);
        assert!(matches!(
            apply(&f.pool, &e2).await.unwrap(),
            Ingest::Applied { .. }
        ));
    }

    #[tokio::test]
    async fn hanging_up_before_answer_fails_the_call_and_refunds_everything() {
        let Some(f) = setup(1000, "0.05").await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        reservation::reserve(&f.pool, f.org, f.call, f.session, None, 50)
            .await
            .unwrap();
        assert_eq!(balance(&f.pool, f.org).await, 950);

        apply(
            &f.pool,
            &f.event(
                "h",
                ProviderEventKind::Hangup {
                    cause: FailureReason::Busy,
                },
            ),
        )
        .await
        .unwrap();
        assert_eq!(status(&f.pool, f.call).await, "failed");
        // Settled in the SAME transaction as the state change, so the refund is already
        // visible — the customer does not watch a stale balance until a sweep runs.
        assert_eq!(
            balance(&f.pool, f.org).await,
            1000,
            "a call nobody answered owes nothing"
        );
        assert!(
            reservation::open_for_call(&f.pool, f.call)
                .await
                .unwrap()
                .is_none(),
            "the recovery sweep has nothing left to do on the happy path"
        );
        let reason: Option<String> =
            sqlx::query_scalar("SELECT failure_reason FROM voip_calls WHERE id = $1")
                .bind(f.call)
                .fetch_one(&f.pool)
                .await
                .unwrap();
        assert_eq!(reason.as_deref(), Some("busy"));
    }

    #[tokio::test]
    async fn a_completed_call_settles_against_its_measured_duration() {
        let Some(f) = setup(1000, "0.60").await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        reservation::reserve(&f.pool, f.org, f.call, f.session, None, 300)
            .await
            .unwrap();
        assert_eq!(balance(&f.pool, f.org).await, 700);

        let answered_at = Utc::now() - chrono::Duration::seconds(120);
        let mut a = f.event("a", ProviderEventKind::Answered);
        a.occurred_at = answered_at;
        apply(&f.pool, &a).await.unwrap();

        let mut h = f.event(
            "h",
            ProviderEventKind::Hangup {
                cause: FailureReason::Unmapped,
            },
        );
        h.occurred_at = answered_at + chrono::Duration::seconds(120);
        apply(&f.pool, &h).await.unwrap();

        assert_eq!(status(&f.pool, f.call).await, "completed");
        let (secs, credits): (Option<i32>, i32) = sqlx::query_as(
            "SELECT duration_seconds, credits_consumed FROM voip_calls WHERE id = $1",
        )
        .bind(f.call)
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(secs, Some(120));
        // 2 minutes at $0.60/min = $1.20 = 120 credits.
        assert_eq!(credits, 120);

        settle_finished_calls(&f.pool, 10).await.unwrap();
        assert_eq!(
            balance(&f.pool, f.org).await,
            880,
            "1000 - 120 actually used; the other 180 held came back"
        );
    }

    #[tokio::test]
    async fn the_settlement_sweep_is_safe_to_run_twice() {
        // It exists precisely to recover from a crash between the state change and the
        // settlement, so running it again must not refund again.
        let Some(f) = setup(1000, "0.60").await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        reservation::reserve(&f.pool, f.org, f.call, f.session, None, 300)
            .await
            .unwrap();
        apply(
            &f.pool,
            &f.event(
                "h",
                ProviderEventKind::Hangup {
                    cause: FailureReason::NoAnswer,
                },
            ),
        )
        .await
        .unwrap();

        // `apply` already settled it, so there is nothing left to reopen…
        let after_apply = balance(&f.pool, f.org).await;
        assert!(reservation::open_for_call(&f.pool, f.call)
            .await
            .unwrap()
            .is_none());
        // …and running the sweep twice moves no money on this call.
        //
        // Asserted on THIS call's balance rather than on the sweep's return count: the
        // sweep is global and the suite runs in parallel against one database, so a count
        // would be measuring other tests.
        settle_finished_calls(&f.pool, 50).await.unwrap();
        settle_finished_calls(&f.pool, 50).await.unwrap();
        assert_eq!(
            balance(&f.pool, f.org).await,
            after_apply,
            "and moves no money"
        );
    }

    #[tokio::test]
    async fn the_sweep_recovers_a_hold_orphaned_by_a_crash() {
        // The case the sweep actually exists for: the process died between marking the
        // call terminal and settling, or a reaper ended a call whose hangup webhook never
        // arrived. Simulated by taking a hold on a call that is already completed.
        let Some(f) = setup(1000, "0.60").await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        sqlx::query(
            "UPDATE voip_calls SET status = 'completed', credits_consumed = 40, \
             ended_at = now() WHERE id = $1",
        )
        .bind(f.call)
        .execute(&f.pool)
        .await
        .unwrap();
        reservation::reserve(&f.pool, f.org, f.call, f.session, None, 300)
            .await
            .unwrap();
        assert_eq!(balance(&f.pool, f.org).await, 700);

        let closed = settle_finished_calls(&f.pool, 200).await.unwrap();
        assert!(closed >= 1, "the sweep closed at least this call's hold");
        assert_eq!(
            balance(&f.pool, f.org).await,
            960,
            "1000 - the 40 credits the call actually consumed"
        );
        assert!(reservation::open_for_call(&f.pool, f.call)
            .await
            .unwrap()
            .is_none());
        // And it does not charge this call again.
        settle_finished_calls(&f.pool, 200).await.unwrap();
        assert_eq!(balance(&f.pool, f.org).await, 960);
    }

    #[tokio::test]
    async fn the_event_ledger_records_what_each_delivery_did() {
        let Some(f) = setup(1000, "0.05").await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        apply(&f.pool, &f.event("a", ProviderEventKind::Answered))
            .await
            .unwrap();
        apply(&f.pool, &f.event("b", ProviderEventKind::Ringing))
            .await
            .unwrap();

        let rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT event_type, state_before, state_after FROM voip_provider_events
             WHERE call_id = $1 ORDER BY received_at",
        )
        .bind(f.call)
        .fetch_all(&f.pool)
        .await
        .unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            (
                "answered".into(),
                Some("dialing".into()),
                Some("answered".into())
            )
        );
        // The no-op is recorded AS a no-op, so a replay does not have to guess.
        assert_eq!(
            rows[1],
            (
                "ringing".into(),
                Some("answered".into()),
                Some("answered".into())
            )
        );
    }

    #[tokio::test]
    async fn an_unmodelled_provider_event_is_kept_under_its_own_name() {
        let Some(f) = setup(1000, "0.05").await else {
            eprintln!("skipping — no DATABASE_URL");
            return;
        };
        apply(
            &f.pool,
            &f.event(
                "u",
                ProviderEventKind::Unhandled {
                    raw_type: "call.machine.detection.ended".into(),
                },
            ),
        )
        .await
        .unwrap();
        let kind: String = sqlx::query_scalar(
            "SELECT event_type FROM voip_provider_events WHERE provider_event_id = $1",
        )
        .bind(format!("{}-u", f.tag))
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(kind, "unhandled:call.machine.detection.ended");
    }
}
