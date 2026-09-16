//! Self-service completion of provider regulatory requirements for a purchased number
//! (spec 0119).
//!
//! [`transition`] is the pure heart of this module, the same way `voip::state` is the pure
//! heart of the call lifecycle: the sweep and the `/refresh` route are both thin callers
//! that read a [`SubOrderState`] from the provider and hand it here, so "what does this
//! provider state mean for the number" is answered in exactly one place and is testable
//! without a provider, a database or a clock.

use crate::telephony::{NumberStatus, OrderStatus, RequirementsStatus, SubOrderState};

/// The longest a rejection reason may be once it reaches `voip_numbers.status_reason`.
/// The column is a status label, not a log: an unbounded provider string must not be
/// forwarded as-is.
const MAX_REASON_CHARS: usize = 500;

/// What applying a [`SubOrderState`] to a number should change.
///
/// `reason` is `None` for every non-rejection outcome, meaning "no reason to record" —
/// distinct from an empty string, which would still be a (pointless) reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub status: NumberStatus,
    pub reason: Option<String>,
    pub outbound: bool,
}

/// Pure projection from provider state onto a [`NumberStatus`] (spec 0119 R5, design D4).
///
/// `None` means "nothing to change" — the provider state is not yet decisive (an order
/// still in flight with nothing pending, or a genuinely unrecognised word). Never `Active`
/// and never `Failed` from an indecisive state: caller id and the terminal failure status
/// are both consequential enough that "we don't know yet" must stay non-committal.
///
/// `current` is accepted for a future where the projection needs to know what it is
/// leaving (a resubmission clearing a prior rejection, for instance); today it does not,
/// because every branch below is decided by `s` alone.
pub fn transition(_current: NumberStatus, s: &SubOrderState) -> Option<Transition> {
    // A deadline-miss cancellation is terminal and OVERRIDES anything requirements say —
    // Telnyx does not reopen requirements on a cancelled order, so a lingering "approved"
    // read from a race must not resurrect it as active (spec 0119 "Deadline Cancellation").
    if matches!(s.order, OrderStatus::Cancelled | OrderStatus::Deleted) {
        return Some(Transition {
            status: NumberStatus::Failed,
            reason: None,
            outbound: false,
        });
    }

    // Either signal being fully positive is enough: an order can complete before the
    // requirements pipeline reports back, and requirements can be approved before the
    // parent order's own bookkeeping catches up.
    if matches!(s.requirements, RequirementsStatus::Approved) || s.order == OrderStatus::Success {
        return Some(Transition {
            status: NumberStatus::Active,
            reason: None,
            outbound: true,
        });
    }

    match &s.requirements {
        RequirementsStatus::Exception { reason } => Some(Transition {
            status: NumberStatus::RegulatoryRejected,
            reason: reason.as_deref().map(truncate),
            outbound: false,
        }),
        RequirementsStatus::UnderReview => Some(Transition {
            status: NumberStatus::RegulatoryReview,
            reason: None,
            outbound: false,
        }),
        RequirementsStatus::InfoPending => Some(Transition {
            status: NumberStatus::PendingRegulatory,
            reason: None,
            outbound: false,
        }),
        // Approved was already handled above; reaching it here would mean `s.order` was
        // also not `Success`, which is still a `None` — nothing left to decide.
        RequirementsStatus::Approved | RequirementsStatus::Unknown => None,
    }
}

