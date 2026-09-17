//! Self-service completion of provider regulatory requirements for a purchased number
//! (spec 0119).
//!
//! [`transition`] is the pure heart of this module, the same way `voip::state` is the pure
//! heart of the call lifecycle: the sweep and the `/refresh` route are both thin callers
//! that read a [`SubOrderState`] from the provider and hand it here, so "what does this
//! provider state mean for the number" is answered in exactly one place and is testable
//! without a provider, a database or a clock.

// Every handler returns `Result<Response, Response>` — the Business API convention.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::time::Duration;

use axum::extract::{Multipart, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::business::{db_err, not_found, require_pool, require_role, ADMIN, MEMBER};
use crate::db::Pool;
use crate::middleware::AuthUser;
use crate::telephony::{
    AddressValue, DocumentUpload, FieldValue, GroupStatus, NumberKind, NumberStatus, OrderStatus,
    ProviderError, RequirementAction, RequirementGroup, RequirementGroupId, RequirementKind,
    RequirementQuery, RequirementSpec, RequirementsStatus, SubOrderId, SubOrderState,
    TelephonyProvider,
};
use crate::voip::routes::refuse;
use crate::AppState;

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

// ---------------------------------------------------------------------------------------
// Single-row reconcile (design D5) — shared today by `POST …/refresh`; the Phase 7 sweep
// calls it once per due row instead of reimplementing the same projection.
// ---------------------------------------------------------------------------------------

/// Why [`reconcile_one`] could not read or apply the provider's state.
#[derive(Debug)]
pub enum ReconcileError {
    Provider(ProviderError),
    Storage(sqlx::Error),
}

/// Read the provider's current view of a sub-order and apply [`transition`] to it.
///
/// `Ok(None)` means nothing changed — either the provider has no record of the sub-order
/// yet (`Ok(None)` from [`TelephonyProvider::sub_order_status`], not an error: a brand-new
/// order legitimately has nothing to report yet) or [`transition`] itself found the state
/// indecisive. The caller keeps showing the number's last known status in both cases.
pub async fn reconcile_one(
    pool: &Pool,
    provider: &dyn TelephonyProvider,
    number_id: Uuid,
    org_id: Uuid,
    current: NumberStatus,
    sub_order: &SubOrderId,
) -> Result<Option<Transition>, ReconcileError> {
    let Some(state) = provider
        .sub_order_status(sub_order)
        .await
        .map_err(ReconcileError::Provider)?
    else {
        return Ok(None);
    };

    let Some(t) = transition(current, &state) else {
        return Ok(None);
    };

    sqlx::query(
        "UPDATE voip_numbers
            SET status = $3, status_reason = $4, outbound_enabled = $5, updated_at = now()
          WHERE id = $1 AND org_id = $2",
    )
    .bind(number_id)
    .bind(org_id)
    .bind(t.status.as_str())
    .bind(&t.reason)
    .bind(t.outbound)
    .execute(pool)
    .await
    .map_err(ReconcileError::Storage)?;

    Ok(Some(t))
}

// ---------------------------------------------------------------------------------------
// HTTP handlers (spec 0119) — registered by `crate::voip::routes` under
// `…/voip/numbers/{number_id}/requirements`.
// ---------------------------------------------------------------------------------------

/// The longest a submitted text or address field may be before it is forwarded to the
/// provider. Same magnitude as [`MAX_REASON_CHARS`] and for the same reason: a column (or
/// here, a carrier request) is not the place for an unbounded string a customer typed.
const MAX_VALUE_CHARS: usize = 500;

fn provider(state: &AppState) -> Result<&dyn TelephonyProvider, Response> {
    state
        .telephony
        .as_deref()
        .ok_or_else(|| not_found("voip is not enabled"))
}

/// Map a regulatory-requirements provider failure onto a refusal a customer can act on.
///
/// `DestinationRefused` is what [`crate::telephony::telnyx::classify`] maps a Telnyx 422
/// onto for every call in this crate, regulatory ones included — and design D12 has those
/// specific calls read their error body **redacted** (never logged in full) because
/// Telnyx's own 422 bodies are known to echo back exactly the value that was rejected. So
/// this refusal carries only a stable code, never provider prose: there is no rejection
/// text in this process to surface even if the code wanted to.
fn provider_err(e: ProviderError) -> Response {
    match e {
        ProviderError::Unsupported { .. } => {
            refuse(StatusCode::NOT_IMPLEMENTED, "requirements_unsupported")
        }
        ProviderError::Unauthorized | ProviderError::AccountBlocked => {
            refuse(StatusCode::SERVICE_UNAVAILABLE, "voip_misconfigured")
        }
        ProviderError::RateLimited => {
            refuse(StatusCode::TOO_MANY_REQUESTS, "provider_rate_limited")
        }
        ProviderError::DestinationRefused => {
            refuse(StatusCode::BAD_REQUEST, "provider_rejected_value")
        }
        ProviderError::Unavailable { .. } | ProviderError::Malformed { .. } => {
            refuse(StatusCode::BAD_GATEWAY, "provider_unavailable")
        }
    }
}

fn group_err(e: GroupError) -> Response {
    match e {
        GroupError::Busy => refuse(StatusCode::CONFLICT, "requirements_busy"),
        GroupError::Provider(p) => provider_err(p),
        GroupError::Storage(s) => db_err(s),
    }
}

fn reconcile_err(e: ReconcileError) -> Response {
    match e {
        ReconcileError::Provider(p) => provider_err(p),
        ReconcileError::Storage(s) => db_err(s),
    }
}

/// Everything a handler needs from `voip_numbers` before it can touch requirements at all.
struct NumberRow {
    status: NumberStatus,
    status_reason: Option<String>,
    provider_sub_order_id: Option<String>,
    country: String,
    number_kind: Option<String>,
    /// `NULL` means this number never had a regulatory requirement in the first place —
    /// the gate for `regulatory_not_required`, distinct from a legacy row that HAD one but
    /// predates the sub-order id column (`regulatory_unlinked`).
    regulatory_requirement: Option<String>,
}

/// `(status, status_reason, provider_sub_order_id, country, number_kind,
/// regulatory_requirement)` — exactly [`load_number`]'s `SELECT` list, named so the
/// function signature does not trip clippy's type-complexity lint.
type NumberRowTuple = (
    String,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
);

/// Org-scoped load. `not_found` for a missing or cross-org id — the same tenancy rule
/// every other handler in `numbers.rs` already follows (never a 403 that would confirm a
/// number id exists in someone else's organisation).
async fn load_number(pool: &Pool, org_id: Uuid, number_id: Uuid) -> Result<NumberRow, Response> {
    let row: Option<NumberRowTuple> = sqlx::query_as(
        "SELECT status, status_reason, provider_sub_order_id, country, number_kind,
                regulatory_requirement
           FROM voip_numbers WHERE id = $1 AND org_id = $2",
    )
    .bind(number_id)
    .bind(org_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    let Some((
        status,
        status_reason,
        provider_sub_order_id,
        country,
        number_kind,
        regulatory_requirement,
    )) = row
    else {
        return Err(not_found("number not found"));
    };
    Ok(NumberRow {
        status: NumberStatus::parse(&status),
        status_reason,
        provider_sub_order_id,
        country,
        number_kind,
        regulatory_requirement,
    })
}

/// Requirement Discovery's two "this route does not apply" refusals (spec 0119).
fn gate_regulatable(row: &NumberRow) -> Result<(), Response> {
    if row.regulatory_requirement.is_none() {
        return Err(refuse(StatusCode::CONFLICT, "regulatory_not_required"));
    }
    if row.provider_sub_order_id.is_none() {
        return Err(refuse(StatusCode::CONFLICT, "regulatory_unlinked"));
    }
    Ok(())
}

/// Record the group a number's paperwork lives in, once — idempotent, so calling this
/// again for a number that already has one is a harmless no-op.
async fn link_group(pool: &Pool, number_id: Uuid, group_row_id: Uuid) -> Result<(), Response> {
    sqlx::query(
        "UPDATE voip_numbers SET requirement_group_id = $2, updated_at = now()
          WHERE id = $1 AND requirement_group_id IS NULL",
    )
    .bind(number_id)
    .bind(group_row_id)
    .execute(pool)
    .await
    .map_err(db_err)?;
    Ok(())
}

/// One requirement's document row, projected down to exactly what design D12 allows this
/// route to hand back: never the provider's own document id, only whether it passed scan.
async fn fetch_docs(pool: &Pool, group_row_id: Uuid) -> Result<HashMap<String, String>, Response> {
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT requirement_id, av_scan_status FROM voip_requirement_documents
          WHERE group_id = $1",
    )
    .bind(group_row_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    Ok(rows
        .into_iter()
        .filter_map(|(req_id, status)| status.map(|s| (req_id, s)))
        .collect())
}

/// A submitted [`FieldValue`] as JSON. Only ever built from a value that was submitted
/// (ours, echoed back by the provider or the mock) — never from provider prose, so there
/// is nothing here design D12's redaction rule needs to protect against.
fn value_json(v: &FieldValue) -> Value {
    match v {
        FieldValue::Text(s) => json!(s),
        FieldValue::Address(a) => json!({
            "first_name": a.first_name,
            "last_name": a.last_name,
            "business_name": a.business_name,
            "street_address": a.street_address,
            "extended_address": a.extended_address,
            "locality": a.locality,
            "administrative_area": a.administrative_area,
            "postal_code": a.postal_code,
            "country_code": a.country_code,
        }),
        // Never rendered: a Document-kind requirement's own branch below renders
        // `document` from OUR OWN table instead, and PUT refuses a `Document` value
        // outright (see `put_requirements`) — so this arm exists only so the match is
        // exhaustive, not because a caller is expected to reach it.
        FieldValue::Document(id) => json!(id),
    }
}

/// One requirement entry in the discovery/submission view, exactly the shape design's
/// route table promises: `{id, name, description, example, kind, value?, document?}`.
fn requirement_entry(
    spec: &RequirementSpec,
    value: &Option<FieldValue>,
    doc_av_scan_status: Option<&String>,
) -> Value {
    let mut entry = json!({
        "id": spec.id,
        "name": spec.name,
        "description": spec.description,
        "example": spec.example,
        "kind": spec.kind.as_str(),
    });
    if spec.kind == RequirementKind::Document {
        if let Some(status) = doc_av_scan_status {
            entry["document"] = json!({ "av_scan_status": status });
        }
    } else if let Some(v) = value {
        entry["value"] = value_json(v);
    }
    entry
}

/// The full discovery/submission view: the number's own status plus the group's.
#[allow(clippy::too_many_arguments)]
fn build_view(
    number_status: NumberStatus,
    status_reason: Option<&str>,
    group_status: GroupStatus,
    reused: bool,
    requirements: &[(RequirementSpec, Option<FieldValue>)],
    docs: &HashMap<String, String>,
) -> Value {
    json!({
        "status": number_status.as_str(),
        "status_reason": status_reason,
        "group": { "status": group_status.as_str(), "reused": reused },
        "requirements": requirements
            .iter()
            .map(|(spec, value)| requirement_entry(spec, value, docs.get(&spec.id)))
            .collect::<Vec<_>>(),
    })
}

/// `GET …/voip/numbers/{number_id}/requirements` — what the provider still wants, and
/// what has already been filled in (spec 0119 "Requirement Discovery").
pub async fn get_requirements(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, number_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;
    let row = load_number(pool, org_id, number_id).await?;
    gate_regulatable(&row)?;

    let query = RequirementQuery {
        country: row.country.clone(),
        kind: NumberKind::parse(row.number_kind.as_deref().unwrap_or("")),
        action: RequirementAction::Ordering,
    };
    let ensured = ensure_group(pool, provider(&state)?, org_id, &query, &org_id.to_string())
        .await
        .map_err(group_err)?;
    link_group(pool, number_id, ensured.row_id).await?;
    let docs = fetch_docs(pool, ensured.row_id).await?;

    Ok(Json(build_view(
        row.status,
        row.status_reason.as_deref(),
        ensured.status,
        ensured.reused,
        &ensured.requirements,
        &docs,
    ))
    .into_response())
}

#[derive(Debug, Deserialize)]
struct AddressJson {
    first_name: String,
    last_name: String,
    business_name: String,
    street_address: String,
    #[serde(default)]
    extended_address: Option<String>,
    locality: String,
    #[serde(default)]
    administrative_area: Option<String>,
    postal_code: String,
    country_code: String,
}

impl AddressJson {
    /// Every field this crate bounds (design's own text fields), checked before a single
    /// byte reaches the provider.
    fn longest_field_chars(&self) -> usize {
        [
            &self.first_name,
            &self.last_name,
            &self.business_name,
            &self.street_address,
            &self.locality,
            &self.postal_code,
            &self.country_code,
        ]
        .into_iter()
        .chain(self.extended_address.iter())
        .chain(self.administrative_area.iter())
        .map(|s| s.chars().count())
        .max()
        .unwrap_or(0)
    }
}

impl From<AddressJson> for AddressValue {
    fn from(a: AddressJson) -> Self {
        AddressValue {
            first_name: a.first_name,
            last_name: a.last_name,
            business_name: a.business_name,
            street_address: a.street_address,
            extended_address: a.extended_address,
            locality: a.locality,
            administrative_area: a.administrative_area,
            postal_code: a.postal_code,
            country_code: a.country_code,
        }
    }
}

#[derive(Debug, Deserialize)]
struct PutValueBody {
    requirement_id: String,
    value: Value,
}

#[derive(Debug, Deserialize)]
pub struct PutRequirementsBody {
    values: Vec<PutValueBody>,
}

/// Validate one submitted value against the requirement's own kind, and turn it into a
/// [`FieldValue`] ready to forward. Never partially validates a batch: the caller collects
/// this into a `Result<Vec<_>, Response>` so one bad entry refuses the whole PUT rather
/// than forwarding some values and silently dropping others.
fn validate_value(spec: &RequirementSpec, value: &Value) -> Result<FieldValue, Response> {
    match spec.kind {
        RequirementKind::Textual => {
            let Some(s) = value.as_str() else {
                return Err(refuse(StatusCode::BAD_REQUEST, "requirement_kind_mismatch"));
            };
            if s.chars().count() > MAX_VALUE_CHARS {
                return Err(refuse(StatusCode::BAD_REQUEST, "value_too_long"));
            }
            Ok(FieldValue::Text(s.to_string()))
        }
        RequirementKind::Address => {
            let addr: AddressJson = serde_json::from_value(value.clone())
                .map_err(|_| refuse(StatusCode::BAD_REQUEST, "requirement_kind_mismatch"))?;
            if addr.longest_field_chars() > MAX_VALUE_CHARS {
                return Err(refuse(StatusCode::BAD_REQUEST, "value_too_long"));
            }
            Ok(FieldValue::Address(addr.into()))
        }
        // A document is fulfilled through the upload route, never through a raw value in
        // this body — accepting one here would let a client claim a scan result it never
        // earned.
        RequirementKind::Document => {
            Err(refuse(StatusCode::BAD_REQUEST, "requirement_kind_mismatch"))
        }
    }
}

/// `PUT …/voip/numbers/{number_id}/requirements` — fill in (or correct) textual and
/// address fields (spec 0119 "Requirement Submission").
pub async fn put_requirements(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, number_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<PutRequirementsBody>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;
    let row = load_number(pool, org_id, number_id).await?;
    gate_regulatable(&row)?;
    if !matches!(
        row.status,
        NumberStatus::PendingRegulatory | NumberStatus::RegulatoryRejected
    ) {
        return Err(refuse(StatusCode::CONFLICT, "requirements_not_editable"));
    }

    let query = RequirementQuery {
        country: row.country.clone(),
        kind: NumberKind::parse(row.number_kind.as_deref().unwrap_or("")),
        action: RequirementAction::Ordering,
    };
    let telephony = provider(&state)?;
    let ensured = ensure_group(pool, telephony, org_id, &query, &org_id.to_string())
        .await
        .map_err(group_err)?;
    link_group(pool, number_id, ensured.row_id).await?;

    let mut values = Vec::with_capacity(body.values.len());
    for entry in &body.values {
        let Some((spec, _)) = ensured
            .requirements
            .iter()
            .find(|(spec, _)| spec.id == entry.requirement_id)
        else {
            return Err(refuse(StatusCode::BAD_REQUEST, "requirement_unknown"));
        };
        let field_value = validate_value(spec, &entry.value)?;
        values.push((entry.requirement_id.clone(), field_value));
    }

    let updated = telephony
        .submit_requirement_values(&ensured.provider_group_id, &values)
        .await
        .map_err(provider_err)?;
    let docs = fetch_docs(pool, ensured.row_id).await?;

    Ok(Json(build_view(
        row.status,
        row.status_reason.as_deref(),
        updated.status,
        ensured.reused,
        &updated.requirements,
        &docs,
    ))
    .into_response())
}

/// `POST …/voip/numbers/{number_id}/requirements/submit` — attach the (fully filled)
/// group to the sub-order, starting the provider's own review (spec 0119).
pub async fn submit_requirements(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, number_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;
    let row = load_number(pool, org_id, number_id).await?;
    gate_regulatable(&row)?;
    if row.status == NumberStatus::RegulatoryReview {
        return Err(refuse(StatusCode::CONFLICT, "submission_already_pending"));
    }
    if !matches!(
        row.status,
        NumberStatus::PendingRegulatory | NumberStatus::RegulatoryRejected
    ) {
        return Err(refuse(StatusCode::CONFLICT, "requirements_not_editable"));
    }

    let query = RequirementQuery {
        country: row.country.clone(),
        kind: NumberKind::parse(row.number_kind.as_deref().unwrap_or("")),
        action: RequirementAction::Ordering,
    };
    let telephony = provider(&state)?;
    let ensured = ensure_group(pool, telephony, org_id, &query, &org_id.to_string())
        .await
        .map_err(group_err)?;
    link_group(pool, number_id, ensured.row_id).await?;

    if ensured.requirements.iter().any(|(_, v)| v.is_none()) {
        return Err(refuse(StatusCode::CONFLICT, "requirements_incomplete"));
    }

    // `provider_sub_order_id` is guaranteed by `gate_regulatable` above.
    let sub_order = SubOrderId(row.provider_sub_order_id.clone().unwrap_or_default());
    telephony
        .attach_requirement_group(&sub_order, &ensured.provider_group_id)
        .await
        .map_err(provider_err)?;

    // Unconditional, per design's own sequence: attaching a group is what STARTS the
    // provider's review, regardless of what `attach_requirement_group`'s own returned
    // `SubOrderState` happens to already report — the sweep and `/refresh` are what read
    // the review's outcome back later.
    sqlx::query(
        "UPDATE voip_numbers
            SET status = 'regulatory_review', status_reason = NULL, outbound_enabled = FALSE,
                regulatory_next_check_at = now() + interval '2 minutes', updated_at = now()
          WHERE id = $1 AND org_id = $2",
    )
    .bind(number_id)
    .bind(org_id)
    .execute(pool)
    .await
    .map_err(db_err)?;

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "status": NumberStatus::RegulatoryReview.as_str() })),
    )
        .into_response())
}

