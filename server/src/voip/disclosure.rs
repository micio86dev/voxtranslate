//! Executing the consent plan on a live call (spec 0111, R19–R21, D7).
//!
//! [`crate::voip::consent`] decides *what* the recipient should hear and whether we wait
//! for an answer. This module is what actually happens on the leg: speak it, ask for the
//! digit, interpret the digit, and only then start capturing anything.
//!
//! ## The one rule
//!
//! **Nothing is captured before the announcement has been spoken and, where a gate
//! applies, granted.** Every branch here fails towards *not capturing*: a provider that
//! cannot speak, an announcement that errors, a digit that never arrives, a row that
//! cannot be read — all of them leave the call running and the recorder off. That
//! asymmetry is deliberate. A call that is not recorded is a lost feature; a call that is
//! recorded without disclosure is an incident with a regulator attached.
//!
//! ## The evidence
//!
//! `disclosure_played_at`, `disclosure_language`, `consent_status`, `consent_received_at`
//! and `recording_started_at` are not telemetry. They are the answer to "who agreed to
//! this, in what language, and when", asked months later by someone who is not us. They
//! are written in the same transaction as the action they describe so the order on the row
//! is the order that actually happened.

use chrono::{DateTime, Utc};
use sqlx::Row;
use uuid::Uuid;

use crate::db::Pool;
use crate::telephony::{GatherConfig, LegId, PlayRequest, RecordingConfig, TelephonyProvider};
use crate::voip::consent::{
    self, AfterConsent, CaptureIntent, ConsentPolicy, ConsentStatus, Disclosure, RefusedAction,
};

/// How long the recipient has to press a key before we stop waiting.
///
/// Short enough that a call does not open with a long silence, long enough that someone
/// holding a handset can react. A timeout is NOT consent — see [`consent::after`].
pub const GATHER_TIMEOUT_SECS: u32 = 10;

/// Digits that count as agreement. One key, and it is the obvious one.
pub const ACCEPT_DIGITS: &str = "1";

/// What the disclosure step ended up doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// Nothing had to be said, because nothing is being captured and no AI disclosure is
    /// required. Capture, if it were configured, would already have produced a body.
    NothingToSay,
    /// Spoken, with no gate. Capture may begin.
    Announced { language: String, fell_back: bool },
    /// Spoken, and we are waiting for a digit. Capture may NOT begin yet.
    AwaitingConsent { language: String, fell_back: bool },
    /// The announcement could not be delivered. The call continues; capture does not start.
    ///
    /// Kept as a distinct outcome rather than an error because it is not a failure of the
    /// call — the two people can talk perfectly well. It is a failure of the *permission*
    /// to record them, and the correct response is to not record them.
    NotAnnounced { reason: &'static str },
}

impl Delivery {
    /// Whether capture may start as a result of this delivery.
    pub fn permits_capture(&self) -> bool {
        matches!(self, Delivery::NothingToSay | Delivery::Announced { .. })
    }
}

