//! The call lifecycle (spec 0111, R25–R27).
//!
//! Provider webhooks are duplicated, delayed and delivered out of business order. That is
//! not an edge case to handle later — it is the normal behaviour of every telephony
//! provider, and money hangs off the result. So the lifecycle is a **pure, total,
//! monotonic** function here, unit-tested on its own, and the webhook handler's only job
//! is to feed it verified events.
//!
//! Three properties, and every one of them exists because violating it costs real money:
//!
//! - **Total** — every (state, event) pair has a defined answer. `None` means "this event
//!   does not move the lifecycle", never "undefined".
//! - **Monotonic** — a `ringing` that overtakes an `answered` cannot un-answer the call.
//!   States carry a rank and a transition may never lower it.
//! - **Absorbing** — once a call is `Completed` or `Failed` it stays there. A recording
//!   webhook that lands ten seconds after hangup updates the recording columns; it does
//!   not resurrect the call and it does not restart the meter.

use std::fmt;

/// Where a call is in its life. Persisted as the lowercase name in `voip_calls.status`,
/// so the string values are API surface and must not change once shipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallState {
    /// The row exists and credits are reserved, but nothing has been dialed. A call that
    /// dies here released its reservation and cost nothing.
    Created,
    /// Handed to the provider; no carrier response yet.
    Dialing,
    /// The far end is ringing.
    Ringing,
    /// Answered. This is the first state in which the customer can be charged.
    Answered,
    /// Media is streaming and translation is live in both directions.
    Bridged,
    /// Teardown started (either side hung up, or a limit was reached).
    Ending,
    /// Terminal, and the call happened.
    Completed,
    /// Terminal, and the call did not happen or could not continue.
    Failed,
}

impl CallState {
    /// Persisted / wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Dialing => "dialing",
            Self::Ringing => "ringing",
            Self::Answered => "answered",
            Self::Bridged => "bridged",
            Self::Ending => "ending",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    /// Parse back from the persisted name. Unknown values are refused rather than mapped
    /// to a default: a row we cannot interpret must not be silently treated as a live
    /// call, because a live call gets billed.
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "created" => Self::Created,
            "dialing" => Self::Dialing,
            "ringing" => Self::Ringing,
            "answered" => Self::Answered,
            "bridged" => Self::Bridged,
            "ending" => Self::Ending,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            _ => return None,
        })
    }

    /// Ordering used to reject out-of-order events. Both terminal states share the top
    /// rank so neither can follow the other.
    fn rank(self) -> u8 {
        match self {
            Self::Created => 0,
            Self::Dialing => 1,
            Self::Ringing => 2,
            Self::Answered => 3,
            Self::Bridged => 4,
            Self::Ending => 5,
            Self::Completed | Self::Failed => 6,
        }
    }

    /// Nothing follows a terminal state.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }

    /// Whether the customer may be charged for time in this state. The meter is allowed to
    /// run from `Answered`: that is the moment the carrier starts charging us.
    pub fn is_billable(self) -> bool {
        matches!(self, Self::Answered | Self::Bridged | Self::Ending)
    }

    /// Whether the call has been answered at some point — which decides whether a hangup
    /// completes the call or fails it.
    pub fn was_answered(self) -> bool {
        self.rank() >= Self::Answered.rank() && !self.is_terminal()
    }
}

impl fmt::Display for CallState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a call ended or could not start. Provider-specific causes are mapped onto these by
/// the adapter, so the rest of the system — dashboard, analytics, support, i18n — never
/// sees a Telnyx string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailureReason {
    /// The far end was busy.
    Busy,
    /// The far end rejected the call.
    Rejected,
    /// Rang out with nobody answering.
    NoAnswer,
    /// The carrier says the number does not exist or is not reachable.
    Unallocated,
    /// The destination is not permitted by policy (country list, international off).
    DestinationNotAllowed,
    /// The destination's provider cost exceeds the configured ceiling.
    DestinationTooExpensive,
    /// No fresh rate is known for the destination, so we refuse rather than guess.
    RateUnavailable,
    /// The org pool could not cover the reservation.
    InsufficientCredits,
    /// The pool emptied while the call was up.
    CreditsExhausted,
    /// EU-only processing was required and the selected tier cannot provide it.
    EuProcessingUnavailable,
    /// A concurrency cap (user, org or global) was already at its limit.
    ConcurrencyLimit,
    /// The call hit `VOIP_MAX_CALL_DURATION_MINUTES`.
    MaxDurationReached,
    /// The media stream was lost and did not come back inside the grace window.
    MediaLost,
    /// The provider itself failed — outage, auth, balance, rate limit.
    ProviderUnavailable,
    /// The recipient declined the recording/transcription consent gate and org policy
    /// says such a call ends.
    ConsentDeclined,
    /// A carrier or provider cause we have not mapped. Kept distinct from
    /// `ProviderUnavailable` so an unmapped cause shows up in analytics as work to do
    /// rather than hiding inside a bucket that already has a meaning.
    Unmapped,
}