/// Cut a provider string down to [`MAX_REASON_CHARS`] **characters**, not bytes — a naive
/// byte-index slice can land inside a multi-byte UTF-8 sequence and panic.
fn truncate(s: &str) -> String {
    s.chars().take(MAX_REASON_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use crate::telephony::{NumberStatus, OrderStatus, RequirementsStatus, SubOrderState};
    use crate::voip::regulatory::transition;

    fn state(order: OrderStatus, requirements: RequirementsStatus) -> SubOrderState {
        SubOrderState {
            order,
            requirements,
            group: None,
        }
    }

    /// `(label, input, expected (status, outbound), or None for "no transition")`.
    type Case = (&'static str, SubOrderState, Option<(NumberStatus, bool)>);

    #[test]
    fn transition_maps_every_provider_state_onto_the_right_number_status() {
        // spec 0119 R5: each provider-reported combination has exactly one honest number
        // status, and only a deadline-miss cancellation of the ORDER ever fails a number.
        let cases: Vec<Case> = vec![
            (
                "info still pending stays pending_regulatory",
                state(OrderStatus::Pending, RequirementsStatus::InfoPending),
                Some((NumberStatus::PendingRegulatory, false)),
            ),
            (
                "submitted requirements move to regulatory_review",
                state(OrderStatus::Pending, RequirementsStatus::UnderReview),
                Some((NumberStatus::RegulatoryReview, false)),
            ),
            (
                "a rejection is regulatory_rejected, resubmittable, outbound withheld",
                state(
                    OrderStatus::Pending,
                    RequirementsStatus::Exception {
                        reason: Some("Missing proof of address".into()),
                    },
                ),
                Some((NumberStatus::RegulatoryRejected, false)),
            ),
            (
                "approved requirements activate the number",
                state(OrderStatus::Pending, RequirementsStatus::Approved),
                Some((NumberStatus::Active, true)),
            ),
            (
                "an order success activates the number even before requirements catch up",
                state(OrderStatus::Success, RequirementsStatus::Unknown),
                Some((NumberStatus::Active, true)),
            ),
            (
                "a cancelled order fails the number regardless of requirements",
                state(OrderStatus::Cancelled, RequirementsStatus::UnderReview),
                Some((NumberStatus::Failed, false)),
            ),
            (
                "a deleted order fails the number the same way a cancellation does",
                state(OrderStatus::Deleted, RequirementsStatus::InfoPending),
                Some((NumberStatus::Failed, false)),
            ),
            (
                "an order failure with nothing else decided moves nothing",
                state(OrderStatus::Failure, RequirementsStatus::Unknown),
                None,
            ),
            (
                "both sides unknown moves nothing — never active, never failed",
                state(OrderStatus::Unknown, RequirementsStatus::Unknown),
                None,
            ),
        ];

        for (label, sub_order, expected) in cases {
            let got = transition(NumberStatus::PendingRegulatory, &sub_order);
            match expected {
                Some((status, outbound)) => {
                    let t = got.unwrap_or_else(|| panic!("{label}: expected a transition"));
                    assert_eq!(t.status, status, "{label}");
                    assert_eq!(t.outbound, outbound, "{label}");
                }
                None => assert!(
                    got.is_none(),
                    "{label}: expected no transition, got {got:?}"
                ),
            }
        }
    }

    #[test]
    fn a_rejection_reason_is_truncated_to_five_hundred_characters() {
        // The provider's own rejection text is free-form and unbounded; `status_reason`
        // is a column, not a log, so it must never grow with whatever a regulator typed.
        let long_reason = "x".repeat(600);
        let sub_order = state(
            OrderStatus::Pending,
            RequirementsStatus::Exception {
                reason: Some(long_reason),
            },
        );
        let t = transition(NumberStatus::RegulatoryReview, &sub_order).unwrap();
        assert_eq!(t.status, NumberStatus::RegulatoryRejected);
        assert_eq!(t.reason.as_ref().unwrap().len(), 500);

        // A short reason is carried verbatim, so the truncation above is proven to be
        // truncation and not accidental clearing.
        let sub_order = state(
            OrderStatus::Pending,
            RequirementsStatus::Exception {
                reason: Some("Too short".into()),
            },
        );
        let t = transition(NumberStatus::RegulatoryReview, &sub_order).unwrap();
        assert_eq!(t.reason.as_deref(), Some("Too short"));
    }
}
