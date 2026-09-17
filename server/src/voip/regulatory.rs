//! Self-service completion of provider regulatory requirements for a purchased number
//! (spec 0119).
//!
//! [`transition`] is the pure heart of this module, the same way `voip::state` is the pure
//! heart of the call lifecycle: the sweep and the `/refresh` route are both thin callers
//! that read a [`SubOrderState`] from the provider and hand it here, so "what does this
//! provider state mean for the number" is answered in exactly one place and is testable
//! without a provider, a database or a clock.

use uuid::Uuid;

use crate::db::Pool;
use crate::telephony::{
    FieldValue, GroupStatus, NumberStatus, OrderStatus, ProviderError, RequirementGroup,
    RequirementGroupId, RequirementQuery, RequirementSpec, RequirementsStatus, SubOrderState,
    TelephonyProvider,
};
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

// ---------------------------------------------------------------------------------------
// Requirement-group claim and reuse (spec 0119, design D7/D8)
// ---------------------------------------------------------------------------------------

/// A requirement group ensured for one org's country/number-kind/action combination:
/// either freshly claimed and created at the provider, or reused from a prior number in
/// the same combination.
#[derive(Debug)]
pub struct EnsuredGroup {
    /// Our own `voip_requirement_groups.id` — the FK target for
    /// `voip_numbers.requirement_group_id`.
    pub row_id: Uuid,
    pub provider_group_id: RequirementGroupId,
    pub status: GroupStatus,
    pub requirements: Vec<(RequirementSpec, Option<FieldValue>)>,
    /// `false` only for the request that won the claim and called the provider; every
    /// other caller for the same combination sees `true`.
    pub reused: bool,
}

/// Why [`ensure_group`] could not produce a group.
#[derive(Debug)]
pub enum GroupError {
    /// Another request claimed this combination and has not finished creating it at the
    /// provider yet (design D7's race). The loser must not also call the provider — that
    /// would create two groups for one combination — so it is told to try again shortly.
    Busy,
    Provider(ProviderError),
    Storage(sqlx::Error),
}