impl FailureReason {
    /// Stable machine-readable value, persisted in `voip_calls.failure_reason` and
    /// localised by the dashboard.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::Rejected => "rejected",
            Self::NoAnswer => "no_answer",
            Self::Unallocated => "unallocated_number",
            Self::DestinationNotAllowed => "destination_not_allowed",
            Self::DestinationTooExpensive => "destination_too_expensive",
            Self::RateUnavailable => "rate_unavailable",
            Self::InsufficientCredits => "insufficient_credits",
            Self::CreditsExhausted => "credits_exhausted",
            Self::EuProcessingUnavailable => "eu_processing_unavailable",
            Self::ConcurrencyLimit => "concurrency_limit",
            Self::MaxDurationReached => "max_duration_reached",
            Self::MediaLost => "media_lost",
            Self::ProviderUnavailable => "provider_unavailable",
            Self::ConsentDeclined => "consent_declined",
            Self::Unmapped => "unmapped",
        }
    }

    /// Whether a *refused or failed* call should have already been paid for. Used by
    /// settlement to assert that nothing was charged for a call that never connected.
    pub fn is_pre_answer(self) -> bool {
        !matches!(
            self,
            Self::CreditsExhausted | Self::MaxDurationReached | Self::MediaLost
        )
    }
}

/// A lifecycle-relevant thing that happened, already normalised out of provider vocabulary.
///
/// Deliberately narrower than the set of provider webhooks: events that carry information
/// but do not move the lifecycle (recording saved, transcription started, DTMF) are
/// handled by their own column updates and are not modelled here, so this enum stays a
/// description of the *lifecycle* rather than of the provider's API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallEvent {
    /// We handed the call to the provider.
    Dialed,
    /// The far end is ringing.
    Ringing,
    /// The far end answered.
    Answered,
    /// Media is streaming; translation is live.
    MediaEstablished,
    /// Teardown has begun (we asked for hangup, or a limit fired).
    Ending,
    /// The call is over. `reason` is `None` for an ordinary hangup by either party.
    Hangup { reason: Option<FailureReason> },
    /// The call cannot proceed.
    Failed { reason: FailureReason },
}