/// `POST …/voip/numbers/{number_id}/requirements/refresh` — read the provider's current
/// status on demand, without waiting for the sweep (spec 0119 "Reconcile Sweep").
pub async fn refresh_requirements(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, number_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, MEMBER).await?;
    let row = load_number(pool, org_id, number_id).await?;
    gate_regulatable(&row)?;

    if !state.rate_limiter.allow(
        &format!("reqrefresh:{number_id}"),
        1,
        Duration::from_secs(30),
    ) {
        return Err(refuse(StatusCode::TOO_MANY_REQUESTS, "refresh_too_soon"));
    }

    let telephony = provider(&state)?;
    // Guaranteed present by `gate_regulatable` above.
    let sub_order = SubOrderId(row.provider_sub_order_id.clone().unwrap_or_default());
    let outcome = reconcile_one(pool, telephony, number_id, org_id, row.status, &sub_order)
        .await
        .map_err(reconcile_err)?;

    let (status, reason) = match outcome {
        Some(t) => (t.status, t.reason),
        None => (row.status, row.status_reason.clone()),
    };
    Ok(Json(json!({ "status": status.as_str(), "status_reason": reason })).into_response())
}

// ---------------------------------------------------------------------------------------
// Document upload — `POST …/requirements/documents` (Phase 6, spec 0119 "Document
// Stream-Through", design D9-D12).
// ---------------------------------------------------------------------------------------