/// Speak the disclosure and, where the policy asks for one, open the DTMF gate.
///
/// Called once, on `call.answered`, before any capture is armed.
pub async fn deliver(
    pool: &Pool,
    provider: &dyn TelephonyProvider,
    call_id: Uuid,
    leg: &LegId,
    disclosure: &Disclosure,
    refused: RefusedAction,
) -> Result<Delivery, sqlx::Error> {
    let Some(ann) = consent::announcement(disclosure, refused) else {
        return Ok(Delivery::NothingToSay);
    };

    // A capability, checked before the call rather than inferred from an API error: a
    // provider that cannot speak must not silently end up recording.
    if !provider.metadata().capabilities.speech_synthesis {
        tracing::warn!(
            %call_id,
            "provider cannot speak the consent announcement; capture will not start"
        );
        return Ok(Delivery::NotAnnounced {
            reason: "provider_cannot_speak",
        });
    }

    if let Err(e) = provider
        .play(
            leg,
            PlayRequest::Speak {
                text: ann.text.clone(),
                language: ann.language.clone(),
                voice: None,
            },
        )
        .await
    {
        crate::metrics::record_voip_provider_error();
        tracing::error!(%call_id, error = %e, "consent announcement failed to play");
        return Ok(Delivery::NotAnnounced {
            reason: "announcement_failed",
        });
    }

    // Stamped only after the provider accepted it. Writing it first would put a disclosure
    // on the record that was never spoken, which is worse than having none.
    sqlx::query(
        "UPDATE voip_calls
         SET disclosure_played_at = COALESCE(disclosure_played_at, now()),
             disclosure_language = $2,
             updated_at = now()
         WHERE id = $1",
    )
    .bind(call_id)
    .bind(&ann.language)
    .execute(pool)
    .await?;

    if ann.fell_back {
        // Not cosmetic. The recipient was addressed in a language they may not read, and
        // that is a fact about the quality of the consent we obtained.
        tracing::warn!(
            %call_id,
            requested = %disclosure.language,
            "no disclosure copy for the recipient's language; spoke English"
        );
    }

    if disclosure.gate.is_none() {
        return Ok(Delivery::Announced {
            language: ann.language,
            fell_back: ann.fell_back,
        });
    }

    if let Err(e) = provider
        .gather(
            leg,
            GatherConfig {
                valid_digits: ACCEPT_DIGITS.to_string(),
                max_digits: 1,
                timeout_secs: GATHER_TIMEOUT_SECS,
            },
        )
        .await
    {
        crate::metrics::record_voip_provider_error();
        tracing::error!(%call_id, error = %e, "consent gate could not be opened");
        // Announced but ungated: the recipient was told, and nobody can answer. Pending is
        // the honest status, and pending does not permit capture.
        return Ok(Delivery::NotAnnounced {
            reason: "gather_failed",
        });
    }

    Ok(Delivery::AwaitingConsent {
        language: ann.language,
        fell_back: ann.fell_back,
    })
}

/// The whole disclosure step, driven from the `call.answered` webhook.
///
/// Rebuilds the plan from the row, speaks it, opens the gate if there is one, and — when
/// there is no gate to wait for — starts capture immediately. When the announcement could
/// not be delivered at all, capture is switched **off** rather than left pending: a
/// pending row is one the operator might later resolve by hand, and there is nothing to
/// resolve when the recipient was never told.
pub async fn announce_on_answer(
    pool: &Pool,
    provider: &dyn TelephonyProvider,
    call_id: Uuid,
    leg: &LegId,
) -> Result<Delivery, sqlx::Error> {
    let Some(row) = load(pool, call_id).await? else {
        return Ok(Delivery::NothingToSay);
    };
    let plan = row.plan();

    if let Some(reason) = plan.policy_escalated {
        // An admin turned the consent step off while capture was on, and the plan raised
        // it back. Overriding a customer's setting silently is its own kind of wrong.
        tracing::warn!(%call_id, reason, "consent policy was overridden");
    }

    let delivery = deliver(pool, provider, call_id, leg, &plan, row.refused).await?;

    match &delivery {
        Delivery::Announced { .. } | Delivery::NothingToSay => {
            // No gate: the announcement itself is the disclosure and it has been made.
            // Re-read so `start_capture` sees the stamp it just wrote.
            if let Some(fresh) = load(pool, call_id).await? {
                start_capture(pool, provider, call_id, leg, &fresh).await?;
            }
        }
        Delivery::AwaitingConsent { .. } => {}
        Delivery::NotAnnounced { reason } => {
            crate::metrics::record_voip_disclosure_failure();
            tracing::error!(%call_id, reason, "capture disabled: the recipient was never told");
            sqlx::query(
                "UPDATE voip_calls
                 SET recording_status = CASE WHEN recording_status = 'pending' THEN 'none'
                                             ELSE recording_status END,
                     transcription_status = CASE WHEN transcription_status = 'live' THEN 'none'
                                                 ELSE transcription_status END,
                     updated_at = now()
                 WHERE id = $1",
            )
            .bind(call_id)
            .execute(pool)
            .await?;
        }
    }

    Ok(delivery)
}

