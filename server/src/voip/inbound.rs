//! Somebody is calling us (spec 0116).
//!
//! Spec 0111 §2 said it plainly: *"The abstraction admits them; the orchestration does
//! not yet."* Every piece an inbound call needs was already built and direction-agnostic —
//! the consent announcement, the media pump, the codec, the room assembly, the meter. What
//! was missing is the half-dozen decisions that turn *a stranger is ringing one of your
//! numbers* into *somebody in your organisation is talking to them, in their own language*.
//!
//! Those decisions, in order:
//!
//! 1. **Is this number ours, and does it take calls?** A number we do not own, or one with
//!    inbound off, is refused and creates nothing.
//! 2. **Who is calling?** The address book answers when it can (spec 0114), and the
//!    language recorded for THAT number is the one they are answered in.
//! 3. **Who should be rung?** The number's routing says. When it says nobody, the
//!    organisation's owners are rung — a call nobody is told about is worse than a call the
//!    wrong person takes.
//! 4. **What if nobody comes?** A clock decides, not hope. `voicemail`, `forward` or a
//!    polite refusal — never ringing for ever.

#![allow(clippy::result_large_err)]

use chrono::{Duration, Utc};
use uuid::Uuid;

use crate::telephony::{TelephonyProvider, E164};
use crate::voip::session;

/// Why an incoming call was turned away. Logged, never spoken to the caller: a stranger
/// dialling a wrong number is owed a normal busy tone, not an explanation of our schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The deployment has not switched inbound on.
    Disabled,
    /// Not a number this install owns.
    UnknownNumber,
    /// Ours, but not taking calls.
    InboundOff,
    /// The organisation's subscription has lapsed, or the org switched VoIP off.
    NotEntitled,
}

impl Refusal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "inbound_disabled",
            Self::UnknownNumber => "unknown_number",
            Self::InboundOff => "inbound_off",
            Self::NotEntitled => "not_entitled",
        }
    }
}

/// What the routing decided, resolved to actual people.
#[derive(Debug, Clone)]
pub struct Ring {
    pub user_ids: Vec<Uuid>,
    pub seconds: i32,
    pub no_answer_action: String,
    pub forward_to: Option<String>,
    pub stranger_language: Option<String>,
}

/// The number that was rung, and the organisation behind it.
#[derive(Debug, Clone)]
pub struct InboundNumber {
    pub number_id: Uuid,
    pub org_id: Uuid,
    pub e164: String,
}

/// Find the number that was rung, if we own it and it takes calls.
pub async fn number_for(
    pool: &crate::db::Pool,
    to: &E164,
) -> Result<Result<InboundNumber, Refusal>, sqlx::Error> {
    let row: Option<(Uuid, Uuid, String, bool, String)> = sqlx::query_as(
        "SELECT id, org_id, e164, inbound_enabled, status
           FROM voip_numbers WHERE e164 = $1",
    )
    .bind(to.as_str())
    .fetch_optional(pool)
    .await?;

    let Some((number_id, org_id, e164, inbound_enabled, status)) = row else {
        return Ok(Err(Refusal::UnknownNumber));
    };
    // A suspended number is one whose renewal went unpaid. It keeps its identity — that is
    // the whole point of suspending rather than releasing — but it does not carry traffic.
    if !inbound_enabled || status != "active" {
        return Ok(Err(Refusal::InboundOff));
    }
    Ok(Ok(InboundNumber {
        number_id,
        org_id,
        e164,
    }))
}

/// One routing row, as the database returns it.
///
/// Named rather than left as a seven-tuple: the columns are heterogeneous enough that
/// position stops meaning anything, and a transposition between two `Option<String>`s
/// would compile.
#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct RoutingRow {
    pub ring_mode: String,
    pub ring_user_ids: Vec<Uuid>,
    pub ring_team_id: Option<Uuid>,
    pub ring_seconds: i32,
    pub no_answer_action: String,
    pub forward_to: Option<String>,
    pub stranger_language: Option<String>,
}

impl RoutingRow {
    /// What happens to a number nobody has configured: ring the owners, and take a
    /// message. Returned by the read endpoint too, so the form opens showing what will
    /// actually happen rather than blank.
    pub fn default_for(ring_seconds: i32) -> Self {
        Self {
            ring_mode: "owners".into(),
            ring_user_ids: Vec::new(),
            ring_team_id: None,
            ring_seconds,
            no_answer_action: "voicemail".into(),
            forward_to: None,
            stranger_language: None,
        }
    }
}