/// File size cap (design D10). Telnyx itself allows up to 20 MB; this crate halves that,
/// because every accepted format here (identity/business proof) is a PDF or a photo, not
/// a scanned binder.
const MAX_DOCUMENT_BYTES: u64 = 10 * 1024 * 1024;

/// The route-level [`axum::extract::DefaultBodyLimit`] — a safety net one MiB above
/// [`MAX_DOCUMENT_BYTES`], never the enforcement itself (that is the byte-counting pump
/// below, which answers a stable `document_too_large` code instead of axum's own opaque
/// body-limit rejection).
pub const DOCUMENT_BODY_LIMIT: usize = 11 * 1024 * 1024;

/// Normalise Telnyx's real `av_scan_status` wire vocabulary (`scanned`/`infected`/
/// `pending_scan`/`not_scanned`, verified PR4) onto migration 064's restricted CHECK
/// vocabulary (`pending`/`passed`/`failed`) — task 6.8. A direct pass-through write would
/// violate the CHECK for every value except a lucky no-op, so this runs on every write,
/// never only for the values that happen to already fit. `infected` must NEVER become
/// `passed`; anything unrecognised defaults to `pending`, never a guessed success.
fn normalize_scan_status(raw: &str) -> &'static str {
    match raw.trim().to_ascii_lowercase().as_str() {
        "scanned" => "passed",
        "infected" => "failed",
        _ => "pending",
    }
}

/// Cross-check a declared multipart `Content-Type` against the magic bytes of the first
/// chunk actually received (design D10, task 6.7): a client can label anything, so both
/// must agree before the bytes ever reach the provider. Returns the canonical `'static`
/// MIME string [`DocumentUpload`] expects, or `None` if the format is unsupported or the
/// two signals disagree (a spoofed declaration).
fn sniff_allowed_type(declared: &str, first_chunk: &[u8]) -> Option<&'static str> {
    let declared = declared
        .split(';')
        .next()
        .unwrap_or(declared)
        .trim()
        .to_ascii_lowercase();
    let sniffed: &'static str = if first_chunk.starts_with(b"%PDF-") {
        "application/pdf"
    } else if first_chunk.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if first_chunk.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else {
        return None;
    };
    let declared_matches = match sniffed {
        "application/pdf" => declared == "application/pdf",
        "image/png" => declared == "image/png",
        "image/jpeg" => matches!(declared.as_str(), "image/jpeg" | "image/jpg"),
        _ => false,
    };
    declared_matches.then_some(sniffed)
}

/// Why the pump stopped before the field was fully read.
enum PumpError {
    /// [`MAX_DOCUMENT_BYTES`] was exceeded mid-stream (D10, task 6.6). The channel
    /// already carries an `Err`, which aborts the outbound request to the provider.
    TooLarge,
    /// The multipart body itself was malformed mid-read.
    Malformed,
}

/// Pump a multipart file field's chunks into `tx`, one at a time, counting bytes as they
/// go (design D9/D10). `first_chunk` was already read by the caller (to sniff its magic
/// bytes) and is sent first here so nothing the client uploaded is silently dropped.
///
/// This is the entire point of D9: an axum `Field<'_>` cannot outlive the request, but the
/// `mpsc::Sender` side can be turned into a `'static` `Stream` (see
/// [`upload_requirement_document`]) that `reqwest::Body::wrap_stream` accepts — so a file
/// many times larger than available memory streams straight through without ever being
/// buffered here or written to disk.
async fn pump_field(
    mut field: axum::extract::multipart::Field<'_>,
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
    first_chunk: Bytes,
) -> Result<u64, PumpError> {
    let mut total = first_chunk.len() as u64;
    if tx.send(Ok(first_chunk)).await.is_err() {
        // The provider side already gave up reading (e.g. it rejected the request
        // outright) — nothing left for the pump to do.
        return Ok(total);
    }
    loop {
        let chunk = match field.chunk().await {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(_) => {
                let _ = tx
                    .send(Err(std::io::Error::other("malformed multipart body")))
                    .await;
                return Err(PumpError::Malformed);
            }
        };
        total += chunk.len() as u64;
        if total > MAX_DOCUMENT_BYTES {
            // Send `Err`, not just stop: this is what aborts the outbound request to the
            // provider immediately, instead of it waiting on a stream that silently ends.
            let _ = tx
                .send(Err(std::io::Error::other("document exceeds the size cap")))
                .await;
            return Err(PumpError::TooLarge);
        }
        if tx.send(Ok(chunk)).await.is_err() {
            break;
        }
    }
    Ok(total)
}