/// Resolve the gate from a DTMF digit.
///
/// Idempotent by construction: the status is only moved out of `pending`, so a redelivered
/// digit webhook cannot turn a denial into a grant or restart a recording.
pub async fn on_dtmf(
    pool: &Pool,
    provider: &dyn TelephonyProvider,
    call_id: Uuid,
    leg: &LegId,
    digit: char,
) -> Result<Option<AfterConsent>, sqlx::Error> {
    let Some(row) = load(pool, call_id).await? else {
        return Ok(None);
    };
    if row.status != ConsentStatus::Pending {
        // Not waiting for anyone: a stray keypress mid-conversation, or a redelivery.
        return Ok(None);
    }

    let status = consent::on_digit(digit, ACCEPT_DIGITS);
    let updated = sqlx::query(
        "UPDATE voip_calls
         SET consent_status = $2, consent_received_at = now(), updated_at = now()
         WHERE id = $1 AND consent_status = 'pending'
         RETURNING id",
    )
    .bind(call_id)
    .bind(status.as_str())
    .fetch_optional(pool)
    .await?;
    if updated.is_none() {
        // Someone else resolved it between the read and the write. Theirs stands.
        return Ok(None);
    }

    let action = consent::after(status, row.refused);
    act(pool, provider, call_id, leg, action, &row).await?;
    Ok(Some(action))
}

/// Close gates nobody answered.
///
/// The provider's own gather timeout produces an event on some carriers and nothing at all
/// on others, and either way the process that opened the gate may be gone. So the timeout
/// is enforced from the row, by the same sweep that reaps stalled calls.
pub async fn time_out_pending_consent(
    pool: &Pool,
    provider: &dyn TelephonyProvider,
    grace_secs: i64,
    limit: i64,
) -> Result<usize, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT c.id, array_to_string(c.provider_leg_ids, ',') AS legs
         FROM voip_calls c
         WHERE c.consent_status = 'pending'
           AND c.disclosure_played_at IS NOT NULL
           AND c.disclosure_played_at < now() - make_interval(secs => $1)
           AND c.status IN ('answered', 'bridged')
         ORDER BY c.disclosure_played_at
         LIMIT $2",
    )
    .bind(grace_secs as f64)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let mut closed = 0usize;
    for r in rows {
        let call_id: Uuid = r.try_get("id")?;
        let legs: Option<String> = r.try_get("legs")?;
        let Some(row) = load(pool, call_id).await? else {
            continue;
        };

        let updated = sqlx::query(
            "UPDATE voip_calls
             SET consent_status = 'timeout', consent_received_at = now(), updated_at = now()
             WHERE id = $1 AND consent_status = 'pending'
             RETURNING id",
        )
        .bind(call_id)
        .fetch_optional(pool)
        .await?;
        if updated.is_none() {
            continue;
        }

        let action = consent::after(ConsentStatus::Timeout, row.refused);
        let leg = legs
            .as_deref()
            .and_then(|l| l.split(',').next_back())
            .filter(|l| !l.is_empty())
            .map(LegId::new);
        if let Some(leg) = leg {
            act(pool, provider, call_id, &leg, action, &row).await?;
        }
        closed += 1;
    }
    Ok(closed)
}