/// Resolve the number's routing into the people to ring.
///
/// Falls back to the organisation's owners in three separate cases — no routing row, a
/// `users` list that has emptied out, and a team that was deleted or has no members. All
/// three are the same failure from a caller's point of view (the telephone rings and
/// nobody is told), and all three are recoverable by ringing the people who are ultimately
/// responsible for the account.
pub async fn resolve_ring(
    pool: &crate::db::Pool,
    number_id: Uuid,
    org_id: Uuid,
    default_seconds: i32,
) -> Result<Ring, sqlx::Error> {
    let row: Option<RoutingRow> = sqlx::query_as(
        "SELECT ring_mode, ring_user_ids, ring_team_id, ring_seconds, no_answer_action,
                    forward_to, stranger_language
               FROM voip_number_routing WHERE number_id = $1",
    )
    .bind(number_id)
    .fetch_optional(pool)
    .await?;

    // No row at all is the same decision as a row that says `owners`: an organisation
    // that bought a number and gave it to somebody has not opted out of being rung.
    let row = row.unwrap_or_else(|| RoutingRow::default_for(default_seconds));

    let mut user_ids: Vec<Uuid> = match row.ring_mode.as_str() {
        "users" => row.ring_user_ids.clone(),
        "team" => match row.ring_team_id {
            Some(team_id) => {
                sqlx::query_scalar("SELECT user_id FROM team_members WHERE team_id = $1")
                    .bind(team_id)
                    .fetch_all(pool)
                    .await?
            }
            None => Vec::new(),
        },
        _ => Vec::new(),
    };

    if user_ids.is_empty() {
        user_ids = sqlx::query_scalar(
            "SELECT user_id FROM organization_members
              WHERE org_id = $1 AND role IN ('owner', 'admin')",
        )
        .bind(org_id)
        .fetch_all(pool)
        .await?;
    }

    Ok(Ring {
        user_ids,
        seconds: row.ring_seconds,
        no_answer_action: row.no_answer_action,
        forward_to: row.forward_to,
        stranger_language: row.stranger_language,
    })
}

/// Who is calling, and what language to answer them in.
///
/// The address book's answer wins: a contact's number carries its own language (spec 0114),
/// and that is a fact about the person rather than a guess about their country.
pub async fn identify_caller(
    pool: &crate::db::Pool,
    org_id: Uuid,
    from: &E164,
    stranger_language: Option<&str>,
) -> Result<(Option<Uuid>, Option<String>, String), sqlx::Error> {
    let row: Option<(Uuid, String, Option<String>)> = sqlx::query_as(
        "SELECT c.id, c.name, n.language
           FROM voip_contact_numbers n
           JOIN voip_contacts c ON c.id = n.contact_id
          WHERE n.org_id = $1 AND n.e164 = $2",
    )
    .bind(org_id)
    .bind(from.as_str())
    .fetch_optional(pool)
    .await?;

    match row {
        Some((id, name, language)) => Ok((
            Some(id),
            Some(name),
            language
                .or_else(|| stranger_language.map(str::to_string))
                .unwrap_or_else(|| "en".into()),
        )),
        None => Ok((None, None, stranger_language.unwrap_or("en").to_string())),
    }
}

/// Tell the people who should answer.
///
/// The notification system the product already has, rather than a second one: it writes a
/// row, respects each user's preferences and sends web push. An inbound call is a
/// notification with a join link — the same link the outbound dialer already hands out.
///
/// This is honestly not a ringing desk phone, and §8.2 of the spec says so. There is no
/// persistent socket to the dashboard to ring down.
pub async fn ring_users(
    state: &crate::AppState,
    pool: &crate::db::Pool,
    user_ids: &[Uuid],
    room: &str,
    caller: &str,
    on_number: &str,
) {
    for user_id in user_ids {
        // Each colleague in THEIR OWN language, resolved per recipient — the same shape
        // `business/meetings.rs` uses, and the reason it bothers: `notify` feeds this
        // language into the email chrome as well as the body, so hardcoding it makes the
        // whole message English for everyone regardless of who they are.
        let lang = crate::notify_copy::user_locale(pool, *user_id).await;
        let (title, body) = crate::notify_copy::voip_copy(&lang, caller, on_number);
        crate::notifications::notify(
            state,
            *user_id,
            "voip_inbound",
            &lang,
            &title,
            &body,
            serde_json::json!({ "room_code": room, "kind": "voip_inbound" }),
        )
        .await;
    }
}