/// Read and validate the `requirement_id` field. Design D11: it MUST be the first
/// multipart field, checked before a single byte of the file field is read — a client
/// that sends the file first is refused outright, not merely warned.
async fn read_requirement_id_field(multipart: &mut Multipart) -> Result<String, Response> {
    let malformed = || refuse(StatusCode::BAD_REQUEST, "malformed_upload");
    let field = multipart
        .next_field()
        .await
        .map_err(|_| malformed())?
        .ok_or_else(malformed)?;
    if field.name() != Some("requirement_id") {
        return Err(malformed());
    }
    let value = field.text().await.map_err(|_| malformed())?;
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(malformed());
    }
    Ok(value)
}

/// Read the `file` field that must follow `requirement_id` (design D11).
async fn read_file_field(
    multipart: &mut Multipart,
) -> Result<axum::extract::multipart::Field<'_>, Response> {
    let malformed = || refuse(StatusCode::BAD_REQUEST, "malformed_upload");
    let field = multipart
        .next_field()
        .await
        .map_err(|_| malformed())?
        .ok_or_else(malformed)?;
    if field.name() != Some("file") {
        return Err(malformed());
    }
    Ok(field)
}

/// `POST …/voip/numbers/{number_id}/requirements/documents` — stream a document straight
/// through to the provider and link it to its requirement in the same request (spec 0119
/// "Document Stream-Through", design D9-D12).
pub async fn upload_requirement_document(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, number_id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    require_role(pool, org_id, user.user_id, ADMIN).await?;

    // Rate-limited BEFORE a single multipart byte is read (design's route table): this
    // gate must not depend on how big the (still unread) body turns out to be.
    let ip = crate::observability::client_ip(&headers);
    if !state
        .rate_limiter
        .allow(&format!("reqdoc-ip:{ip}"), 10, Duration::from_secs(60))
        || !state.rate_limiter.allow(
            &format!("reqdoc-org:{org_id}"),
            30,
            Duration::from_secs(3600),
        )
    {
        return Err(refuse(StatusCode::TOO_MANY_REQUESTS, "too_many_requests"));
    }

    let row = load_number(pool, org_id, number_id).await?;
    gate_regulatable(&row)?;
    if !matches!(
        row.status,
        NumberStatus::PendingRegulatory | NumberStatus::RegulatoryRejected
    ) {
        return Err(refuse(StatusCode::CONFLICT, "requirements_not_editable"));
    }

    let query = RequirementQuery {
        country: row.country.clone(),
        kind: NumberKind::parse(row.number_kind.as_deref().unwrap_or("")),
        action: RequirementAction::Ordering,
    };
    let telephony = provider(&state)?;
    let ensured = ensure_group(pool, telephony, org_id, &query, &org_id.to_string())
        .await
        .map_err(group_err)?;
    link_group(pool, number_id, ensured.row_id).await?;

    let requirement_id = read_requirement_id_field(&mut multipart).await?;
    let Some((spec, _)) = ensured
        .requirements
        .iter()
        .find(|(spec, _)| spec.id == requirement_id)
    else {
        return Err(refuse(StatusCode::BAD_REQUEST, "requirement_unknown"));
    };
    if spec.kind != RequirementKind::Document {
        return Err(refuse(StatusCode::BAD_REQUEST, "requirement_kind_mismatch"));
    }

    let mut field = read_file_field(&mut multipart).await?;
    let declared_type = field.content_type().unwrap_or("").to_string();
    let first_chunk = field
        .chunk()
        .await
        .map_err(|_| refuse(StatusCode::BAD_REQUEST, "malformed_upload"))?
        .ok_or_else(|| refuse(StatusCode::BAD_REQUEST, "malformed_upload"))?;

    let Some(content_type) = sniff_allowed_type(&declared_type, &first_chunk) else {
        return Err(refuse(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "document_type_unsupported",
        ));
    };

    // D9: an mpsc channel is the bridge from the non-`'static` `Field` to a `'static`
    // `Stream` `reqwest::Body::wrap_stream` can accept. Both sides run concurrently below
    // (not one after the other) so the bounded channel (capacity 4) never deadlocks:
    // `upload_document` must be actively draining the receiver while the pump still has
    // chunks left to send.
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(4);
    let body: BoxStream<'static, Result<Bytes, std::io::Error>> =
        stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        })
        .boxed();
    let upload = DocumentUpload { content_type, body };

    let (pump_result, upload_result) = tokio::join!(
        pump_field(field, tx, first_chunk),
        telephony.upload_document(upload)
    );

    let total_bytes = match pump_result {
        Ok(total) => total,
        Err(PumpError::TooLarge) => {
            return Err(refuse(StatusCode::PAYLOAD_TOO_LARGE, "document_too_large"))
        }
        Err(PumpError::Malformed) => {
            return Err(refuse(StatusCode::BAD_REQUEST, "malformed_upload"))
        }
    };
    let uploaded = upload_result.map_err(provider_err)?;

    // D11: the link happens in the SAME request, right after the upload. Telnyx deletes
    // an unlinked document after 30 minutes, so a failure here is never retried — the
    // orphan simply expires rather than this handler trying again with stale state.
    telephony
        .submit_requirement_values(
            &ensured.provider_group_id,
            &[(
                requirement_id.clone(),
                FieldValue::Document(uploaded.id.clone()),
            )],
        )
        .await
        .map_err(|_| refuse(StatusCode::BAD_GATEWAY, "document_link_failed"))?;

    // Only reached once the link succeeded: no row is persisted for a document nobody
    // has actually attached to the sub-order yet (D12: content_type + size only, no
    // filename, no bytes — task 6.8 normalises the scan status before it touches the
    // CHECK-constrained column).
    let scan_status = normalize_scan_status(&uploaded.av_scan_status);
    sqlx::query(
        "INSERT INTO voip_requirement_documents
            (group_id, org_id, requirement_id, provider_document_id, av_scan_status,
             content_type, size_bytes, uploaded_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (group_id, requirement_id) DO UPDATE
             SET provider_document_id = EXCLUDED.provider_document_id,
                 av_scan_status = EXCLUDED.av_scan_status,
                 content_type = EXCLUDED.content_type,
                 size_bytes = EXCLUDED.size_bytes,
                 uploaded_by = EXCLUDED.uploaded_by",
    )
    .bind(ensured.row_id)
    .bind(org_id)
    .bind(&requirement_id)
    .bind(&uploaded.id)
    .bind(scan_status)
    .bind(content_type)
    .bind(total_bytes as i32)
    .bind(user.user_id)
    .execute(pool)
    .await
    .map_err(db_err)?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "requirement_id": requirement_id,
            "document": { "av_scan_status": scan_status },
        })),
    )
        .into_response())
}

// ---------------------------------------------------------------------------------------
// Reconcile sweep + webhook nudge (Phase 7, spec 0119 "Reconcile Sweep"/"Webhook
// Fast-Path", design D5/D6/D7/D8). The sweep is what actually MOVES a number's status;
// the webhook (`nudge`) only ever makes it check sooner.
// ---------------------------------------------------------------------------------------

/// How long a claimed row is pinned before its own outcome reschedules it — long enough
/// that two overlapping sweep ticks (or two replicas) never both see it as due, short
/// enough that a crash between the claim and the outcome write self-heals within one
/// grace period rather than jamming the row forever.
const CLAIM_HOLD_SECS: i64 = 10 * 60;
/// Re-check cadence once requirements are under the provider's own review.
const REVIEW_RECHECK_SECS: i64 = 5 * 60;
/// Re-check cadence while still pending info, or resubmittable after a rejection.
const PENDING_RECHECK_SECS: i64 = 30 * 60;
/// A provider that cannot answer this call at all (a capability gap, not an outage) is
/// asked again once a day rather than on the exponential ladder below.
const UNSUPPORTED_RECHECK_SECS: i64 = 24 * 60 * 60;
const BACKOFF_BASE_SECS: i64 = 5 * 60;
const BACKOFF_MAX_SECS: i64 = 6 * 60 * 60;