/// Carry out what the resolved consent implies.
async fn act(
    pool: &Pool,
    provider: &dyn TelephonyProvider,
    call_id: Uuid,
    leg: &LegId,
    action: AfterConsent,
    row: &ConsentRow,
) -> Result<(), sqlx::Error> {
    match action {
        AfterConsent::StartCapture => start_capture(pool, provider, call_id, leg, row).await,
        AfterConsent::ContinueWithoutCapture => {
            // The call is fine; only the recorder is off. Moving the columns off their
            // "wanted" values stops the sweep from ever reconsidering them.
            //
            // `none` rather than a dedicated `declined`: the reason is already on the row,
            // and it is the authoritative one. `consent_status = 'denied'` next to
            // `recording_status = 'none'` reads unambiguously as "we asked, they said no,
            // nothing was kept" — which is exactly the sentence an auditor needs. A second
            // column saying the same thing is a second column that can disagree.
            sqlx::query(
                "UPDATE voip_calls
                 SET recording_status = CASE WHEN recording_status = 'pending' THEN 'none'
                                             ELSE recording_status END,
                     transcription_status = CASE WHEN transcription_status = 'live' THEN 'none'
                                                 ELSE transcription_status END,
                     updated_at = now()
                 WHERE id = $1",
            )
            .bind(call_id)
            .execute(pool)
            .await?;
            Ok(())
        }
        AfterConsent::EndCall => {
            if let Err(e) = provider.hangup(leg).await {
                crate::metrics::record_voip_provider_error();
                tracing::warn!(%call_id, error = %e, "could not end a call after consent was refused");
            }
            Ok(())
        }
    }
}

/// Start recording and/or transcription, and stamp when.
///
/// Re-reads the disclosure stamp rather than trusting the caller: this is the last gate
/// before audio is kept, and the row is the only thing that knows for certain whether the
/// recipient was ever told.
async fn start_capture(
    pool: &Pool,
    provider: &dyn TelephonyProvider,
    call_id: Uuid,
    leg: &LegId,
    row: &ConsentRow,
) -> Result<(), sqlx::Error> {
    if row.wants_recording
        && row.disclosure_played_at.is_none()
        && row.status != ConsentStatus::NotRequired
    {
        tracing::error!(
            %call_id,
            "refusing to record: no disclosure is stamped on the call"
        );
        return Ok(());
    }

    if row.wants_recording {
        if let Err(e) = provider
            .start_recording(
                leg,
                RecordingConfig {
                    // Separate channels so a transcript can attribute a line without
                    // diarisation — which on a two-party call is free accuracy.
                    dual_channel: true,
                    // An ADDITIONAL signal, never a substitute for the spoken
                    // announcement (D7).
                    beep: true,
                },
            )
            .await
        {
            crate::metrics::record_voip_provider_error();
            tracing::error!(%call_id, error = %e, "recording could not be started");
            sqlx::query(
                "UPDATE voip_calls SET recording_status = 'failed', updated_at = now()
                 WHERE id = $1",
            )
            .bind(call_id)
            .execute(pool)
            .await?;
        } else {
            sqlx::query(
                "UPDATE voip_calls
                 SET recording_status = 'recording',
                     recording_started_at = COALESCE(recording_started_at, now()),
                     updated_at = now()
                 WHERE id = $1",
            )
            .bind(call_id)
            .execute(pool)
            .await?;
        }
    }

    if row.wants_transcription {
        // Transcription needs nothing from the provider: the engine session is already
        // producing finals, and the transcript writer is already claiming them. All that
        // was missing is the permission, and the stamp that proves when it arrived.
        sqlx::query(
            "UPDATE voip_calls
             SET transcription_started_at = COALESCE(transcription_started_at, now()),
                 updated_at = now()
             WHERE id = $1",
        )
        .bind(call_id)
        .execute(pool)
        .await?;
    }

    Ok(())
}

/// The consent-relevant half of a call row.
#[derive(Debug, Clone)]
pub struct ConsentRow {
    pub policy: ConsentPolicy,
    pub status: ConsentStatus,
    pub refused: RefusedAction,
    pub language: String,
    pub wants_recording: bool,
    pub wants_transcription: bool,
    pub disclosure_played_at: Option<DateTime<Utc>>,
}

impl ConsentRow {
    pub fn intent(&self) -> CaptureIntent {
        CaptureIntent {
            recording: self.wants_recording,
            transcription: self.wants_transcription,
            ai_analysis: false,
        }
        .normalised()
    }

    /// Rebuild the plan that was made at dial time.
    ///
    /// Recomputed rather than stored: the plan is a pure function of the policy, the
    /// intent and the language, all of which are on the row, and a second copy of a
    /// derived value is a second thing that can disagree with the first.
    pub fn plan(&self) -> Disclosure {
        consent::plan(self.policy, self.intent(), true, &self.language)
    }
}