/// One unanswered call, as the sweep reads it.
#[derive(Debug, sqlx::FromRow)]
struct UnansweredRow {
    id: Uuid,
    provider_leg_ids: Vec<String>,
    no_answer_action: Option<String>,
    /// Distinguishes the sweep's TWO passes over the same call: the ring timeout, then
    /// closing the voicemail once the caller has had their two minutes.
    missed: bool,
    /// What this caller was ANSWERED in, so the message prompt is in their language.
    target_language: String,
}

/// Calls nobody came to (spec 0116 R4).
///
/// Only a clock can notice an absence — the same reasoning `fail_stalled_calls` is built
/// on. Returns how many were closed.
pub async fn sweep_unanswered(
    state: &crate::AppState,
    pool: &crate::db::Pool,
    provider: &dyn TelephonyProvider,
    batch: i64,
) -> Result<u64, sqlx::Error> {
    // `no_answer_action` comes through a LEFT JOIN and is NULL for a number with no
    // routing row — which is the common case, not an error. `Option` here rather than a
    // COALESCE in the SQL, so the default lives in one place: `resolve_ring`.
    // `missed` distinguishes the TWO passes this sweep makes over the same call. The
    // first is the ring timeout; the second closes a voicemail once the caller has had
    // their two minutes. Without it the second pass would replay the prompt for ever,
    // because a call taking a message is still unanswered and still has a deadline.
    let due: Vec<UnansweredRow> = sqlx::query_as(
        "SELECT id, provider_leg_ids, no_answer_action, missed, c.target_language
           FROM voip_calls c
           LEFT JOIN LATERAL (
                SELECT r.no_answer_action
                  FROM voip_number_routing r
                 WHERE r.number_id = c.inbound_number_id
           ) route ON TRUE
          WHERE c.direction = 'inbound'
            AND c.answered_by IS NULL
            AND c.ring_deadline_at IS NOT NULL
            AND c.ring_deadline_at <= now()
            AND c.status NOT IN ('completed', 'failed')
          LIMIT $1",
    )
    .bind(batch)
    .fetch_all(pool)
    .await?;

    let mut closed = 0u64;
    for row in due {
        let (call_id, legs, action, already_missed, language) = (
            row.id,
            row.provider_leg_ids,
            row.no_answer_action,
            row.missed,
            row.target_language,
        );
        if already_missed {
            // Second pass: the message is over. Stop the recording, hang up, and let the
            // ordinary `RecordingSaved` webhook attach what was said.
            for leg in &legs {
                let leg_id = crate::telephony::LegId::new(leg.clone());
                let _ = provider.stop_recording(&leg_id).await;
                let _ = provider.hangup(&leg_id).await;
            }
            sqlx::query(
                "UPDATE voip_calls
                    SET status = 'completed', ended_at = COALESCE(ended_at, now()),
                        ring_deadline_at = NULL
                  WHERE id = $1",
            )
            .bind(call_id)
            .execute(pool)
            .await?;
            session::reclaim(state, call_id);
            closed += 1;
            tracing::info!(call = %call_id, "voicemail closed");
            continue;
        }

        // Marked missed BEFORE the carrier is touched. A hangup that fails must not leave a
        // call ringing in our records for ever — the record of the miss is what the
        // organisation needs, and it is cheap to write.
        sqlx::query(
            "UPDATE voip_calls
                SET missed = TRUE, failure_reason = COALESCE(failure_reason, 'no_answer'),
                    ring_deadline_at = NULL
              WHERE id = $1",
        )
        .bind(call_id)
        .execute(pool)
        .await?;

        let leaves_a_message = action.as_deref() == Some("voicemail");
        for leg in &legs {
            let leg_id = crate::telephony::LegId::new(leg.clone());
            if leaves_a_message {
                // Say so, then record. The caller is told what is about to happen before
                // it happens — the same rule the consent announcement follows, and for the
                // same reason: somebody being recorded is owed the sentence beforehand.
                // In the language this caller was ANSWERED in. `identify_caller`
                // resolved it at admission and wrote it to `target_language`; a missed
                // call is one nobody picked up, which is a different thing from one where
                // nobody was identified.
                let (text, fell_back) = crate::voip::consent::voicemail_prompt(&language);
                if fell_back {
                    // A compliance fact, not a cosmetic one — the same reason
                    // `announcement` reports its own fallback.
                    tracing::warn!(
                        call = %call_id,
                        %language,
                        "voicemail prompt fell back to English"
                    );
                }
                let _ = provider
                    .play(
                        &leg_id,
                        crate::telephony::PlayRequest::Speak {
                            text,
                            language: language.clone(),
                            voice: None,
                        },
                    )
                    .await;
                let _ = provider
                    .start_recording(
                        &leg_id,
                        crate::telephony::RecordingConfig {
                            dual_channel: false,
                            beep: true,
                        },
                    )
                    .await;
            } else {
                let _ = provider.hangup(&leg_id).await;
            }
        }

        if leaves_a_message {
            // Still live, deliberately: the caller is speaking. A second deadline closes it,
            // and the recording lands through the ordinary `RecordingSaved` webhook that
            // already attaches a recording to its call.
            sqlx::query(
                "UPDATE voip_calls
                    SET status = 'answered', recording_status = 'pending',
                        ring_deadline_at = now() + make_interval(secs => $2::int)
                  WHERE id = $1",
            )
            .bind(call_id)
            .bind(VOICEMAIL_SECONDS)
            .execute(pool)
            .await?;
            tracing::info!(call = %call_id, "inbound call unanswered: taking a message");
        } else {
            sqlx::query(
                "UPDATE voip_calls
                    SET status = 'completed', ended_at = COALESCE(ended_at, now())
                  WHERE id = $1",
            )
            .bind(call_id)
            .execute(pool)
            .await?;
            session::reclaim(state, call_id);
            tracing::info!(call = %call_id, "inbound call missed: nobody answered in time");
        }
        closed += 1;
    }
    Ok(closed)
}