/// The cadence a DECISIVE outcome earns, by the status it just produced (design's sweep
/// note: "review → 5 min; pending/rejected → 30 min"). `None` for a terminal status: the
/// row already drops out of the partial index migration 064 defines
/// (`idx_voip_numbers_regulatory`), so nothing needs to check it again.
fn recheck_secs_for(status: NumberStatus) -> Option<i64> {
    match status {
        NumberStatus::RegulatoryReview => Some(REVIEW_RECHECK_SECS),
        NumberStatus::PendingRegulatory | NumberStatus::RegulatoryRejected => {
            Some(PENDING_RECHECK_SECS)
        }
        _ => None,
    }
}

/// `min(5min · 2^(failures-1), 6h)` — design's own backoff formula. `failures_after` is
/// counted AFTER the failure this call is scheduling for, so the first-ever failure gets
/// the base interval rather than double it. `saturating_mul` before the final `min` means
/// a very large failure count clamps instead of overflowing.
fn backoff_secs(failures_after: i32) -> i64 {
    let exponent = failures_after.saturating_sub(1).clamp(0, 32);
    BACKOFF_BASE_SECS
        .saturating_mul(1i64 << exponent)
        .min(BACKOFF_MAX_SECS)
}

/// One row [`reconcile_due`] claimed for this tick — exactly its own `RETURNING` list.
#[derive(sqlx::FromRow)]
struct DueRow {
    id: Uuid,
    org_id: Uuid,
    status: String,
    provider_sub_order_id: Option<String>,
    regulatory_failures: i32,
    requirement_group_id: Option<Uuid>,
    country: String,
    number_kind: Option<String>,
}

/// What one sweep pass did, for the caller to log.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileSummary {
    /// Rows where a decisive transition was read and applied.
    pub reconciled: u64,
    /// Rows read but left unchanged (an indecisive answer, a provider error, or a
    /// capability gap) — still processed, just not moved.
    pub unchanged: u64,
}

impl ReconcileSummary {
    pub fn is_empty(&self) -> bool {
        self.reconciled == 0 && self.unchanged == 0
    }
}

enum RowOutcome {
    Applied,
    Unchanged,
    RateLimited,
}

/// Poll every open sub-order past its own next-check time and apply the same projection
/// the webhook path would (design D5) — the sweep is what actually MOVES a number's
/// status; the webhook only ever nudges this to run sooner (see [`nudge`]).
///
/// Claims up to `batch` due rows with `FOR UPDATE SKIP LOCKED` and immediately pushes
/// their own `regulatory_next_check_at` forward by [`CLAIM_HOLD_SECS`] — safe across
/// replicas: two ticks racing each other lock disjoint rows, and a tick that crashes
/// mid-batch leaves its claimed rows self-healing once the hold expires rather than stuck
/// forever.
pub async fn reconcile_due(
    pool: &Pool,
    provider: &dyn TelephonyProvider,
    batch: i64,
) -> Result<ReconcileSummary, sqlx::Error> {
    let claimed: Vec<DueRow> = sqlx::query_as(
        "UPDATE voip_numbers
            SET regulatory_next_check_at = now() + make_interval(secs => $2::double precision)
          WHERE id IN (
              SELECT id FROM voip_numbers
               WHERE status IN ('pending_regulatory', 'regulatory_review', 'regulatory_rejected')
                 AND provider_sub_order_id IS NOT NULL
                 AND regulatory_next_check_at IS NOT NULL
                 AND regulatory_next_check_at <= now()
               ORDER BY regulatory_next_check_at
               LIMIT $1
                 FOR UPDATE SKIP LOCKED
          )
          RETURNING id, org_id, status, provider_sub_order_id, regulatory_failures,
                    requirement_group_id, country, number_kind",
    )
    .bind(batch.clamp(1, 200))
    .bind(CLAIM_HOLD_SECS as f64)
    .fetch_all(pool)
    .await?;

    let mut summary = ReconcileSummary::default();
    for row in claimed {
        match reconcile_claimed_row(pool, provider, &row).await? {
            RowOutcome::RateLimited => break,
            RowOutcome::Applied => summary.reconciled += 1,
            RowOutcome::Unchanged => summary.unchanged += 1,
        }
    }
    Ok(summary)
}

async fn reconcile_claimed_row(
    pool: &Pool,
    provider: &dyn TelephonyProvider,
    row: &DueRow,
) -> Result<RowOutcome, sqlx::Error> {
    let current = NumberStatus::parse(&row.status);
    // Guaranteed present by the claim query's own `provider_sub_order_id IS NOT NULL`.
    let sub_order = SubOrderId(row.provider_sub_order_id.clone().unwrap_or_default());

    // design D8: a number nobody has opened the panel for yet may already sit behind an
    // approved, reusable group from an earlier purchase in the same org/country/kind
    // combination — attach it now rather than waiting on a human visit. Best-effort: a
    // provider failure here changes nothing about the read/reschedule below.
    if row.requirement_group_id.is_none() {
        attach_reusable_group_if_any(pool, provider, row, &sub_order).await?;
    }

    let state = match provider.sub_order_status(&sub_order).await {
        Ok(Some(state)) => state,
        // `reconcile_one` (the manual `/refresh` path) treats a fresh `None` as "nothing
        // to report yet" because it may run seconds after a purchase. A row reaching the
        // SWEEP has already survived at least one prior check with a sub-order id the
        // provider itself returned — a provider that has since forgotten it is read as
        // gone, never as still-pending, exactly as `transition()` already treats a
        // cancelled order.
        Ok(None) => SubOrderState {
            order: OrderStatus::Deleted,
            requirements: RequirementsStatus::Unknown,
            group: None,
        },
        // A rate-limited provider gets nothing else asked of it this tick; the row keeps
        // the claim's own `CLAIM_HOLD_SECS` schedule and is retried on the next one.
        Err(ProviderError::RateLimited) => return Ok(RowOutcome::RateLimited),
        // A capability gap, not an outage — asking again in a minute would just waste a
        // call, so this backs off to a full day instead of the exponential ladder below.
        Err(ProviderError::Unsupported { .. }) => {
            reschedule(pool, row.id, UNSUPPORTED_RECHECK_SECS, None).await?;
            return Ok(RowOutcome::Unchanged);
        }
        Err(_) => {
            let failures = row.regulatory_failures + 1;
            reschedule(pool, row.id, backoff_secs(failures), Some(failures)).await?;
            return Ok(RowOutcome::Unchanged);
        }
    };

    let Some(t) = transition(current, &state) else {
        // The provider answered, it just had nothing decisive to say. Reschedule on the
        // CURRENT status's own cadence and clear any accumulated failures — this was a
        // successful read, not an error.
        let secs = recheck_secs_for(current).unwrap_or(PENDING_RECHECK_SECS);
        reschedule(pool, row.id, secs, Some(0)).await?;
        return Ok(RowOutcome::Unchanged);
    };

    apply_transition(pool, row.id, &t).await?;
    Ok(RowOutcome::Applied)
}

/// Write a decisive [`Transition`] and its own next-check schedule in one statement.
async fn apply_transition(pool: &Pool, number_id: Uuid, t: &Transition) -> Result<(), sqlx::Error> {
    let next_check_secs = recheck_secs_for(t.status);
    sqlx::query(
        "UPDATE voip_numbers
            SET status = $2, status_reason = $3, outbound_enabled = $4,
                regulatory_failures = 0,
                regulatory_next_check_at = CASE
                    WHEN $5::double precision IS NULL THEN NULL
                    ELSE now() + make_interval(secs => $5::double precision)
                END,
                updated_at = now()
          WHERE id = $1",
    )
    .bind(number_id)
    .bind(t.status.as_str())
    .bind(&t.reason)
    .bind(t.outbound)
    .bind(next_check_secs.map(|s| s as f64))
    .execute(pool)
    .await?;
    Ok(())
}