/// Read the consent-relevant columns, joining the org's refusal policy.
pub async fn load(pool: &Pool, call_id: Uuid) -> Result<Option<ConsentRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT c.consent_policy, c.consent_status, c.target_language,
                c.recording_status, c.transcription_status, c.disclosure_played_at,
                COALESCE(s.consent_refused_action, 'continue_unrecorded') AS refused
         FROM voip_calls c
         LEFT JOIN voip_org_settings s ON s.org_id = c.org_id
         WHERE c.id = $1",
    )
    .bind(call_id)
    .fetch_optional(pool)
    .await?;

    let Some(r) = row else { return Ok(None) };
    let recording: String = r.try_get("recording_status")?;
    let transcription: String = r.try_get("transcription_status")?;
    Ok(Some(ConsentRow {
        policy: ConsentPolicy::parse(&r.try_get::<String, _>("consent_policy")?),
        status: parse_status(&r.try_get::<String, _>("consent_status")?),
        refused: RefusedAction::parse(&r.try_get::<String, _>("refused")?),
        language: r.try_get("target_language")?,
        // `pending` is the only value that means "wanted and not yet started". `recording`
        // means it already is, and anything else means it never will be.
        wants_recording: recording == "pending",
        wants_transcription: transcription == "live",
        disclosure_played_at: r.try_get("disclosure_played_at")?,
    }))
}

fn parse_status(raw: &str) -> ConsentStatus {
    match raw {
        "pending" => ConsentStatus::Pending,
        "granted" => ConsentStatus::Granted,
        "denied" => ConsentStatus::Denied,
        "timeout" => ConsentStatus::Timeout,
        _ => ConsentStatus::NotRequired,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(policy: ConsentPolicy, recording: bool, transcription: bool) -> ConsentRow {
        ConsentRow {
            policy,
            status: ConsentStatus::Pending,
            refused: RefusedAction::ContinueUnrecorded,
            language: "it".into(),
            wants_recording: recording,
            wants_transcription: transcription,
            disclosure_played_at: None,
        }
    }

    #[test]
    fn only_announced_or_nothing_to_say_permits_capture() {
        // The asymmetry this module is built around, asserted once so a later refactor
        // cannot quietly add a permitting variant.
        assert!(Delivery::NothingToSay.permits_capture());
        assert!(Delivery::Announced {
            language: "it".into(),
            fell_back: false
        }
        .permits_capture());
        assert!(
            !Delivery::AwaitingConsent {
                language: "it".into(),
                fell_back: false
            }
            .permits_capture(),
            "waiting for a digit is not the same as having one"
        );
        assert!(
            !Delivery::NotAnnounced {
                reason: "provider_cannot_speak"
            }
            .permits_capture(),
            "a call that could not be disclosed must never be captured"
        );
    }

    #[test]
    fn the_plan_is_rebuilt_from_the_row_not_stored_beside_it() {
        let r = row(ConsentPolicy::PressKey, true, true);
        let plan = r.plan();
        assert_eq!(plan.language, "it");
        assert!(plan.gate.is_some(), "press-key with capture must gate");
        assert_eq!(plan.initial_status, ConsentStatus::Pending);
    }

    #[test]
    fn a_call_that_captures_nothing_never_gates() {
        // Asking someone to press 1 to allow a recording that is not happening is theatre,
        // and it costs the first ten seconds of every call.
        let r = row(ConsentPolicy::PressKey, false, false);
        assert!(r.plan().gate.is_none());
    }

    #[test]
    fn recording_status_pending_is_the_only_value_that_means_wanted() {
        // `recording` means it already started; anything else means it never will. Getting
        // this wrong would restart a recording on every redelivered webhook.
        let r = row(ConsentPolicy::NoticeOnly, true, false);
        assert!(r.intent().recording);
        let done = ConsentRow {
            wants_recording: false,
            ..r
        };
        assert!(!done.intent().recording);
    }
}