/// Claim, create-or-reuse, and return the requirement group for one org's
/// country/number-kind/action combination (design D7/D8).
///
/// Lazily called by the `GET …/requirements` handler (and by `PUT`/`POST submit`, which
/// need the same group) — never at buy time, so `numbers::buy`'s charge path stays
/// untouched. The claim is a single `INSERT … ON CONFLICT DO NOTHING` against the
/// partial unique index migration 064 defines on
/// `(org_id, provider, country, number_kind, action)`: it excludes only `expired` and
/// `no_longer_eligible` rows, so an expired group's slot is free for a fresh claim while
/// every other status (including another in-flight `creating` claim) blocks it — which is
/// exactly the reuse rule spec 0119 wants for a group already tied to a purchase.
pub async fn ensure_group(
    pool: &Pool,
    provider: &dyn TelephonyProvider,
    org_id: Uuid,
    query: &RequirementQuery,
    customer_ref: &str,
) -> Result<EnsuredGroup, GroupError> {
    let provider_id = provider.metadata().id;
    let number_kind = query.kind.as_str();
    let action = query.action.as_str();

    let claimed_row: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO voip_requirement_groups (org_id, provider, country, number_kind, action)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (org_id, provider, country, number_kind, action)
             WHERE status NOT IN ('expired', 'no_longer_eligible')
         DO NOTHING
         RETURNING id",
    )
    .bind(org_id)
    .bind(provider_id)
    .bind(&query.country)
    .bind(number_kind)
    .bind(action)
    .fetch_optional(pool)
    .await
    .map_err(GroupError::Storage)?;

    if let Some(row_id) = claimed_row {
        // Won the claim: call the carrier, then record what it said.
        return match provider.create_requirement_group(query, customer_ref).await {
            Ok(group) => {
                sqlx::query(
                    "UPDATE voip_requirement_groups
                        SET provider_group_id = $2, status = $3, updated_at = now()
                      WHERE id = $1",
                )
                .bind(row_id)
                .bind(&group.id.0)
                .bind(group.status.as_str())
                .execute(pool)
                .await
                .map_err(GroupError::Storage)?;
                Ok(EnsuredGroup {
                    row_id,
                    provider_group_id: group.id,
                    status: group.status,
                    requirements: group.requirements,
                    reused: false,
                })
            }
            Err(e) => {
                // Release the claim so a carrier failure does not permanently jam this
                // combination for every later attempt at it.
                sqlx::query(
                    "DELETE FROM voip_requirement_groups
                      WHERE id = $1 AND provider_group_id IS NULL",
                )
                .bind(row_id)
                .execute(pool)
                .await
                .map_err(GroupError::Storage)?;
                Err(GroupError::Provider(e))
            }
        };
    }

    // Lost the claim: somebody already owns this combination.
    let existing: Option<(Uuid, Option<String>)> = sqlx::query_as(
        "SELECT id, provider_group_id FROM voip_requirement_groups
          WHERE org_id = $1 AND provider = $2 AND country = $3 AND number_kind = $4
            AND action = $5 AND status NOT IN ('expired', 'no_longer_eligible')",
    )
    .bind(org_id)
    .bind(provider_id)
    .bind(&query.country)
    .bind(number_kind)
    .bind(action)
    .fetch_optional(pool)
    .await
    .map_err(GroupError::Storage)?;

    match existing {
        // Still mid-claim: another request is creating it right now.
        Some((_, None)) => Err(GroupError::Busy),
        Some((row_id, Some(provider_group_id))) => {
            let group_id = RequirementGroupId(provider_group_id);
            let group: RequirementGroup = provider
                .get_requirement_group(&group_id)
                .await
                .map_err(GroupError::Provider)?
                .ok_or_else(|| {
                    GroupError::Provider(ProviderError::Malformed {
                        detail: "requirement group vanished at the provider".into(),
                    })
                })?;
            Ok(EnsuredGroup {
                row_id,
                provider_group_id: group.id,
                status: group.status,
                requirements: group.requirements,
                reused: true,
            })
        }
        // The unique index rejected our INSERT, so a row must exist — a row that vanished
        // between the two queries is a storage anomaly, not a domain one.
        None => Err(GroupError::Storage(sqlx::Error::RowNotFound)),
    }
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

    // -----------------------------------------------------------------------------------
    // `ensure_group` — claim, create and reuse (spec 0119, design D7/D8).
    //
    // DB-gated: skipped without `DATABASE_URL`, same as every other DB test in this crate.
    // -----------------------------------------------------------------------------------

    use uuid::Uuid;

    use crate::telephony::mock::MockTelephonyProvider;
    use crate::telephony::{NumberKind, ProviderError, RequirementAction, RequirementQuery};
    use crate::voip::regulatory::{ensure_group, GroupError};

    macro_rules! skip_without_db {
        () => {
            match crate::db::test_database_url() {
                Some(url) => {
                    let pool = crate::db::connect(&url).await.unwrap();
                    crate::db::migrate(&pool).await.unwrap();
                    pool
                }
                None => {
                    eprintln!("skipping — no DATABASE_URL");
                    return;
                }
            }
        };
    }

    /// A minimal org row, everything `ensure_group`'s FK needs and nothing more.
    async fn fresh_org(pool: &crate::db::Pool) -> Uuid {
        let owner: Uuid = sqlx::query_scalar(
            "INSERT INTO users (google_id, email, name, balance)
             VALUES ($1, $2, 'Owner', 0) RETURNING id",
        )
        .bind(format!("g-{}", Uuid::new_v4()))
        .bind(format!("{}@example.test", Uuid::new_v4()))
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query_scalar(
            "INSERT INTO organizations (name, slug, owner_id, credits_balance)
             VALUES ('Regulatory Co', $1, $2, 0) RETURNING id",
        )
        .bind(format!("reg-{}", Uuid::new_v4().simple()))
        .bind(owner)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    fn fr_mobile() -> RequirementQuery {
        RequirementQuery {
            country: "FR".into(),
            kind: NumberKind::Mobile,
            action: RequirementAction::Ordering,
        }
    }

    #[tokio::test]
    async fn ensure_group_creates_a_fresh_group_when_none_exists_for_the_combination() {
        let pool = skip_without_db!();
        let org_id = fresh_org(&pool).await;
        let provider = MockTelephonyProvider::default();

        let ensured = ensure_group(&pool, &provider, org_id, &fr_mobile(), &org_id.to_string())
            .await
            .expect("no group exists yet — this must create one");

        assert!(
            !ensured.reused,
            "the very first call must not claim a reuse"
        );
        assert!(
            !ensured.requirements.is_empty(),
            "the fixture requirement list must come back with the fresh group"
        );

        let (status, provider_group_id): (String, Option<String>) = sqlx::query_as(
            "SELECT status, provider_group_id FROM voip_requirement_groups WHERE id = $1",
        )
        .bind(ensured.row_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "unapproved");
        assert_eq!(provider_group_id, Some(ensured.provider_group_id.0));
    }

    #[tokio::test]
    async fn a_second_call_for_the_same_combination_reuses_the_first_groups_id() {
        // The sequential shape of the reuse spec 0119 requires: a second `GET
        // …/requirements` (this time for a different number in the same combination)
        // must land on the exact same provider group, not create a second one.
        let pool = skip_without_db!();
        let org_id = fresh_org(&pool).await;
        let provider = MockTelephonyProvider::default();

        let first = ensure_group(&pool, &provider, org_id, &fr_mobile(), &org_id.to_string())
            .await
            .unwrap();
        let second = ensure_group(&pool, &provider, org_id, &fr_mobile(), &org_id.to_string())
            .await
            .expect("an already-claimed, already-created combination must be reused");

        assert!(second.reused, "the second caller must see reused = true");
        assert_eq!(second.row_id, first.row_id);
        assert_eq!(second.provider_group_id, first.provider_group_id);

        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM voip_requirement_groups
              WHERE org_id = $1 AND country = 'FR' AND number_kind = 'mobile'",
        )
        .bind(org_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rows, 1, "one row per combination, never two");
    }

    #[tokio::test]
    async fn a_claim_still_being_created_by_another_caller_is_reported_busy() {
        // design D7's race: simulate the exact state a real second caller would land on
        // mid-race — a claim row already inserted by the "winner", with no provider id
        // yet because that winner has not called the carrier back with an answer.
        let pool = skip_without_db!();
        let org_id = fresh_org(&pool).await;
        let provider = MockTelephonyProvider::default();

        sqlx::query(
            "INSERT INTO voip_requirement_groups (org_id, provider, country, number_kind, action)
             VALUES ($1, 'mock', 'FR', 'mobile', 'ordering')",
        )
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();

        // If `ensure_group` incorrectly treated this as free to claim, it would call the
        // carrier and get THIS scripted failure instead of `Busy` — so a `Busy` result
        // proves the create path was never reached, not just that some result came back.
        provider.fail_next(
            "create_requirement_group",
            ProviderError::Unavailable {
                detail: "must never be called".into(),
            },
        );

        let err = ensure_group(&pool, &provider, org_id, &fr_mobile(), &org_id.to_string())
            .await
            .expect_err("a mid-claim combination must never be double-created");
        assert!(matches!(err, GroupError::Busy), "{err:?}");

        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM voip_requirement_groups
              WHERE org_id = $1 AND country = 'FR' AND number_kind = 'mobile'",
        )
        .bind(org_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rows, 1, "the loser must not have inserted a second row");
    }

    #[tokio::test]
    async fn a_carrier_failure_releases_the_claim_for_the_next_attempt() {
        let pool = skip_without_db!();
        let org_id = fresh_org(&pool).await;
        let provider = MockTelephonyProvider::default();
        provider.fail_next(
            "create_requirement_group",
            ProviderError::Unavailable {
                detail: "carrier outage".into(),
            },
        );

        let err = ensure_group(&pool, &provider, org_id, &fr_mobile(), &org_id.to_string())
            .await
            .expect_err("the scripted failure must surface");
        assert!(matches!(err, GroupError::Provider(_)), "{err:?}");

        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM voip_requirement_groups
              WHERE org_id = $1 AND country = 'FR' AND number_kind = 'mobile'",
        )
        .bind(org_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rows, 0, "a failed claim must not leave a jammed row behind");

        // The very next attempt succeeds — proof the slot was actually freed, not just
        // that the row count happens to read zero.
        let ensured = ensure_group(&pool, &provider, org_id, &fr_mobile(), &org_id.to_string())
            .await
            .expect("the freed combination must be claimable again");
        assert!(!ensured.reused);
    }

    #[tokio::test]
    async fn an_expired_groups_combination_can_be_claimed_afresh() {
        // The partial unique index (migration 064) excludes `expired`/`no_longer_eligible`
        // on purpose (spec 0119 "Declined or expired group not reused"): an expired row
        // must not block a brand-new claim for the same combination.
        let pool = skip_without_db!();
        let org_id = fresh_org(&pool).await;
        let provider = MockTelephonyProvider::default();

        let stale = ensure_group(&pool, &provider, org_id, &fr_mobile(), &org_id.to_string())
            .await
            .unwrap();
        sqlx::query("UPDATE voip_requirement_groups SET status = 'expired' WHERE id = $1")
            .bind(stale.row_id)
            .execute(&pool)
            .await
            .unwrap();

        let fresh = ensure_group(&pool, &provider, org_id, &fr_mobile(), &org_id.to_string())
            .await
            .expect("an expired combination must be claimable again");
        assert!(
            !fresh.reused,
            "a fresh claim, not a reuse of the expired row"
        );
        assert_ne!(fresh.row_id, stale.row_id);

        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM voip_requirement_groups
              WHERE org_id = $1 AND country = 'FR' AND number_kind = 'mobile'",
        )
        .bind(org_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rows, 2, "the stale row is kept, not overwritten");
    }
}