/// Reschedule a row without changing its status — a provider error, an indecisive read, or
/// an `Unsupported` capability gap. `failures` is written only when the caller wants it
/// changed (`None` leaves the counter as the claim query already returned it).
async fn reschedule(
    pool: &Pool,
    number_id: Uuid,
    delay_secs: i64,
    failures: Option<i32>,
) -> Result<(), sqlx::Error> {
    match failures {
        Some(f) => {
            sqlx::query(
                "UPDATE voip_numbers
                    SET regulatory_next_check_at = now() + make_interval(secs => $2::double precision),
                        regulatory_failures = $3,
                        updated_at = now()
                  WHERE id = $1",
            )
            .bind(number_id)
            .bind(delay_secs as f64)
            .bind(f)
            .execute(pool)
            .await?;
        }
        None => {
            sqlx::query(
                "UPDATE voip_numbers
                    SET regulatory_next_check_at = now() + make_interval(secs => $2::double precision),
                        updated_at = now()
                  WHERE id = $1",
            )
            .bind(number_id)
            .bind(delay_secs as f64)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

/// design D8's reuse half of the sweep: if this row has no requirement group yet, but an
/// APPROVED group already exists for its org/country/number_kind/ordering combination,
/// attach it to the sub-order and link it — the same effect `GET …/requirements` has when
/// a human opens the panel, done here so a number nobody looked at still profits from an
/// already-cleared combination.
async fn attach_reusable_group_if_any(
    pool: &Pool,
    provider: &dyn TelephonyProvider,
    row: &DueRow,
    sub_order: &SubOrderId,
) -> Result<(), sqlx::Error> {
    let provider_id = provider.metadata().id;
    let number_kind = row.number_kind.as_deref().unwrap_or("");
    let existing: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT id, provider_group_id FROM voip_requirement_groups
          WHERE org_id = $1 AND provider = $2 AND country = $3 AND number_kind = $4
            AND action = 'ordering' AND status = 'approved' AND provider_group_id IS NOT NULL",
    )
    .bind(row.org_id)
    .bind(provider_id)
    .bind(&row.country)
    .bind(number_kind)
    .fetch_optional(pool)
    .await?;

    let Some((group_row_id, provider_group_id)) = existing else {
        return Ok(());
    };

    sqlx::query(
        "UPDATE voip_numbers SET requirement_group_id = $2, updated_at = now()
          WHERE id = $1 AND requirement_group_id IS NULL",
    )
    .bind(row.id)
    .bind(group_row_id)
    .execute(pool)
    .await?;

    // Best-effort: a carrier failure here changes nothing about the read/reschedule the
    // caller still does right after — the next tick tries again.
    let _ = provider
        .attach_requirement_group(sub_order, &RequirementGroupId(provider_group_id))
        .await;
    Ok(())
}

/// Free a requirement-group claim (design D7's `status='creating'` row) whose winner never
/// came back with a provider answer — a crash between the claim and the provider call, or
/// a process killed mid-request. Mirrors [`ensure_group`]'s own release-on-failure DELETE,
/// run on a timer instead of inline, because nobody is waiting on THIS particular claim to
/// fail synchronously.
pub async fn reclaim_stale_groups(pool: &Pool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM voip_requirement_groups
          WHERE status = 'creating'
            AND provider_group_id IS NULL
            AND created_at < now() - interval '10 minutes'",
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// The webhook's entire contribution (design D5/"Webhook Fast-Path"): move the affected
/// numbers' next check to now, so the SWEEP reads live status ahead of schedule. Naturally
/// idempotent — setting `regulatory_next_check_at = now()` twice for the same delivery has
/// the same observable effect as doing it once — so this needs no separate dedupe ledger,
/// and it never touches `status` or `status_reason`: the sweep alone applies those.
pub async fn nudge(
    pool: &Pool,
    provider_id: &'static str,
    sub_order_ids: &[String],
) -> Result<u64, sqlx::Error> {
    if sub_order_ids.is_empty() {
        return Ok(0);
    }
    let result = sqlx::query(
        "UPDATE voip_numbers
            SET regulatory_next_check_at = now(), updated_at = now()
          WHERE provider = $1
            AND provider_sub_order_id = ANY($2)
            AND status IN ('pending_regulatory', 'regulatory_review', 'regulatory_rejected')",
    )
    .bind(provider_id)
    .bind(sub_order_ids)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
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

    // -----------------------------------------------------------------------------------
    // `reconcile_due`'s own schedule — pure, no DB, no provider (spec 0119 "Reconcile
    // Sweep", design's sweep note).
    // -----------------------------------------------------------------------------------

    use crate::voip::regulatory::{backoff_secs, recheck_secs_for};

    #[test]
    fn recheck_cadence_follows_designs_own_review_and_pending_split() {
        assert_eq!(
            recheck_secs_for(NumberStatus::RegulatoryReview),
            Some(5 * 60)
        );
        assert_eq!(
            recheck_secs_for(NumberStatus::PendingRegulatory),
            Some(30 * 60)
        );
        assert_eq!(
            recheck_secs_for(NumberStatus::RegulatoryRejected),
            Some(30 * 60)
        );
        // Terminal statuses drop out of the partial index migration 064 defines — nothing
        // needs to check them again.
        assert_eq!(recheck_secs_for(NumberStatus::Active), None);
        assert_eq!(recheck_secs_for(NumberStatus::Failed), None);
    }

    #[test]
    fn error_backoff_doubles_from_a_five_minute_base_and_caps_at_six_hours() {
        assert_eq!(backoff_secs(1), 5 * 60);
        assert_eq!(backoff_secs(2), 10 * 60);
        assert_eq!(backoff_secs(3), 20 * 60);
        assert_eq!(backoff_secs(4), 40 * 60);
        // Keeps doubling until it would exceed the cap, then clamps rather than overflows.
        assert_eq!(backoff_secs(10), 6 * 60 * 60);
        assert_eq!(backoff_secs(40), 6 * 60 * 60);
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
    use crate::telephony::{
        NumberKind, ProviderError, RequirementAction, RequirementQuery, SubOrderId,
    };
    use crate::voip::regulatory::{ensure_group, reconcile_one, GroupError, ReconcileError};

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

    // -----------------------------------------------------------------------------------
    // `reconcile_one` — the single-row primitive `/refresh` and the Phase 7 sweep share.
    // -----------------------------------------------------------------------------------

    /// A number row this org owns, in `pending_regulatory` with a sub-order id — the only
    /// state `reconcile_one` needs to exist.
    async fn regulated_number(pool: &crate::db::Pool, org_id: Uuid, sub_order_id: &str) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO voip_numbers
                (org_id, provider, provider_number_id, e164, country, status,
                 regulatory_requirement, provider_sub_order_id, number_kind)
             VALUES ($1, 'mock', $2, $3, 'FR', 'pending_regulatory', 'proof required', $4,
                     'mobile')
             RETURNING id",
        )
        .bind(org_id)
        .bind(format!("prov-{}", Uuid::new_v4().simple()))
        .bind(format!(
            "+3312{:08}",
            Uuid::new_v4().as_u128() % 100_000_000
        ))
        .bind(sub_order_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn reconcile_one_applies_a_decisive_transition_and_writes_it() {
        let pool = skip_without_db!();
        let org_id = fresh_org(&pool).await;
        let provider = MockTelephonyProvider::default();
        let sub_order = SubOrderId("suborder-1".into());
        let number_id = regulated_number(&pool, org_id, sub_order.as_str()).await;
        provider.set_sub_order_state(
            &sub_order,
            SubOrderState {
                order: OrderStatus::Pending,
                requirements: RequirementsStatus::UnderReview,
                group: None,
            },
        );

        let outcome = reconcile_one(
            &pool,
            &provider,
            number_id,
            org_id,
            NumberStatus::PendingRegulatory,
            &sub_order,
        )
        .await
        .unwrap();

        let t = outcome.expect("UnderReview is decisive — a transition must be applied");
        assert_eq!(t.status, NumberStatus::RegulatoryReview);

        let (status, outbound): (String, bool) =
            sqlx::query_as("SELECT status, outbound_enabled FROM voip_numbers WHERE id = $1")
                .bind(number_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status, "regulatory_review");
        assert!(!outbound);
    }

    #[tokio::test]
    async fn reconcile_one_leaves_the_row_untouched_when_the_provider_has_nothing_yet() {
        let pool = skip_without_db!();
        let org_id = fresh_org(&pool).await;
        let provider = MockTelephonyProvider::default();
        let sub_order = SubOrderId("suborder-unscripted".into());
        let number_id = regulated_number(&pool, org_id, sub_order.as_str()).await;
        // Deliberately never scripted via `set_sub_order_state`, so the mock answers
        // `Ok(None)` — a brand-new order the provider has nothing to report on yet.

        let outcome = reconcile_one(
            &pool,
            &provider,
            number_id,
            org_id,
            NumberStatus::PendingRegulatory,
            &sub_order,
        )
        .await
        .unwrap();
        assert!(outcome.is_none());

        let status: String = sqlx::query_scalar("SELECT status FROM voip_numbers WHERE id = $1")
            .bind(number_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            status, "pending_regulatory",
            "nothing to change, nothing changed"
        );
    }

    // -----------------------------------------------------------------------------------
    // `reconcile_due` — the batched sweep primitive (spec 0119 "Reconcile Sweep", design
    // D5/D7/D8). `reconcile_one` above is the single-row primitive `/refresh` uses; this is
    // the sweep's own claim-then-process loop over every row past its next-check time.
    // -----------------------------------------------------------------------------------

    use crate::voip::regulatory::{nudge, reclaim_stale_groups, reconcile_due};

    /// `reconcile_due`/`reclaim_stale_groups`/`nudge` all scan or mutate rows across the
    /// WHOLE shared test database, not scoped to one `org_id` the way every other test
    /// above is. Cargo runs tests in this file concurrently, so two such tests racing each
    /// other would otherwise claim, count or clean up each other's rows. A `tokio::sync`
    /// mutex (not `std::sync`) is required here because the guard is held across `.await`
    /// points; it serialises only the tests that touch one of those three functions —
    /// every other test in the file is unaffected.
    static SWEEP_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Push a row's own next-check time into the past, so [`reconcile_due`] treats it as
    /// due right now.
    async fn make_due(pool: &crate::db::Pool, number_id: Uuid) {
        sqlx::query(
            "UPDATE voip_numbers SET regulatory_next_check_at = now() - interval '1 minute'
              WHERE id = $1",
        )
        .bind(number_id)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn reconcile_due_applies_a_decisive_transition_and_schedules_the_review_cadence() {
        let _guard = SWEEP_TEST_LOCK.lock().await;
        let pool = skip_without_db!();
        let org_id = fresh_org(&pool).await;
        let provider = MockTelephonyProvider::default();
        let sub_order = SubOrderId("suborder-due-1".into());
        let number_id = regulated_number(&pool, org_id, sub_order.as_str()).await;
        make_due(&pool, number_id).await;
        provider.set_sub_order_state(
            &sub_order,
            SubOrderState {
                order: OrderStatus::Pending,
                requirements: RequirementsStatus::UnderReview,
                group: None,
            },
        );

        let summary = reconcile_due(&pool, &provider, 25).await.unwrap();
        // `>=` rather than `==`: `reconcile_due` is a global scan, and a REGULATED purchase
        // made via `buy()` anywhere else in this long-running suite (design D8's own
        // schedule-at-purchase fix) can legitimately become "due" and get swept up
        // alongside this test's own row. The row THIS test cares about is checked
        // specifically below; that is the actual assertion.
        assert!(
            summary.reconciled >= 1,
            "expected at least our own row: {summary:?}"
        );

        let (status, next_check): (String, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
            "SELECT status, regulatory_next_check_at FROM voip_numbers WHERE id = $1",
        )
        .bind(number_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "regulatory_review");
        let delta = (next_check.expect("a review row must still be scheduled")
            - chrono::Utc::now())
        .num_seconds();
        assert!((240..=320).contains(&delta), "expected ~5min, got {delta}s");
        // This row is now scheduled ~5min in the future — left uncleaned, it would
        // eventually become "due" again and pollute a LATER test-binary invocation's own
        // `reconcile_due` call in this shared throwaway database.
        forget_number(&pool, number_id).await;
    }

    #[tokio::test]
    async fn reconcile_due_backs_off_on_a_provider_error_without_moving_status() {
        let _guard = SWEEP_TEST_LOCK.lock().await;
        let pool = skip_without_db!();
        let org_id = fresh_org(&pool).await;
        let provider = MockTelephonyProvider::default();
        let sub_order = SubOrderId("suborder-due-err".into());
        let number_id = regulated_number(&pool, org_id, sub_order.as_str()).await;
        make_due(&pool, number_id).await;
        provider.fail_next(
            "sub_order_status",
            ProviderError::Unavailable {
                detail: "carrier outage".into(),
            },
        );

        let summary = reconcile_due(&pool, &provider, 25).await.unwrap();
        // `>=`, same reasoning as the sibling test above — this is a global scan.
        assert!(
            summary.unchanged >= 1,
            "expected at least our own row: {summary:?}"
        );

        let (status, failures, next_check): (String, i32, chrono::DateTime<chrono::Utc>) =
            sqlx::query_as(
                "SELECT status, regulatory_failures, regulatory_next_check_at
                   FROM voip_numbers WHERE id = $1",
            )
            .bind(number_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            status, "pending_regulatory",
            "an error must never move the status"
        );
        assert_eq!(failures, 1);
        let delta = (next_check - chrono::Utc::now()).num_seconds();
        assert!(
            (240..=320).contains(&delta),
            "expected the base 5min backoff, got {delta}s"
        );
        forget_number(&pool, number_id).await;
    }

    #[tokio::test]
    async fn reconcile_due_stops_the_batch_on_a_rate_limited_provider() {
        let _guard = SWEEP_TEST_LOCK.lock().await;
        let pool = skip_without_db!();
        let org_id = fresh_org(&pool).await;
        let provider = MockTelephonyProvider::default();
        let sub_order_a = SubOrderId("suborder-rl-a".into());
        let sub_order_b = SubOrderId("suborder-rl-b".into());
        let number_a = regulated_number(&pool, org_id, sub_order_a.as_str()).await;
        let number_b = regulated_number(&pool, org_id, sub_order_b.as_str()).await;
        make_due(&pool, number_a).await;
        make_due(&pool, number_b).await;
        // Scripted once: whichever row is claimed first hits it, and `reconcile_due` must
        // stop the WHOLE batch there rather than falling through to an unscripted (and
        // therefore misleading) `Ok(None)` on the second row.
        provider.fail_next("sub_order_status", ProviderError::RateLimited);

        // No aggregate assertion on `summary` here: `reconcile_due` is a global scan, and
        // an unrelated row from elsewhere in this long-running suite could legitimately be
        // claimed and processed BEFORE the scripted `RateLimited` error fires (ordered by
        // `regulatory_next_check_at`). What actually matters — that THIS test's own two
        // rows never moved once the batch broke — is checked below regardless of what
        // happened to any other row.
        reconcile_due(&pool, &provider, 25).await.unwrap();

        for id in [number_a, number_b] {
            let status: String =
                sqlx::query_scalar("SELECT status FROM voip_numbers WHERE id = $1")
                    .bind(id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(status, "pending_regulatory", "neither row may have moved");
            forget_number(&pool, id).await;
        }
    }

    #[tokio::test]
    async fn reconcile_due_treats_a_vanished_sub_order_as_a_deadline_miss_and_fails_it() {
        let _guard = SWEEP_TEST_LOCK.lock().await;
        let pool = skip_without_db!();
        let org_id = fresh_org(&pool).await;
        let provider = MockTelephonyProvider::default();
        let sub_order = SubOrderId("suborder-vanished".into());
        let number_id = regulated_number(&pool, org_id, sub_order.as_str()).await;
        make_due(&pool, number_id).await;
        // Deliberately never scripted via `set_sub_order_state`: the provider answers
        // `Ok(None)`, which the SWEEP (unlike `reconcile_one`) reads as gone rather than
        // "not ready yet".

        let summary = reconcile_due(&pool, &provider, 25).await.unwrap();
        assert!(
            summary.reconciled >= 1,
            "expected at least our own row: {summary:?}"
        );

        let (status, next_check): (String, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
            "SELECT status, regulatory_next_check_at FROM voip_numbers WHERE id = $1",
        )
        .bind(number_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "failed");
        assert!(
            next_check.is_none(),
            "a terminal status needs no further check"
        );
        forget_number(&pool, number_id).await;
    }

    #[tokio::test]
    async fn reconcile_due_attaches_an_already_approved_reusable_group_nobody_opened_the_panel_for()
    {
        let _guard = SWEEP_TEST_LOCK.lock().await;
        let pool = skip_without_db!();
        let org_id = fresh_org(&pool).await;
        let provider = MockTelephonyProvider::default();

        // An approved group already exists for this combination (a prior number's own
        // submission went through), but THIS number's row has never had a human visit
        // `GET …/requirements` — `requirement_group_id` is still NULL.
        let approved_group_row: Uuid = sqlx::query_scalar(
            "INSERT INTO voip_requirement_groups
                (org_id, provider, country, number_kind, action, provider_group_id, status)
             VALUES ($1, 'mock', 'FR', 'mobile', 'ordering', $2, 'approved')
             RETURNING id",
        )
        .bind(org_id)
        .bind(format!("mock-group-approved-{}", Uuid::new_v4().simple()))
        .fetch_one(&pool)
        .await
        .unwrap();

        let sub_order = SubOrderId("suborder-reuse-1".into());
        let number_id = regulated_number(&pool, org_id, sub_order.as_str()).await;
        make_due(&pool, number_id).await;

        let summary = reconcile_due(&pool, &provider, 25).await.unwrap();
        assert!(
            summary.reconciled >= 1,
            "attaching the reused group starts the provider's review, which is decisive"
        );

        let (status, group_id): (String, Option<Uuid>) =
            sqlx::query_as("SELECT status, requirement_group_id FROM voip_numbers WHERE id = $1")
                .bind(number_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            group_id,
            Some(approved_group_row),
            "the reusable group must be linked"
        );
        assert_eq!(
            status, "regulatory_review",
            "attaching a group starts the provider's review"
        );
        forget_number(&pool, number_id).await;
        sqlx::query("DELETE FROM voip_requirement_groups WHERE id = $1")
            .bind(approved_group_row)
            .execute(&pool)
            .await
            .unwrap();
    }

    // -----------------------------------------------------------------------------------
    // `reclaim_stale_groups` — frees a design D7 claim row whose winner never came back
    // (spec 0119, Phase 7).
    // -----------------------------------------------------------------------------------

    #[tokio::test]
    async fn a_stale_creating_group_claim_is_reclaimed_by_the_sweep() {
        // `reclaim_stale_groups` scans the WHOLE table, exactly like `reconcile_due` —
        // shares the same lock so a sibling scan never claims or counts this test's rows.
        let _guard = SWEEP_TEST_LOCK.lock().await;
        let pool = skip_without_db!();
        let org_id = fresh_org(&pool).await;
        let row_id: Uuid = sqlx::query_scalar(
            "INSERT INTO voip_requirement_groups (org_id, provider, country, number_kind, action)
             VALUES ($1, 'mock', 'FR', 'mobile', 'ordering') RETURNING id",
        )
        .bind(org_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE voip_requirement_groups SET created_at = now() - interval '11 minutes'
              WHERE id = $1",
        )
        .bind(row_id)
        .execute(&pool)
        .await
        .unwrap();

        let reclaimed = reclaim_stale_groups(&pool).await.unwrap();
        assert_eq!(reclaimed, 1);

        let rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM voip_requirement_groups WHERE id = $1")
                .bind(row_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            rows, 0,
            "the stale claim must be gone, freeing the combination"
        );

        // Proof the slot is really free, not just that the row count reads zero.
        let provider = MockTelephonyProvider::default();
        let fresh = ensure_group(&pool, &provider, org_id, &fr_mobile(), &org_id.to_string())
            .await
            .expect("the reclaimed combination must be claimable again");
        assert!(!fresh.reused);
    }

    #[tokio::test]
    async fn a_recent_creating_group_claim_is_left_alone() {
        let _guard = SWEEP_TEST_LOCK.lock().await;
        let pool = skip_without_db!();
        let org_id = fresh_org(&pool).await;
        sqlx::query(
            "INSERT INTO voip_requirement_groups (org_id, provider, country, number_kind, action)
             VALUES ($1, 'mock', 'FR', 'mobile', 'ordering')",
        )
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();

        let reclaimed = reclaim_stale_groups(&pool).await.unwrap();
        assert_eq!(reclaimed, 0);

        let rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM voip_requirement_groups WHERE org_id = $1")
                .bind(org_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            rows, 1,
            "a fresh claim, not yet stale, must survive the sweep"
        );
    }

    // -----------------------------------------------------------------------------------
    // `nudge` — the webhook's entire contribution (spec 0119 "Webhook Fast-Path").
    // -----------------------------------------------------------------------------------

    /// Delete a test's own number row once its assertions are done. `nudge` deliberately
    /// sets `regulatory_next_check_at = now()` — correct production behaviour — which
    /// would otherwise leave the row looking DUE to any `reconcile_due`/`reclaim_stale_groups`
    /// test that runs afterwards in the same shared database.
    async fn forget_number(pool: &crate::db::Pool, number_id: Uuid) {
        sqlx::query("DELETE FROM voip_numbers WHERE id = $1")
            .bind(number_id)
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn nudge_moves_the_next_check_forward_for_every_matching_sub_order() {
        let _guard = SWEEP_TEST_LOCK.lock().await;
        let pool = skip_without_db!();
        let org_id = fresh_org(&pool).await;
        let sub_order = SubOrderId("suborder-nudge-1".into());
        let number_id = regulated_number(&pool, org_id, sub_order.as_str()).await;
        sqlx::query(
            "UPDATE voip_numbers SET regulatory_next_check_at = now() + interval '1 hour'
              WHERE id = $1",
        )
        .bind(number_id)
        .execute(&pool)
        .await
        .unwrap();

        let touched = nudge(&pool, "mock", &[sub_order.as_str().to_string()])
            .await
            .unwrap();
        assert_eq!(touched, 1);

        let next_check: chrono::DateTime<chrono::Utc> =
            sqlx::query_scalar("SELECT regulatory_next_check_at FROM voip_numbers WHERE id = $1")
                .bind(number_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(next_check <= chrono::Utc::now() + chrono::Duration::seconds(2));
        forget_number(&pool, number_id).await;
    }

    #[tokio::test]
    async fn nudging_the_same_sub_order_twice_is_idempotent_and_never_writes_status() {
        let _guard = SWEEP_TEST_LOCK.lock().await;
        let pool = skip_without_db!();
        let org_id = fresh_org(&pool).await;
        let sub_order = SubOrderId("suborder-nudge-2".into());
        let number_id = regulated_number(&pool, org_id, sub_order.as_str()).await;

        let ids = vec![sub_order.as_str().to_string()];
        let first = nudge(&pool, "mock", &ids).await.unwrap();
        let second = nudge(&pool, "mock", &ids).await.unwrap();
        assert_eq!(first, 1);
        assert_eq!(
            second, 1,
            "a redelivery still nudges — idempotent in EFFECT, not a no-op the second time"
        );

        let status: String = sqlx::query_scalar("SELECT status FROM voip_numbers WHERE id = $1")
            .bind(number_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            status, "pending_regulatory",
            "the nudge never writes a status"
        );
        forget_number(&pool, number_id).await;
    }

    #[tokio::test]
    async fn nudge_ignores_a_sub_order_id_that_belongs_to_no_open_number() {
        let pool = skip_without_db!();
        let touched = nudge(&pool, "mock", &["suborder-unknown".to_string()])
            .await
            .unwrap();
        assert_eq!(touched, 0);
    }

    #[tokio::test]
    async fn reconcile_one_surfaces_a_provider_failure_without_writing_anything() {
        let pool = skip_without_db!();
        let org_id = fresh_org(&pool).await;
        let provider = MockTelephonyProvider::default();
        let sub_order = SubOrderId("suborder-failing".into());
        let number_id = regulated_number(&pool, org_id, sub_order.as_str()).await;
        provider.fail_next(
            "sub_order_status",
            ProviderError::Unavailable {
                detail: "carrier outage".into(),
            },
        );

        let err = reconcile_one(
            &pool,
            &provider,
            number_id,
            org_id,
            NumberStatus::PendingRegulatory,
            &sub_order,
        )
        .await
        .expect_err("the scripted failure must surface");
        assert!(matches!(err, ReconcileError::Provider(_)), "{err:?}");
    }
}