/// How long a caller may speak before the line is closed. Long enough for a real message,
/// short enough that a forgotten handset does not bill for an hour.
const VOICEMAIL_SECONDS: i32 = 120;

/// How long to ring, clamped to what the schema allows.
pub fn ring_deadline(seconds: i32) -> chrono::DateTime<Utc> {
    Utc::now() + Duration::seconds(seconds.clamp(5, 120) as i64)
}

/// Admit an incoming call: identify it, create it, answer it, and ring somebody.
///
/// Returns the new call's id, or the reason it was turned away. A refusal hangs the leg up
/// and writes nothing: a stranger dialling a wrong number is owed a normal busy tone, not
/// a row in somebody's history.
pub async fn admit(
    state: &crate::AppState,
    pool: &crate::db::Pool,
    provider: &dyn TelephonyProvider,
    leg: &crate::telephony::LegId,
    from_raw: &str,
    to_raw: &str,
) -> Result<Uuid, Refusal> {
    let cfg = match state.config.voip.as_ref() {
        Some(c) if c.inbound_enabled => c,
        _ => return Err(Refusal::Disabled),
    };

    let (Ok(from), Ok(to)) = (E164::parse(from_raw), E164::parse(to_raw)) else {
        // A carrier that sends us an unparseable number is not something a customer can
        // fix, and guessing which of the two was malformed would not help them either.
        return Err(Refusal::UnknownNumber);
    };

    let number = match number_for(pool, &to).await {
        Ok(Ok(n)) => n,
        Ok(Err(r)) => return Err(r),
        Err(e) => {
            tracing::error!("inbound number lookup failed: {e}");
            return Err(Refusal::UnknownNumber);
        }
    };

    // The same gate an outbound call passes. An organisation whose subscription has lapsed
    // does not get a free translated call because the caller dialled in rather than out.
    let entitled = crate::business::credits::org_subscription_active(pool, number.org_id)
        .await
        .unwrap_or(false);
    if !entitled {
        return Err(Refusal::NotEntitled);
    }

    // Is the office even open? Checked before anybody is rung, because a menu that offers
    // departments nobody is in is worse than a closed sign (spec 0118 R4).
    let hours: Option<(String, Vec<i32>, Vec<i32>, String)> = sqlx::query_as(
        "SELECT timezone, opens_at, closes_at, closed_action
           FROM voip_business_hours WHERE number_id = $1",
    )
    .bind(number.number_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    let open = match &hours {
        Some((tz, opens, closes, _)) => {
            let week = crate::voip::hours::Week::from_arrays(opens, closes);
            crate::voip::hours::is_open(chrono::Utc::now(), tz, Some(&week))
        }
        // No hours configured means always open: an organisation that has not said
        // otherwise has not asked to be closed.
        None => true,
    };

    let ring = resolve_ring(
        pool,
        number.number_id,
        number.org_id,
        cfg.ring_seconds_default,
    )
    .await
    .map_err(|e| {
        tracing::error!("inbound routing lookup failed: {e}");
        Refusal::UnknownNumber
    })?;

    let (contact_id, contact_name, language) = identify_caller(
        pool,
        number.org_id,
        &from,
        ring.stranger_language.as_deref(),
    )
    .await
    .unwrap_or((None, None, "en".into()));

    // The room exists before the call row and its session id becomes the call's — exactly
    // as an outbound call does it, and for the same reason: the human joins this same room
    // by the ordinary path, and a session id of its own would split the conversation.
    let engine_id = state.engines.default().metadata().id.clone();
    let room = format!("ph-{}", Uuid::new_v4().simple());
    let peer = session::create_phone_peer(&state.rooms, &room, &engine_id, &language)
        .map_err(|_| Refusal::NotEntitled)?;
    let session_id = peer.session_id;
    let guard = session::PeerGuard::new(&state.rooms, &peer);

    let deadline = ring_deadline(ring.seconds);
    let pseudonym = from.pseudonym(&cfg.pseudonym_key);

    let mut tx = match pool.begin().await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("inbound admission could not open a transaction: {e}");
            return Err(Refusal::UnknownNumber);
        }
    };

    let created: Result<Uuid, sqlx::Error> = async {
        sqlx::query(
            "INSERT INTO call_sessions (id, room, org_id, kind) VALUES ($1, $2, $3, 'phone')",
        )
        .bind(session_id)
        .bind(&room)
        .bind(number.org_id)
        .execute(&mut *tx)
        .await?;

        sqlx::query_scalar(
            "INSERT INTO voip_calls
                (session_id, org_id, user_id, provider, provider_region, direction,
                 caller_e164, recipient_e164, recipient_pseudonym, recipient_country,
                 source_language, target_language, engine_id, status, consent_policy,
                 consent_status, provider_leg_ids, contact_id, inbound_number_id,
                 rang_user_ids, ring_deadline_at)
             VALUES ($1, $2, NULL, $3, $4, 'inbound', $5, $6, $7, $8, $9, $9, $10,
                     'ringing', 'notice_only', 'not_required', ARRAY[$11], $12, $13, $14, $15)
             RETURNING id",
        )
        .bind(session_id)
        .bind(number.org_id)
        .bind(&cfg.provider)
        .bind(&cfg.default_region)
        // The number of OURS that was rung is the caller id the far end saw; the stranger
        // who rang it is the "recipient" of our translation, which is the column the rest
        // of the pipeline already reads.
        .bind(&number.e164)
        .bind(from.as_str())
        .bind(&pseudonym)
        .bind(from.region())
        .bind(&language)
        .bind(&engine_id)
        .bind(leg.as_str())
        .bind(contact_id)
        .bind(number.number_id)
        .bind(&ring.user_ids)
        .bind(deadline)
        .fetch_one(&mut *tx)
        .await
    }
    .await;

    let call_id = match created {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("inbound admission insert failed: {e}");
            return Err(Refusal::UnknownNumber);
        }
    };
    if let Err(e) = tx.commit().await {
        tracing::error!("inbound admission commit failed: {e}");
        return Err(Refusal::UnknownNumber);
    }

    // Park the leg BEFORE answering, the way `dial` parks before dialing: a carrier that
    // connects its media socket the instant we answer must find the room already waiting.
    //
    // `user_id` is None: nobody has picked up yet. It is filled in when somebody does,
    // which is also what makes a missed call a fact rather than an inference.
    state
        .voip_calls
        .park_peer(peer, call_id, number.org_id, None, &engine_id, &language);
    guard.disarm();

    if let Err(e) = provider.answer(leg).await {
        tracing::error!(call = %call_id, "inbound answer failed: {e:?}");
    }

    if open {
        ring_users(
            state,
            pool,
            &ring.user_ids,
            &room,
            contact_name.as_deref().unwrap_or(&from.masked()),
            &number.e164,
        )
        .await;
    } else {
        // Closed. Nobody is rung, and the deadline is brought forward to now so the sweep
        // applies the closed-hours action on its next pass — which is the same machinery
        // that handles nobody answering, rather than a second copy of it.
        let closed_action = hours
            .as_ref()
            .map(|(_, _, _, a)| a.clone())
            .unwrap_or_else(|| "voicemail".into());
        sqlx::query(
            "UPDATE voip_calls
                SET ring_deadline_at = now(), rang_user_ids = '{}',
                    failure_reason = 'outside_business_hours'
              WHERE id = $1",
        )
        .bind(call_id)
        .execute(pool)
        .await
        .ok();
        tracing::info!(call = %call_id, action = %closed_action, "inbound call outside business hours");
    }

    tracing::info!(
        call = %call_id,
        org = %number.org_id,
        rang = ring.user_ids.len(),
        known = contact_id.is_some(),
        "inbound call admitted"
    );
    Ok(call_id)
}