/// The lifecycle function.
///
/// Returns the new state, or `None` when the event does not move the lifecycle — a
/// duplicate delivery, an event that arrived late, or anything at all once the call is
/// terminal. `None` is a success: it is what makes redelivery safe (R25) and out-of-order
/// delivery harmless (R26).
pub fn next(state: CallState, event: CallEvent) -> Option<CallState> {
    // Terminal absorbs everything. This single line is what stops a delayed
    // `recording.saved` or a redelivered `answered` from restarting a finished call.
    if state.is_terminal() {
        return None;
    }

    let candidate = match event {
        CallEvent::Dialed => CallState::Dialing,
        CallEvent::Ringing => CallState::Ringing,
        CallEvent::Answered => CallState::Answered,
        CallEvent::MediaEstablished => CallState::Bridged,
        CallEvent::Ending => CallState::Ending,

        // A hangup is terminal from wherever it lands, so it bypasses the rank check
        // below: it is the one event allowed to end a call at any point.
        //
        // One rule decides completed-vs-failed: **did anyone answer?** A conversation that
        // happened is a completed call carrying whatever reason ended it — credits ran
        // out, the clock ran out, the media died. A call nobody picked up is a failed
        // call, not a zero-length completed one, and billing depends on the difference:
        // only a completed call has anything to settle.
        //
        // A mid-call termination is therefore a `Hangup` with a reason, never a `Failed`.
        // `Failed` is reserved for a call that cannot proceed at all.
        CallEvent::Hangup { .. } => {
            return Some(if state.was_answered() {
                CallState::Completed
            } else {
                CallState::Failed
            });
        }
        CallEvent::Failed { .. } => return Some(CallState::Failed),
    };

    // Monotonic: an out-of-order or duplicate event is ignored rather than applied.
    if candidate.rank() > state.rank() {
        Some(candidate)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::CallState::*;
    use super::*;

    /// Drive a sequence and return where it lands, ignoring no-op events the way the
    /// webhook handler does.
    fn run(start: CallState, events: &[CallEvent]) -> CallState {
        events
            .iter()
            .fold(start, |s, e| next(s, *e).unwrap_or(s))
    }

    const ORDER: [CallState; 8] = [
        Created, Dialing, Ringing, Answered, Bridged, Ending, Completed, Failed,
    ];

    // ---- the happy path -------------------------------------------------------

    #[test]
    fn a_normal_call_walks_created_to_completed() {
        let end = run(
            Created,
            &[
                CallEvent::Dialed,
                CallEvent::Ringing,
                CallEvent::Answered,
                CallEvent::MediaEstablished,
                CallEvent::Ending,
                CallEvent::Hangup { reason: None },
            ],
        );
        assert_eq!(end, Completed);
    }

    #[test]
    fn a_call_can_be_answered_without_a_ringing_event() {
        // Some carriers never send `ringing`. Requiring it would strand the call.
        assert_eq!(
            run(Created, &[CallEvent::Dialed, CallEvent::Answered]),
            Answered
        );
    }

    // ---- idempotency (R25) ----------------------------------------------------

    #[test]
    fn a_redelivered_event_changes_nothing() {
        let s = run(Created, &[CallEvent::Dialed, CallEvent::Answered]);
        assert_eq!(next(s, CallEvent::Answered), None, "duplicate is a no-op");
        assert_eq!(next(s, CallEvent::Dialed), None);
        assert_eq!(next(s, CallEvent::Ringing), None);
        // …and replaying the whole prefix lands in the same place.
        assert_eq!(
            run(
                Created,
                &[
                    CallEvent::Dialed,
                    CallEvent::Dialed,
                    CallEvent::Answered,
                    CallEvent::Answered,
                ]
            ),
            Answered
        );
    }

    // ---- out-of-order (R26) ---------------------------------------------------

    #[test]
    fn a_late_ringing_cannot_un_answer_a_call() {
        // The regression this exists for: `ringing` overtaking `answered` would stop the
        // meter on a call that is connected and being charged to us by the carrier.
        let s = run(Created, &[CallEvent::Dialed, CallEvent::Answered]);
        assert_eq!(next(s, CallEvent::Ringing), None);
        assert_eq!(run(s, &[CallEvent::Ringing]), Answered);
    }

    #[test]
    fn no_event_can_ever_lower_the_state() {
        // Exhaustive: for every state and every non-terminal event, the result is either
        // ignored or strictly forward. Hangup/Failed are terminal and tested separately.
        let moves = [
            CallEvent::Dialed,
            CallEvent::Ringing,
            CallEvent::Answered,
            CallEvent::MediaEstablished,
            CallEvent::Ending,
        ];
        for from in ORDER {
            for e in moves {
                if let Some(to) = next(from, e) {
                    assert!(
                        to.rank() > from.rank(),
                        "{from} + {e:?} went backwards to {to}"
                    );
                }
            }
        }
    }

    // ---- terminal is absorbing ------------------------------------------------

    #[test]
    fn nothing_follows_a_terminal_state() {
        let every_event = [
            CallEvent::Dialed,
            CallEvent::Ringing,
            CallEvent::Answered,
            CallEvent::MediaEstablished,
            CallEvent::Ending,
            CallEvent::Hangup { reason: None },
            CallEvent::Failed {
                reason: FailureReason::ProviderUnavailable,
            },
        ];
        for terminal in [Completed, Failed] {
            for e in every_event {
                assert_eq!(
                    next(terminal, e),
                    None,
                    "{terminal} must absorb {e:?} — a late webhook cannot restart a \
                     finished call, or the meter starts again on a call nobody is on"
                );
            }
        }
    }

    // ---- hangup semantics -----------------------------------------------------

    #[test]
    fn hanging_up_before_answer_is_a_failure_not_a_zero_length_call() {
        // Billing depends on this: a call that never connected must not appear in history
        // as a completed call the customer was charged for.
        for reason in [
            None,
            Some(FailureReason::Busy),
            Some(FailureReason::NoAnswer),
            Some(FailureReason::Rejected),
        ] {
            for from in [Created, Dialing, Ringing] {
                assert_eq!(
                    next(from, CallEvent::Hangup { reason }),
                    Some(Failed),
                    "{from} + hangup({reason:?})"
                );
            }
        }
    }

    #[test]
    fn hanging_up_after_answer_completes_the_call() {
        for from in [Answered, Bridged, Ending] {
            assert_eq!(next(from, CallEvent::Hangup { reason: None }), Some(Completed));
        }
    }

    #[test]
    fn a_mid_call_problem_completes_rather_than_fails() {
        // Credits ran out, the clock ran out, the media died — the conversation really
        // happened and is billable up to that instant, so it is a completed call carrying
        // a reason, not a failed one. Marking it failed would strand the settlement.
        for reason in [
            FailureReason::CreditsExhausted,
            FailureReason::MaxDurationReached,
            FailureReason::MediaLost,
        ] {
            assert_eq!(
                next(
                    Bridged,
                    CallEvent::Hangup {
                        reason: Some(reason)
                    }
                ),
                Some(Completed),
                "{reason:?}"
            );
            // The same reason before answer is still a failure — nothing was billable.
            assert_eq!(
                next(
                    Dialing,
                    CallEvent::Hangup {
                        reason: Some(reason)
                    }
                ),
                Some(Failed)
            );
        }
    }

    #[test]
    fn an_explicit_failure_ends_the_call_from_anywhere_non_terminal() {
        for from in [Created, Dialing, Ringing, Answered, Bridged, Ending] {
            assert_eq!(
                next(
                    from,
                    CallEvent::Failed {
                        reason: FailureReason::ProviderUnavailable
                    }
                ),
                Some(Failed),
                "{from}"
            );
        }
    }

    // ---- billing-facing predicates -------------------------------------------

    #[test]
    fn only_a_connected_call_is_billable() {
        for s in [Created, Dialing, Ringing, Completed, Failed] {
            assert!(!s.is_billable(), "{s} must not bill");
        }
        for s in [Answered, Bridged, Ending] {
            assert!(s.is_billable(), "{s} must bill");
        }
    }

    #[test]
    fn a_terminal_state_was_not_answered_for_hangup_purposes() {
        // `was_answered` decides completed-vs-failed, and a terminal state never reaches
        // that decision — asserting it here keeps the predicate honest if it is reused.
        assert!(!Completed.was_answered());
        assert!(!Failed.was_answered());
        assert!(Answered.was_answered());
        assert!(Bridged.was_answered());
        assert!(!Ringing.was_answered());
    }

    // ---- persistence round-trip ----------------------------------------------

    #[test]
    fn every_state_round_trips_through_its_persisted_name() {
        for s in ORDER {
            assert_eq!(CallState::parse(s.as_str()), Some(s), "{s}");
        }
        // A value we do not recognise is refused, never defaulted: defaulting an
        // unreadable row to a live state would put it back on the meter.
        assert_eq!(CallState::parse("in_progress"), None);
        assert_eq!(CallState::parse(""), None);
    }

    #[test]
    fn persisted_names_are_unique() {
        let names: std::collections::HashSet<&str> = ORDER.iter().map(|s| s.as_str()).collect();
        assert_eq!(names.len(), ORDER.len());
    }

    #[test]
    fn every_failure_reason_has_a_unique_stable_code() {
        let all = [
            FailureReason::Busy,
            FailureReason::Rejected,
            FailureReason::NoAnswer,
            FailureReason::Unallocated,
            FailureReason::DestinationNotAllowed,
            FailureReason::DestinationTooExpensive,
            FailureReason::RateUnavailable,
            FailureReason::InsufficientCredits,
            FailureReason::CreditsExhausted,
            FailureReason::EuProcessingUnavailable,
            FailureReason::ConcurrencyLimit,
            FailureReason::MaxDurationReached,
            FailureReason::MediaLost,
            FailureReason::ProviderUnavailable,
            FailureReason::ConsentDeclined,
            FailureReason::Unmapped,
        ];
        let codes: std::collections::HashSet<&str> = all.iter().map(|r| r.as_str()).collect();
        assert_eq!(codes.len(), all.len(), "duplicate failure_reason value");
        for r in all {
            assert!(!r.as_str().is_empty());
        }
    }
}
