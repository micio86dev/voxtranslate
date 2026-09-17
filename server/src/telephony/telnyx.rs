//! Telnyx Call Control v2 adapter (spec 0111, D1).
//!
//! This file is the **only** place in the server that knows Telnyx exists. Everything it
//! returns is already in domain vocabulary; everything it accepts is too.
//!
//! ## What is EU about this
//!
//! The default REST base is `https://api.telnyx.eu`, and with it the calls, recordings and
//! related Voice API services are processed in Frankfurt. The anchorsite must be one of
//! Frankfurt, Amsterdam or London for a leg to be handled in region. Both are configurable
//! and both default to the careful value, because pointing the base at the global endpoint
//! moves telephony out of the EU without any other visible change.
//!
//! **This says nothing about where translation happens.** Our tiers reach Alibaba in
//! Singapore, OpenAI and Google in the US, and Groq in the US on every tier. The EU gate
//! (R22) must therefore look at the *tier*, not just at this adapter. See
//! `docs/voip-data-flow.md`.
//!
//! ## Structure
//!
//! Every decision — signature verification, event normalisation, cause mapping, request
//! body construction — is a **pure function** tested exhaustively below. The HTTP calls
//! are the thin remainder, and they are the part the gated live smoke suite covers,
//! because a unit test of a `reqwest` call tests `reqwest`.

use async_trait::async_trait;
use base64::Engine as _;
use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{
    CallLeg, Cdr, DialRequest, DocumentUpload, FieldValue, GatherConfig, GroupStatus, LegId,
    MediaStreamConfig, MediaTrack, NumberKind, NumberOffer, NumberSearch, NumberStatus, OrderRef,
    OrderStatus, PlayRequest, ProviderCapabilities, ProviderError, ProviderEvent,
    ProviderEventKind, ProviderMetadata, ProviderNumberId, PurchaseRequest, PurchasedNumber,
    RecordingConfig, RecordingDownloadUrl, RequirementGroup, RequirementGroupId, RequirementKind,
    RequirementQuery, RequirementSpec, RequirementsStatus, SipConnection, SubOrderId,
    SubOrderState, TelephonyProvider, UploadedDocument, VerificationStart, VerificationState,
    WebhookError, WebhookHeaders, E164,
};
use rust_decimal::Decimal;

use crate::config::TelnyxConfig;
use crate::voip::pricing::Rate;
use crate::voip::state::FailureReason;

/// Stable id, persisted in `voip_calls.provider`. Never change once shipped.
pub const TELNYX_ID: &str = "telnyx";

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// Ceiling on any single Call Control request.
///
/// Generous next to a healthy API (tens of milliseconds) and far below the 60-second sweep
/// interval, so a stuck request cannot make one tick overlap the next.
const HTTP_TIMEOUT_SECS: u64 = 15;

/// Separate and shorter: a TCP connect that has not completed in five seconds is a
/// reachability problem, and waiting the full request budget for it only delays the
/// failover the caller is going to do anyway.
const HTTP_CONNECT_TIMEOUT_SECS: u64 = 5;

pub struct TelnyxProvider {
    cfg: TelnyxConfig,
    http: reqwest::Client,
    metadata: ProviderMetadata,
    tolerance: Duration,
}

impl TelnyxProvider {
    pub fn new(cfg: TelnyxConfig, webhook_tolerance_secs: i64) -> Self {
        let metadata = ProviderMetadata {
            id: TELNYX_ID,
            display_name: "Telnyx",
            region: cfg.media_anchor.clone(),
            eu_telephony: cfg.is_eu(),
            default_caller_id: cfg.default_caller_id.clone(),
            capabilities: ProviderCapabilities {
                speech_synthesis: true,
                bidirectional_media: true,
                // Documented provider limit: one streaming operation and one bidirectional
                // RTP stream per call. The entire un-bridged two-leg design (D2) follows
                // from this number, so it is stated here rather than assumed anywhere.
                max_streams_per_leg: 1,
                recording: true,
                dtmf_gather: true,
                inbound: true,
                rate_deck: true,
            },
        };
        Self {
            cfg,
            // A timeout, because every caller of this client is on a path where hanging is
            // worse than failing. A dial sits in front of a customer's request; the
            // background sweep runs its four jobs sequentially, so one hung carrier call
            // stalls the credit-hold recovery and the consent-gate timeout behind it for
            // as long as the socket stays open. `ProviderError::Unavailable` is a result
            // the callers already know how to handle; an indefinite wait is not.
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS))
                .connect_timeout(std::time::Duration::from_secs(HTTP_CONNECT_TIMEOUT_SECS))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            metadata,
            tolerance: Duration::seconds(webhook_tolerance_secs.max(1)),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.cfg.api_base, path)
    }

    /// POST a Call Control action and translate transport/HTTP failures into
    /// [`ProviderError`].
    async fn action(&self, leg: &LegId, action: &str, body: Value) -> Result<(), ProviderError> {
        let path = format!("/v2/calls/{}/actions/{action}", leg.as_str());
        let res = self
            .http
            .post(self.url(&path))
            .bearer_auth(&self.cfg.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Unavailable {
                detail: e.to_string(),
            })?;
        classify_response(action, res).await.map(|_| ())
    }

    /// GET a v2 resource. `Ok(None)` is a 404 — "the carrier has no such thing", which for
    /// a number means it is gone, and is a fact rather than an error to retry for ever.
    async fn get_json(
        &self,
        path: &str,
        query: &[(String, String)],
    ) -> Result<Option<Value>, ProviderError> {
        let res = self
            .http
            .get(self.url(path))
            .query(query)
            .bearer_auth(&self.cfg.api_key)
            .send()
            .await
            .map_err(|e| ProviderError::Unavailable {
                detail: e.to_string(),
            })?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let res = classify_response(path, res).await?;
        res.json::<Value>()
            .await
            .map(Some)
            .map_err(|e| ProviderError::Malformed {
                detail: e.to_string(),
            })
    }

    /// POST with OUR idempotency key on the header the carrier honours.
    ///
    /// The retry this protects against is not hypothetical: a timeout after the order was
    /// accepted, retried by a client, buys a second number that the customer then pays for
    /// monthly, for ever, without ever having asked for it.
    async fn post_json_idempotent(
        &self,
        path: &str,
        body: &Value,
        key: &str,
    ) -> Result<Value, ProviderError> {
        let res = self
            .http
            .post(self.url(path))
            .bearer_auth(&self.cfg.api_key)
            .header("Idempotency-Key", key)
            .json(body)
            .send()
            .await
            .map_err(|e| ProviderError::Unavailable {
                detail: e.to_string(),
            })?;
        let res = classify_response(path, res).await?;
        res.json::<Value>()
            .await
            .map_err(|e| ProviderError::Malformed {
                detail: e.to_string(),
            })
    }

    /// DELETE where a 404 is success: a number the carrier no longer has is released, the
    /// same treatment `hangup` gives a call that has already ended.
    async fn delete_ok_if_missing(&self, path: &str) -> Result<(), ProviderError> {
        let res = self
            .http
            .delete(self.url(path))
            .bearer_auth(&self.cfg.api_key)
            .send()
            .await
            .map_err(|e| ProviderError::Unavailable {
                detail: e.to_string(),
            })?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        classify_response(path, res).await.map(|_| ())
    }

    /// Turn one submitted [`FieldValue`] into the plain string
    /// [`requirement_group_patch_body`] wants. `Text` and `Document` values are already
    /// opaque strings and need no network call; an `Address` must first become a Telnyx
    /// address id via `POST /v2/addresses` (design D13) — the only branch here that is
    /// not pure, and the reason this lives on `self` rather than being folded into the
    /// (still pure, still exhaustively unit-tested) body builder.
    async fn resolve_field_value(&self, value: &FieldValue) -> Result<String, ProviderError> {
        match value {
            FieldValue::Text(s) => Ok(s.clone()),
            FieldValue::Document(doc_id) => Ok(doc_id.clone()),
            FieldValue::Address(addr) => self.create_address(addr).await,
        }
    }

    /// `POST /v2/addresses` — verified against the published `AddressCreate`/`Address`
    /// schemas (2026-09-16, see [`address_create_body`]/[`parse_address_id`]). Uses the
    /// redacted classifier (design D12): the request body is a full mailing address, and
    /// an error response echoing it back must never reach the log.
    async fn create_address(&self, addr: &super::AddressValue) -> Result<String, ProviderError> {
        let payload = address_create_body(addr);
        let res = self
            .http
            .post(self.url("/v2/addresses"))
            .bearer_auth(&self.cfg.api_key)
            .json(&payload)
            .send()
            .await
            .map_err(|e| ProviderError::Unavailable {
                detail: e.to_string(),
            })?;
        let res = classify_response_redacted("create_address", res).await?;
        let body: Value = res.json().await.map_err(|e| ProviderError::Malformed {
            detail: e.to_string(),
        })?;
        parse_address_id(&body).ok_or_else(|| ProviderError::Malformed {
            detail: "address response has no id".into(),
        })
    }
}

/// Ceiling on what one carrier error contributes to a log line.
///
/// A carrier answers in JSON; a proxy or a WAF in front of one answers in HTML, and an
/// HTML error page is several kilobytes of markup that would otherwise go through the log
/// pipeline verbatim on every failure.
const MAX_ERROR_DETAIL: usize = 400;

/// Pick a download URL out of a `GET /v2/recordings/{id}` response's `data` object.
///
/// mp3 is preferred over wav purely for size — both are equally valid per the schema.
/// `None` covers both "no `download_urls` object at all" and "the object is there but
/// both formats are absent/empty", which is what a recording still being processed at
/// the carrier looks like: a row exists, but nothing is downloadable yet.
fn download_url_from_recording(data: &Value) -> Option<String> {
    let urls = data.get("download_urls")?;
    // The blank check runs per format, BEFORE the fallback: a present-but-empty mp3 must
    // not stop a usable wav from being handed out.
    let usable = |format: &str| {
        urls.get(format)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    usable("mp3").or_else(|| usable("wav")).map(str::to_string)
}

/// Classify a response, and on failure say what the carrier actually complained about.
///
/// The status alone is a category — "the destination was refused" — and the category is
/// never the thing an operator needs. It does not say whether the outbound voice profile
/// is missing a country, whether the account is unverified, or whether the connection is
/// detached; all three are 422, and all three are fixed in a different place. Reading the
/// body is the whole difference between a log line that ends the investigation and one
/// that starts it.
///
/// The body is consumed **only** on the error path, so every success still hands back an
/// unread response for the caller to deserialize.
async fn classify_response(
    op: &str,
    res: reqwest::Response,
) -> Result<reqwest::Response, ProviderError> {
    let status = res.status();
    let Some(err) = classify(status) else {
        return Ok(res);
    };
    // A body we cannot read is not a reason to lose the status: report what we have.
    let raw = res.text().await.unwrap_or_default();
    tracing::warn!(
        provider = TELNYX_ID,
        operation = op,
        status = status.as_u16(),
        detail = %error_detail(&raw),
        "the carrier refused a request"
    );
    Err(err)
}

/// Same success/failure classification as [`classify_response`], but the failure path
/// never reads the response body at all.
///
/// Design D12: a submitted requirement value, an uploaded document, and a resolved
/// address are all PII, and Telnyx's own error bodies are known to echo back exactly
/// what was submitted (a malformed-address 422 restating the street address, a rejected
/// value quoting it back). [`error_detail`] redacting phone numbers is not enough
/// protection for THAT shape of leak, so these three calls skip reading the body on
/// failure entirely rather than trying to redact a payload this file does not control
/// the shape of.
async fn classify_response_redacted(
    op: &str,
    res: reqwest::Response,
) -> Result<reqwest::Response, ProviderError> {
    let status = res.status();
    let Some(err) = classify(status) else {
        return Ok(res);
    };
    tracing::warn!(
        provider = TELNYX_ID,
        operation = op,
        status = status.as_u16(),
        "the carrier refused a request (body redacted: may echo submitted PII)"
    );
    Err(err)
}

/// Reduce a carrier error body to one redacted, bounded log line.
///
/// Pure, and tested exhaustively, for the same reason [`classify`] is: this runs only on
/// the failure path, where a panic would turn a recoverable carrier error into a dropped
/// call, and where nobody is watching closely enough to notice a leak.
///
/// **Telephone numbers are redacted.** A carrier echoes `to` and `from` back in its error
/// prose, and this file's whole convention is that a number never reaches a log in full —
/// [`E164`]'s own `Display` is the masked form precisely so a stray `{}` cannot leak one.
/// A carrier's sentence must not be the hole in that rule.
fn error_detail(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "<empty body>".to_string();
    }

    // The documented shape: `{"errors":[{"code":…,"title":…,"detail":…,"meta":{…}}]}`.
    // `meta` is dropped — it carries a documentation URL and an echo of the request, and
    // neither helps in a log line.
    let summary = serde_json::from_str::<Value>(trimmed)
        .ok()
        .and_then(|v| {
            let errors = v.get("errors")?.as_array()?;
            let joined: Vec<String> = errors
                .iter()
                .map(|e| {
                    ["code", "title", "detail"]
                        .iter()
                        .filter_map(|k| e.get(*k))
                        .map(value_as_text)
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join(": ")
                })
                .filter(|s| !s.is_empty())
                .collect();
            (!joined.is_empty()).then(|| joined.join(" | "))
        })
        // Not JSON, or JSON in a shape we do not know: keep it rather than report
        // nothing, which would put us back where this started.
        .unwrap_or_else(|| trimmed.to_string());

    bound(&redact_numbers(&collapse_whitespace(&summary)))
}

/// A log line is one line. Carrier prose and HTML both arrive with newlines in them.
fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Replace anything shaped like a telephone number with a marker.
///
/// Seven digits is the bar: it is below every national number we can dial and above the
/// carrier's own error codes, which are five and must survive — an error code that gets
/// redacted defeats the purpose of reading the body at all.
fn redact_numbers(s: &str) -> String {
    const MIN_NUMBER_DIGITS: usize = 7;
    let mut out = String::with_capacity(s.len());
    let mut run = String::new();

    // Kept as one pass with an explicit digit run rather than a regex: this file has no
    // regex dependency, and adding one for six lines of scanning is not a trade worth
    // making on a path that must not fail.
    let flush = |run: &mut String, out: &mut String| {
        if run.len() >= MIN_NUMBER_DIGITS {
            out.push_str("<number>");
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in s.chars() {
        if c.is_ascii_digit() {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

/// Truncate on a character boundary. A carrier answering in a non-Latin script must not
/// take the process down on the error path, which is what `String::truncate` would do.
fn bound(s: &str) -> String {
    if s.len() <= MAX_ERROR_DETAIL {
        return s.to_string();
    }
    const ELLIPSIS: &str = "…";
    let budget = MAX_ERROR_DETAIL - ELLIPSIS.len();
    let mut end = budget;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ELLIPSIS}", &s[..end])
}

/// Render a JSON scalar as the text a human would read, without the quotes
/// `Value::to_string` adds.
fn value_as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Map an HTTP status onto a provider error, or `None` when it is a success.
///
/// Kept separate and pure so the mapping is testable: getting 422 wrong (retryable vs not)
/// is the difference between a transient blip and a loop of paid dials.
fn classify(status: reqwest::StatusCode) -> Option<ProviderError> {
    if status.is_success() {
        return None;
    }
    Some(match status.as_u16() {
        401 | 403 => ProviderError::Unauthorized,
        402 => ProviderError::AccountBlocked,
        422 => ProviderError::DestinationRefused,
        429 => ProviderError::RateLimited,
        s => ProviderError::Unavailable {
            detail: format!("HTTP {s}"),
        },
    })
}

// ---------------------------------------------------------------------------
// Regulatory requirements (spec 0119)
// ---------------------------------------------------------------------------
//
// `GET /v2/regulatory_requirements` — verified against Telnyx's published OpenAPI spec
// (github.com/team-telnyx/openapi, schema `RegulatoryRequirements`) 2026-09-16. This is
// NOT `/v2/requirements` (design's original placeholder guess): that path lists every
// requirement Telnyx has ever defined, unfiltered. The `filter[...]` query keys
// (`country_code`, `phone_number_type`, `action`) are confirmed by the same schema.

/// One entry per matching (country, phone_number_type, action) combination, each nesting
/// its own `regulatory_requirements` array — the response is not a flat list at `data[]`.
fn parse_requirements(body: &Value) -> Vec<RequirementSpec> {
    body.get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("regulatory_requirements"))
        .filter_map(Value::as_array)
        .flatten()
        .map(parse_requirement_spec)
        .collect()
}

fn parse_requirement_spec(item: &Value) -> RequirementSpec {
    RequirementSpec {
        id: item
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        name: item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        description: item
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        example: item
            .get("example")
            .and_then(Value::as_str)
            .map(str::to_string),
        kind: item
            .get("field_type")
            .and_then(Value::as_str)
            .map(RequirementKind::parse)
            .unwrap_or(RequirementKind::Textual),
    }
}

/// `POST /v2/requirement_groups` body — verified against the same `RequirementGroup`
/// requestBody schema: `country_code`, `phone_number_type`, `action` and
/// `customer_reference` are the fields this crate needs at creation time. Field values are
/// not sent here; they arrive later through `submit_requirement_values` (design D8: a group
/// is created empty and filled once the customer starts answering).
fn requirement_group_create_body(query: &RequirementQuery, customer_ref: &str) -> Value {
    json!({
        "country_code": query.country.to_ascii_uppercase(),
        "phone_number_type": query.kind.as_str(),
        "action": query.action.as_str(),
        "customer_reference": customer_ref,
    })
}

/// `PATCH /v2/requirement_groups/:id` body — verified against the same schema: a
/// `regulatory_requirements` array of `{requirement_id, field_value}`, both plain strings.
///
/// Pure and infallible: every value is already a plain string by the time this runs.
/// `TelnyxProvider::resolve_field_value` is what turns a `FieldValue` into one — an
/// `Address` becomes a Telnyx address id via `POST /v2/addresses` (design D13, PR4),
/// never raw address text — so this function itself never needs to know `FieldValue`
/// exists.
fn requirement_group_patch_body(values: &[(String, String)]) -> Value {
    let requirements: Vec<Value> = values
        .iter()
        .map(|(requirement_id, field_value)| {
            json!({ "requirement_id": requirement_id, "field_value": field_value })
        })
        .collect();
    json!({ "regulatory_requirements": requirements })
}

/// `POST /v2/addresses` body — verified against the published `AddressCreate` schema
/// (2026-09-16): required fields are `first_name`, `last_name`, `business_name`,
/// `street_address`, `locality`, `country_code`; `extended_address`/`administrative_area`
/// are optional and omitted entirely (not sent as JSON `null`) when absent.
fn address_create_body(addr: &super::AddressValue) -> Value {
    let mut body = json!({
        "first_name": addr.first_name,
        "last_name": addr.last_name,
        "business_name": addr.business_name,
        "street_address": addr.street_address,
        "locality": addr.locality,
        "postal_code": addr.postal_code,
        "country_code": addr.country_code,
    });
    if let Some(extended) = &addr.extended_address {
        body["extended_address"] = json!(extended);
    }
    if let Some(area) = &addr.administrative_area {
        body["administrative_area"] = json!(area);
    }
    body
}

/// Parse `POST /v2/addresses`' response. Verified against the published `Address`
/// schema: the created address is always wrapped in `data`, with `id` as an opaque
/// string (Telnyx documents it as an int64-formatted string, not a uuid, unlike most of
/// this file's other resources — read as a plain string regardless).
fn parse_address_id(body: &Value) -> Option<String> {
    body.get("data")
        .unwrap_or(body)
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Parse `POST /v2/documents`' response. Verified against the published
/// `DocServiceDocument` schema: the created document is wrapped in `data`, with `id`
/// (uuid) and `av_scan_status` (`scanned`/`infected`/`pending_scan`/`not_scanned`).
/// `av_scan_status` is stored verbatim rather than mapped onto a crate-owned enum — see
/// [`UploadedDocument`] — so an empty string here honestly means "the field was
/// missing", never a guessed status word.
fn parse_uploaded_document(body: &Value) -> Option<UploadedDocument> {
    let data = body.get("data").unwrap_or(body);
    let id = data.get("id").and_then(Value::as_str)?.to_string();
    let av_scan_status = data
        .get("av_scan_status")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some(UploadedDocument { id, av_scan_status })
}

/// Parse a `RequirementGroup` response. `POST`/`GET`/`PATCH /v2/requirement_groups[/{id}]`
/// all return the object directly at the top level — verified against the schema, and
/// unlike almost every other Telnyx resource in this file, which wraps its payload in
/// `data`. A `data`-wrapped shape is tolerated anyway (cheap, and consistent with how
/// [`TelnyxProvider::purchase_number`] already reads `body.get("data").unwrap_or(&body)`)
/// in case a future response ever adds the envelope.
///
/// `None` means the response carries no `id` — not a value this crate can act on.
fn parse_requirement_group(body: &Value) -> Option<RequirementGroup> {
    let data = body.get("data").unwrap_or(body);
    let id = data.get("id").and_then(Value::as_str)?.to_string();
    let status = data
        .get("status")
        .and_then(Value::as_str)
        .map(GroupStatus::parse)
        .unwrap_or(GroupStatus::Unknown);
    let requirements = data
        .get("regulatory_requirements")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(parse_user_requirement)
        .collect();
    Some(RequirementGroup {
        id: RequirementGroupId(id),
        status,
        requirements,
    })
}

/// One entry of a group's `regulatory_requirements` array (`UserRequirement` schema):
/// `{requirement_id, field_value, field_type, status}`. Unlike the list endpoint, this
/// shape carries no human-readable `name`/`description`/`example` — those live only in
/// [`parse_requirements`]'s response. The caller is expected to merge the two (the id is
/// the shared key); repeating the id as the name here is a documented, tolerant fallback so
/// a spec is never blank rather than an attempt to fabricate a label Telnyx never sent.
fn parse_user_requirement(item: &Value) -> (RequirementSpec, Option<FieldValue>) {
    let requirement_id = item
        .get("requirement_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let kind = item
        .get("field_type")
        .and_then(Value::as_str)
        .map(RequirementKind::parse)
        .unwrap_or(RequirementKind::Textual);
    let spec = RequirementSpec {
        id: requirement_id.clone(),
        name: requirement_id,
        description: None,
        example: None,
        kind,
    };
    // An empty string is "not submitted yet", not a blank text answer — Telnyx returns the
    // field even before it has ever been filled in.
    let value = item
        .get("field_value")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|v| match kind {
            RequirementKind::Document => FieldValue::Document(v.to_string()),
            // A submitted address round-trips as its already-resolved provider id (an
            // opaque string), which this crate's own `FieldValue::Text` shape represents
            // just as well — reconstructing a full `AddressValue` from one id is neither
            // possible nor needed for this read path.
            RequirementKind::Address | RequirementKind::Textual => FieldValue::Text(v.to_string()),
        });
    (spec, value)
}

/// Parse `GET /v2/sub_number_orders/:id` (with `filter[include_phone_numbers]=true`) or the
/// `data` object of the attach-group response into [`SubOrderState`] — verified against the
/// `numbers_SubNumberOrder`/`SubNumberOrderRequirementGroupResponse` schemas.
///
/// `group` is always `None` here: neither endpoint echoes back which requirement group is
/// attached (verified — no such field exists on either schema). The domain layer tracks
/// that id itself (design D7's `voip_requirement_groups` table); [`TelnyxProvider`]'s own
/// `attach_requirement_group` fills it in from the id it was just given, since that call
/// alone knows for certain which group it attached.
fn parse_sub_order_state(data: &Value) -> SubOrderState {
    let order = data
        .get("status")
        .and_then(Value::as_str)
        .map(OrderStatus::parse)
        .unwrap_or(OrderStatus::Unknown);
    let requirements_met = data
        .get("requirements_met")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // Present only when the caller asked for `filter[include_phone_numbers]=true`; a
    // sub-order without it (or with no numbers on it yet) must degrade to Unknown rather
    // than panic.
    let status_str = data
        .get("phone_numbers")
        .and_then(Value::as_array)
        .and_then(|numbers| numbers.first())
        .and_then(|number| number.get("requirements_status"))
        .and_then(Value::as_str);
    SubOrderState {
        order,
        requirements: parse_requirements_status(status_str, requirements_met),
        group: None,
    }
}

/// `requirements_met: true` is the one authoritative bit Telnyx's spec documents for "the
/// regulator is satisfied" — no confirmed string value for that case exists anywhere in the
/// published schema, so the boolean is checked FIRST and wins over whatever the string
/// says. The `requirement-info-*` strings themselves are verified against the spec's own
/// response example. The exception reason text has no documented field anywhere in the
/// spec (a real, open gap, not an oversight here) — `None` is honest about it; the reason
/// shown to the customer, if any, is a later phase's problem to solve, not a fabricated
/// value now.
fn parse_requirements_status(status: Option<&str>, requirements_met: bool) -> RequirementsStatus {
    if requirements_met {
        return RequirementsStatus::Approved;
    }
    match status.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "requirement-info-exception" => RequirementsStatus::Exception { reason: None },
        "requirement-info-under-review" => RequirementsStatus::UnderReview,
        "requirement-info-pending" => RequirementsStatus::InfoPending,
        _ => RequirementsStatus::Unknown,
    }
}

/// Parse `data.id` / `data.sub_number_orders_ids[0]` out of a `POST /v2/number_orders`
/// response into an [`OrderRef`] — verified against `NumberOrderWithPhoneNumbers`.
///
/// `sub_number_orders_ids` (a top-level array on the order) is the confirmed source: the
/// embedded `PhoneNumber` schema was checked directly and carries no `sub_number_order_id`
/// field, so `phone_numbers[0].sub_number_order_id` — design's original guess — is kept
/// only as a defensive fallback that costs nothing, never the primary path.
///
/// `None` when either half is missing: a purchase this crate cannot fully identify must
/// never be represented as a fabricated, partially-guessed `OrderRef`.
fn parse_order_ref(data: &Value) -> Option<OrderRef> {
    let order_id = data.get("id").and_then(Value::as_str)?.to_string();
    let sub_order_id = data
        .get("sub_number_orders_ids")
        .and_then(Value::as_array)
        .and_then(|ids| ids.first())
        .and_then(Value::as_str)
        .or_else(|| {
            data.get("phone_numbers")
                .and_then(Value::as_array)
                .and_then(|numbers| numbers.first())
                .and_then(|number| number.get("sub_number_order_id"))
                .and_then(Value::as_str)
        })?
        .to_string();
    Some(OrderRef {
        order_id,
        sub_order_id: SubOrderId(sub_order_id),
    })
}

/// Telnyx carries `client_state` base64-encoded.
fn encode_client_state(raw: &str) -> String {
    B64.encode(raw.as_bytes())
}

fn decode_client_state(raw: &str) -> Option<String> {
    B64.decode(raw).ok().and_then(|b| String::from_utf8(b).ok())
}

/// The body for `POST /v2/calls`.
fn dial_body(cfg: &TelnyxConfig, req: &DialRequest) -> Value {
    let mut body = json!({
        "connection_id": cfg.connection_id,
        "to": req.to.as_str(),
        "from": req.from.as_str(),
        "timeout_secs": req.timeout_secs,
        "client_state": encode_client_state(&req.client_state),
        // A stable id makes the dial itself idempotent provider-side: a retried POST after
        // a network timeout must not place a second paid call.
        "command_id": Uuid::new_v4().to_string(),
    });
    if let Some(profile) = &cfg.outbound_voice_profile_id {
        body["outbound_voice_profile_id"] = json!(profile);
    }
    body
}

/// The body for the `streaming_start` action.
fn streaming_body(cfg: &MediaStreamConfig) -> Value {
    let mut body = json!({
        "stream_url": cfg.url,
        "stream_track": match cfg.track {
            MediaTrack::Inbound => "inbound_track",
            MediaTrack::Outbound => "outbound_track",
            MediaTrack::Both => "both_tracks",
        },
        "stream_codec": cfg.codec.as_str(),
        "command_id": Uuid::new_v4().to_string(),
    });
    if cfg.bidirectional {
        // RTP rather than mp3: mp3 accepts at most one submission per second, which is a
        // whole second of added latency on a conversation.
        body["stream_bidirectional_mode"] = json!("rtp");
        body["stream_bidirectional_codec"] = json!(cfg.codec.as_str());
        // Telnyx defaults this to 8000 when it is omitted. We negotiate L16 at 16 kHz, so
        // leaving it unset would silently downsample the bidirectional stream to 8 kHz on
        // the wire while every leg on our side keeps decoding it at the codec's own rate.
        body["stream_bidirectional_sampling_rate"] = json!(cfg.codec.sample_rate());
    }
    body
}

/// Carrier hangup causes → domain reasons.
///
/// Anything unrecognised becomes [`FailureReason::Unmapped`], deliberately **not**
/// `ProviderUnavailable`: that reason already means "the provider broke", and laundering
/// an unknown carrier cause into it hides the gap instead of surfacing it in analytics.
///
/// `normal_clearing` and `originator_cancel` are also `Unmapped` on purpose — they say
/// only that the call ended normally, and whether that is a completed call or an abandoned
/// one is decided by the state machine from whether anyone answered.
fn map_hangup_cause(cause: &str) -> FailureReason {
    match cause {
        "user_busy" | "busy" => FailureReason::Busy,
        "call_rejected" | "rejected" => FailureReason::Rejected,
        "timeout" | "no_answer" | "no_user_response" => FailureReason::NoAnswer,
        "invalid_bnumber" | "unallocated_number" | "no_route_destination" => {
            FailureReason::Unallocated
        }
        _ => FailureReason::Unmapped,
    }
}

// ---------------------------------------------------------------------------
// Webhook
// ---------------------------------------------------------------------------

/// Verify an Ed25519 webhook signature.
///
/// The signed message is `{timestamp}|{raw body}`. The **timestamp is checked first**: a
/// replayed webhook carries a genuinely valid signature, so verifying the signature alone
/// would accept it happily.
fn verify_signature(
    public_key_b64: &str,
    headers: &WebhookHeaders,
    body: &[u8],
    now: DateTime<Utc>,
    tolerance: Duration,
) -> Result<(), WebhookError> {
    let (Some(sig_b64), Some(ts)) = (headers.signature.as_deref(), headers.timestamp.as_deref())
    else {
        return Err(WebhookError::MissingSignature);
    };

    let secs: i64 = ts.trim().parse().map_err(|_| WebhookError::BadSignature)?;
    let sent = DateTime::from_timestamp(secs, 0).ok_or(WebhookError::BadSignature)?;
    // Absolute difference: a webhook from the future is a clock problem we cannot
    // distinguish from an attack, and accepting it widens the replay window in the one
    // direction nobody thinks to check.
    if (now - sent).abs() > tolerance {
        return Err(WebhookError::StaleTimestamp);
    }

    // An unset public key must reject everything rather than accept everything. This is
    // the fail-closed branch that matters most: a deployment that forgot TELNYX_PUBLIC_KEY
    // would otherwise take call-control instructions from anyone on the internet.
    let key_bytes = B64
        .decode(public_key_b64.trim())
        .map_err(|_| WebhookError::BadSignature)?;
    let key_array: [u8; 32] = key_bytes
        .as_slice()
        .try_into()
        .map_err(|_| WebhookError::BadSignature)?;
    let key = VerifyingKey::from_bytes(&key_array).map_err(|_| WebhookError::BadSignature)?;

    let sig_bytes = B64
        .decode(sig_b64.trim())
        .map_err(|_| WebhookError::BadSignature)?;
    let sig_array: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| WebhookError::BadSignature)?;
    let signature = Signature::from_bytes(&sig_array);

    let mut message = Vec::with_capacity(ts.len() + 1 + body.len());
    message.extend_from_slice(ts.trim().as_bytes());
    message.push(b'|');
    message.extend_from_slice(body);

    key.verify_strict(&message, &signature)
        .map_err(|_| WebhookError::BadSignature)
}

/// The subset of a Telnyx webhook we read. Unknown fields are ignored by design: the
/// provider adds fields without notice, and refusing an event because it grew a key would
/// take the whole call lifecycle down.
#[derive(Debug, Deserialize)]
struct TelnyxEnvelope {
    data: TelnyxData,
}

#[derive(Debug, Deserialize)]
struct TelnyxData {
    id: String,
    event_type: String,
    occurred_at: DateTime<Utc>,
    payload: TelnyxPayload,
}

#[derive(Debug, Deserialize)]
struct TelnyxPayload {
    call_control_id: Option<String>,
    #[serde(default)]
    client_state: Option<String>,
    #[serde(default)]
    hangup_cause: Option<String>,
    #[serde(default)]
    digit: Option<String>,
    #[serde(default)]
    digits: Option<String>,
    #[serde(default)]
    recording_urls: Option<Value>,
    #[serde(default)]
    public_recording_urls: Option<Value>,
    #[serde(default)]
    duration_millis: Option<u64>,
    #[serde(default)]
    recording_id: Option<String>,
    /// "incoming" or "outgoing" (spec 0116). Telnyx reports both directions as
    /// `call.initiated` and distinguishes them only here.
    #[serde(default)]
    direction: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
}

/// First playable URL out of Telnyx's recording URL maps (`{mp3, wav}`).
fn first_recording_url(payload: &TelnyxPayload) -> String {
    for source in [&payload.recording_urls, &payload.public_recording_urls] {
        if let Some(Value::Object(map)) = source {
            for key in ["mp3", "wav"] {
                if let Some(Value::String(u)) = map.get(key) {
                    if !u.is_empty() {
                        return u.clone();
                    }
                }
            }
        }
    }
    String::new()
}

/// Everything from a `number_order.complete` payload [`parse_number_order_event`] needs —
/// parsed from the raw JSON rather than [`TelnyxPayload`], whose fields describe a call
/// leg and never carry an order's id or its sub-orders.
///
/// Mirrors [`parse_order_ref`]'s own verified fields exactly: `id` is confirmed against
/// `NumberOrderWithPhoneNumbers`, and `sub_number_orders_ids` (a top-level array on the
/// order) is the confirmed source of every sub-order id, with
/// `phone_numbers[].sub_number_order_id` kept only as a defensive fallback.
fn parse_number_order_event(event_type: &str, payload: &Value) -> ProviderEventKind {
    if event_type != "number_order.complete" {
        // Recorded, not rejected — the same honesty `Unhandled` already gives every event
        // type this crate does not fully model. The webhook is only ever a NUDGE (design
        // D5); the sweep is the source of truth regardless of what this branch decides.
        return ProviderEventKind::Unhandled {
            raw_type: event_type.to_string(),
        };
    }
    let order_id = payload
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let sub_order_ids: Vec<String> = payload
        .get("sub_number_orders_ids")
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|ids: &Vec<String>| !ids.is_empty())
        .or_else(|| {
            payload
                .get("phone_numbers")
                .and_then(Value::as_array)
                .map(|numbers| {
                    numbers
                        .iter()
                        .filter_map(|n| n.get("sub_number_order_id").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .filter(|ids: &Vec<String>| !ids.is_empty())
        })
        .unwrap_or_default();

    match order_id {
        Some(order_id) if !sub_order_ids.is_empty() => ProviderEventKind::NumberOrderCompleted {
            order_id,
            sub_order_ids,
        },
        // An order this crate cannot fully identify must never be represented as a
        // fabricated, partially-guessed nudge target — recorded as unhandled instead, the
        // same honest gap `parse_order_ref` leaves for its own `None` case.
        _ => ProviderEventKind::Unhandled {
            raw_type: event_type.to_string(),
        },
    }
}

/// Normalise a verified body into a domain event.
fn normalise(body: &[u8]) -> Result<ProviderEvent, WebhookError> {
    let root: Value = serde_json::from_slice(body).map_err(|e| WebhookError::Malformed {
        detail: e.to_string(),
    })?;
    let env: TelnyxEnvelope =
        serde_json::from_value(root.clone()).map_err(|e| WebhookError::Malformed {
            detail: e.to_string(),
        })?;

    // `number_order.*` events describe an ORDER, not a call leg: Telnyx's own schema for
    // them carries no `call_control_id` at all, so they must be recognised and handled
    // BEFORE the call_control_id requirement below — which used to reject every one of
    // them as Malformed (spec 0119 Phase 7, design D5).
    if env.data.event_type.starts_with("number_order.") {
        let payload = root
            .pointer("/data/payload")
            .cloned()
            .unwrap_or(Value::Null);
        let kind = parse_number_order_event(&env.data.event_type, &payload);
        return Ok(ProviderEvent {
            provider: TELNYX_ID,
            event_id: env.data.id,
            // No call leg exists for an order event. `apply()` resolves nothing for an
            // empty leg id and writes nothing — the honest no-op design D5 requires; the
            // caller (`inbound_webhook`) reads `NumberOrderCompleted` directly instead.
            leg_id: LegId::new(""),
            client_state: None,
            occurred_at: env.data.occurred_at,
            kind,
        });
    }

    let leg = env
        .data
        .payload
        .call_control_id
        .clone()
        .ok_or_else(|| WebhookError::Malformed {
            detail: "event has no call_control_id".into(),
        })?;

    let kind = match env.data.event_type.as_str() {
        // Telnyx reports both directions as `call.initiated` and distinguishes them with
        // `direction`. "incoming" is somebody ringing one of our numbers; anything else is
        // the dial we asked for.
        "call.initiated" => {
            if env.data.payload.direction.as_deref() == Some("incoming") {
                ProviderEventKind::Incoming {
                    from: env.data.payload.from.clone().unwrap_or_default(),
                    to: env.data.payload.to.clone().unwrap_or_default(),
                }
            } else {
                ProviderEventKind::Initiated
            }
        }
        // Telnyx does not emit a distinct "ringing"; early media / ringing is reported as
        // `call.initiated` on the outbound leg. Modelling one anyway would invent a state.
        "call.answered" => ProviderEventKind::Answered,
        "call.hangup" => ProviderEventKind::Hangup {
            cause: map_hangup_cause(
                env.data
                    .payload
                    .hangup_cause
                    .as_deref()
                    .unwrap_or("unspecified"),
            ),
        },
        "streaming.started" => ProviderEventKind::MediaStarted,
        "streaming.stopped" | "streaming.failed" => ProviderEventKind::MediaStopped,
        "call.recording.saved" => ProviderEventKind::RecordingSaved {
            url: first_recording_url(&env.data.payload),
            recording_id: env
                .data
                .payload
                .recording_id
                .clone()
                .filter(|id| !id.is_empty()),
            duration_secs: env
                .data
                .payload
                .duration_millis
                .map(|ms| ms / 1000)
                .unwrap_or_default(),
        },
        "call.dtmf.received" => {
            let raw = env
                .data
                .payload
                .digit
                .as_deref()
                .or(env.data.payload.digits.as_deref())
                .unwrap_or("");
            match raw.chars().next() {
                Some(d) => ProviderEventKind::Dtmf { digit: d },
                None => {
                    return Err(WebhookError::Malformed {
                        detail: "dtmf event without a digit".into(),
                    })
                }
            }
        }
        other => ProviderEventKind::Unhandled {
            raw_type: other.to_string(),
        },
    };

    Ok(ProviderEvent {
        provider: TELNYX_ID,
        event_id: env.data.id,
        leg_id: LegId::new(leg),
        client_state: env
            .data
            .payload
            .client_state
            .as_deref()
            .and_then(decode_client_state),
        occurred_at: env.data.occurred_at,
        kind,
    })
}

// ---------------------------------------------------------------------------
// Rate deck
// ---------------------------------------------------------------------------
//
// There is no parser here, deliberately. `fetch_rate_deck` reports `Unsupported` because
// the endpoint it was written against returns 404 on both the EU and the global base, and
// the one public pricing endpoint that does answer is a product catalogue with no prefixes
// and no per-minute prices — all verified against a live EU account.
//
// The parser that used to live here read a response shape nobody has ever seen, and its
// tests asserted against an invented fixture. That is worse than nothing: it reads as a
// verified capability. The deck is downloaded from the Outbound Voice Profile and imported
// by `src/bin/voip-rates.rs`, which is tested against the shapes real exports actually
// take. When Telnyx publishes a real endpoint, `the_rate_deck_is_still_not_available_over_the_api`
// starts failing and says so.

#[async_trait]
impl TelephonyProvider for TelnyxProvider {
    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    async fn dial(&self, req: DialRequest) -> Result<CallLeg, ProviderError> {
        let res = self
            .http
            .post(self.url("/v2/calls"))
            .bearer_auth(&self.cfg.api_key)
            .json(&dial_body(&self.cfg, &req))
            .send()
            .await
            .map_err(|e| ProviderError::Unavailable {
                detail: e.to_string(),
            })?;

        let res = classify_response("dial", res).await?;

        let body: Value = res.json().await.map_err(|e| ProviderError::Malformed {
            detail: e.to_string(),
        })?;
        let id = body
            .pointer("/data/call_control_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ProviderError::Malformed {
                detail: "dial response has no call_control_id".into(),
            })?;

        Ok(CallLeg {
            id: LegId::new(id),
            session_id: body
                .pointer("/data/call_session_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            created_at: Utc::now(),
        })
    }

    async fn answer(&self, leg: &LegId) -> Result<(), ProviderError> {
        self.action(leg, "answer", json!({})).await
    }

    async fn hangup(&self, leg: &LegId) -> Result<(), ProviderError> {
        match self.action(leg, "hangup", json!({})).await {
            // Hanging up a leg the far party already dropped is not a fault. Teardown
            // always races the far side, and surfacing that race as an error would make
            // every clean hangup look like a problem in the logs.
            Err(ProviderError::DestinationRefused) => Ok(()),
            other => other,
        }
    }

    async fn start_media_stream(
        &self,
        leg: &LegId,
        cfg: MediaStreamConfig,
    ) -> Result<(), ProviderError> {
        self.action(leg, "streaming_start", streaming_body(&cfg))
            .await
    }

    async fn stop_media_stream(&self, leg: &LegId) -> Result<(), ProviderError> {
        self.action(leg, "streaming_stop", json!({})).await
    }

    async fn play(&self, leg: &LegId, req: PlayRequest) -> Result<(), ProviderError> {
        let body = match req {
            PlayRequest::Url { url } => json!({ "audio_url": url }),
            PlayRequest::Audio { payload_b64 } => json!({ "playback_content": payload_b64 }),
            // A different Call Control command, not a different body: `speak` synthesises
            // on the carrier side, so nothing about it goes through `playback_start`.
            PlayRequest::Speak {
                text,
                language,
                voice,
            } => {
                return self
                    .action(
                        leg,
                        "speak",
                        json!({
                            "payload": text,
                            "payload_type": "text",
                            "language": speak_language(&language),
                            "voice": voice.unwrap_or_else(|| "female".to_string()),
                        }),
                    )
                    .await
            }
        };
        self.action(leg, "playback_start", body).await
    }

    async fn gather(&self, leg: &LegId, cfg: GatherConfig) -> Result<(), ProviderError> {
        self.action(
            leg,
            "gather",
            json!({
                "valid_digits": cfg.valid_digits,
                "maximum_digits": cfg.max_digits,
                "timeout_millis": cfg.timeout_secs.saturating_mul(1000),
            }),
        )
        .await
    }

    async fn start_recording(
        &self,
        leg: &LegId,
        cfg: RecordingConfig,
    ) -> Result<(), ProviderError> {
        self.action(
            leg,
            "record_start",
            json!({
                "format": "mp3",
                "channels": if cfg.dual_channel { "dual" } else { "single" },
                "play_beep": cfg.beep,
            }),
        )
        .await
    }

    async fn delete_recording(&self, recording_id: &str) -> Result<(), ProviderError> {
        // Not an `action`: a recording outlives its call, so it is addressed as a resource
        // in its own right rather than as a command on a leg.
        let res = self
            .http
            .delete(self.url(&format!("/v2/recordings/{recording_id}")))
            .bearer_auth(&self.cfg.api_key)
            .send()
            .await
            .map_err(|e| ProviderError::Unavailable {
                detail: e.to_string(),
            })?;
        // 404 is success for a delete: the bytes are not there, which is the outcome
        // asked for. Treating it as failure would make erasure un-retryable after the
        // first partial run.
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        classify_response("delete_recording", res).await.map(|_| ())
    }

    async fn stop_recording(&self, leg: &LegId) -> Result<(), ProviderError> {
        self.action(leg, "record_stop", json!({})).await
    }

    /// `GET /v2/recordings/{recording_id}` → `data.download_urls.{mp3,wav}` — verified
    /// against developers.telnyx.com/api-reference/call-recordings/retrieve-a-call-recording
    /// (2026-09-16): both fields are optional strings, mp3 preferred here for size. The
    /// endpoint does not return an expiry for these links; `docs/voip-telnyx-setup.md`
    /// already documents Telnyx's presigned recording URLs as valid for roughly 10
    /// minutes, so that is the (deliberately conservative) estimate handed to the caller —
    /// better to ask again a minute early than to advertise a URL as live past the point
    /// Telnyx actually revokes it.
    async fn recording_download_url(
        &self,
        recording_id: &str,
    ) -> Result<Option<RecordingDownloadUrl>, ProviderError> {
        let Some(body) = self
            .get_json(&format!("/v2/recordings/{recording_id}"), &[])
            .await?
        else {
            // 404: gone at the carrier — already purged by retention, or never existed.
            return Ok(None);
        };
        let Some(data) = body.get("data") else {
            return Err(ProviderError::Malformed {
                detail: "recording response missing \"data\"".to_string(),
            });
        };
        let Some(url) = download_url_from_recording(data) else {
            // The recording row exists but carries no link yet (still processing at the
            // carrier) — same "nothing to hand out" outcome as a 404.
            return Ok(None);
        };
        Ok(Some(RecordingDownloadUrl {
            url,
            expires_at: Utc::now() + Duration::minutes(10),
        }))
    }

    /// Per-leg cost is **not** available from this provider synchronously.
    ///
    /// Telnyx rates calls asynchronously and exposes the result through batched usage
    /// reports, not on the hangup webhook. Rather than invent a parser for an endpoint
    /// whose shape has not been confirmed against a live account, this reports the
    /// capability as absent. The reconcile job then leaves
    /// `voip_calls.actual_provider_cost_usd` NULL and the call shows as **unreconciled**
    /// in the margin report — a visible gap, which is the whole point. Wiring the usage
    /// report is a credential-dependent step, tracked in `docs/voip-telnyx-setup.md`.
    async fn fetch_cdr(&self, _leg: &LegId) -> Result<Option<Cdr>, ProviderError> {
        Err(ProviderError::Unsupported {
            operation: "fetch a per-leg CDR synchronously",
        })
    }

    /// **Not available from the API.** Verified against a live EU account:
    ///
    /// | Endpoint | Result |
    /// |---|---|
    /// | `api.telnyx.eu/v2/public/pricing?primitive=voice` | 404 |
    /// | `api.telnyx.com/v2/public/pricing?primitive=voice` | 404 |
    /// | `api.telnyx.com/v2/pricing/products` | 200, but a product catalogue — no prefixes, no per-minute prices |
    ///
    /// What Telnyx offers is a rate deck **downloaded** from the Outbound Voice Profile.
    /// `src/bin/voip-rates.rs` imports it into `voip_rates`.
    ///
    /// Reported as unsupported rather than left pointing at an endpoint that does not
    /// exist, for the same reason [`Self::fetch_cdr`] is: a sync that always fails makes
    /// every call refuse with `rate_unavailable` for a reason no log explains. This way
    /// the failure names itself, and the day Telnyx publishes a real endpoint the live
    /// test starts failing and says so.
    async fn fetch_rate_deck(&self) -> Result<Vec<Rate>, ProviderError> {
        Err(ProviderError::Unsupported {
            operation: "fetch a rate deck over the API — import the downloaded deck with \
                        `cargo run --bin voip-rates`",
        })
    }

    // ---- number management (spec 0115) ------------------------------------

    async fn search_numbers(&self, q: NumberSearch) -> Result<Vec<NumberOffer>, ProviderError> {
        // `GET /v2/available_phone_numbers`. The filter names are Telnyx's and stop here.
        let mut query: Vec<(String, String)> = vec![
            (
                "filter[country_code]".into(),
                q.country.to_ascii_uppercase(),
            ),
            ("filter[limit]".into(), q.limit.clamp(1, 10).to_string()),
            // Without this the catalogue happily offers numbers that cannot carry a call.
            ("filter[features][]".into(), "voice".into()),
        ];
        if let Some(area) = &q.area_code {
            query.push(("filter[national_destination_code]".into(), area.clone()));
        }
        if let Some(kind) = q.kind {
            query.push((
                "filter[phone_number_type]".into(),
                match kind {
                    NumberKind::Local => "local",
                    NumberKind::National => "national",
                    NumberKind::TollFree => "toll_free",
                    NumberKind::Mobile => "mobile",
                }
                .into(),
            ));
        }

        let body = self
            .get_json("/v2/available_phone_numbers", &query)
            .await?
            .unwrap_or_default();
        let items = body
            .get("data")
            .and_then(|d| d.as_array())
            .cloned()
            .unwrap_or_default();

        Ok(items
            .iter()
            .filter_map(|item| {
                let e164 = item.get("phone_number")?.as_str()?.to_string();
                let cost = item.get("cost_information");
                let currency = cost
                    .and_then(|c| c.get("currency"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("USD")
                    .to_string();
                // Telnyx reports these as decimal STRINGS. Parsing to Decimal rather than
                // f64 keeps the money path free of binary floating point, which is the
                // rule the whole pricing module is built on.
                let monthly = cost
                    .and_then(|c| c.get("monthly_cost"))
                    .and_then(|c| c.as_str())
                    .and_then(|c| Decimal::from_str_exact(c).ok())
                    .unwrap_or_default();
                let setup = cost
                    .and_then(|c| c.get("upfront_cost"))
                    .and_then(|c| c.as_str())
                    .and_then(|c| Decimal::from_str_exact(c).ok())
                    .unwrap_or_default();
                let requirement = item
                    .get("region_information")
                    .and_then(|r| r.as_array())
                    .and_then(|r| {
                        r.iter()
                            .find(|x| {
                                x.get("region_type").and_then(|t| t.as_str()) == Some("location")
                            })
                            .and_then(|x| x.get("region_name"))
                            .and_then(|n| n.as_str())
                    })
                    .filter(|_| {
                        item.get("record_type")
                            .and_then(|t| t.as_str())
                            .is_some_and(|t| t.contains("regulatory"))
                    })
                    .map(|name| format!("Local documentation is required for {name}."));

                Some(NumberOffer {
                    country: item
                        .get("country_code")
                        .and_then(|c| c.as_str())
                        .unwrap_or(&q.country)
                        .to_string(),
                    kind: item
                        .get("phone_number_type")
                        .and_then(|t| t.as_str())
                        .map(NumberKind::parse)
                        .unwrap_or(NumberKind::Local),
                    e164,
                    monthly_cost: monthly,
                    setup_cost: setup,
                    currency,
                    regulatory_requirement: requirement,
                })
            })
            .collect())
    }

    async fn purchase_number(
        &self,
        req: PurchaseRequest,
    ) -> Result<PurchasedNumber, ProviderError> {
        // `POST /v2/number_orders`, with OUR idempotency key on the header the carrier
        // honours. A retry after a timeout must not buy a second number — the customer
        // would be charged monthly, for ever, for one they never asked for.
        let payload = serde_json::json!({
            "phone_numbers": [{ "phone_number": req.e164 }],
            "connection_id": self.cfg.connection_id,
        });
        let body = self
            .post_json_idempotent("/v2/number_orders", &payload, &req.idempotency_key)
            .await?;
        let data = body.get("data").unwrap_or(&body);

        let entry = data
            .get("phone_numbers")
            .and_then(|p| p.as_array())
            .and_then(|p| p.first());

        Ok(PurchasedNumber {
            provider_number_id: ProviderNumberId(
                entry
                    .and_then(|e| e.get("id"))
                    .and_then(|i| i.as_str())
                    .or_else(|| data.get("id").and_then(|i| i.as_str()))
                    .unwrap_or_default()
                    .to_string(),
            ),
            e164: entry
                .and_then(|e| e.get("phone_number"))
                .and_then(|p| p.as_str())
                .unwrap_or(&req.e164)
                .to_string(),
            status: data
                .get("status")
                .and_then(|s| s.as_str())
                .map(NumberStatus::parse)
                .unwrap_or(NumberStatus::Ordering),
            monthly_cost: Decimal::ZERO,
            setup_cost: Decimal::ZERO,
            currency: "USD".into(),
            regulatory_requirement: data
                .get("requirements_met")
                .and_then(|m| m.as_bool())
                .and_then(|met| {
                    (!met).then(|| {
                        "Regulatory documents are required before this number can carry calls."
                            .to_string()
                    })
                }),
            order: parse_order_ref(data),
        })
    }

    async fn release_number(&self, id: &ProviderNumberId) -> Result<(), ProviderError> {
        // A number the carrier no longer has is already released; 404 is success, the same
        // treatment `hangup` gives a call that has already ended.
        self.delete_ok_if_missing(&format!("/v2/phone_numbers/{}", id.as_str()))
            .await
    }

    async fn number_status(&self, id: &ProviderNumberId) -> Result<NumberStatus, ProviderError> {
        // Gone from the carrier means gone, and the reconcile sweep needs to hear that
        // rather than an error it will retry for ever.
        let Some(body) = self
            .get_json(&format!("/v2/phone_numbers/{}", id.as_str()), &[])
            .await?
        else {
            return Ok(NumberStatus::Released);
        };
        Ok(body
            .get("data")
            .and_then(|d| d.get("status"))
            .and_then(|s| s.as_str())
            .map(NumberStatus::parse)
            .unwrap_or(NumberStatus::Ordering))
    }

    async fn start_caller_id_verification(
        &self,
        _e164: &E164,
    ) -> Result<VerificationStart, ProviderError> {
        // Telnyx verifies ownership of an external number through its portal and its
        // regulatory flow, not through a REST call this account can drive. Reporting that
        // honestly is the same treatment `fetch_rate_deck` gets: an operation we cannot
        // perform must not return a fabricated success, because the thing it would be
        // claiming — that a customer owns this number — is the thing that keeps
        // caller-id spoofing illegal rather than merely discouraged.
        Err(ProviderError::Unsupported {
            operation: "start caller-id verification over the API — verify the number in \
                        the Telnyx portal and record the outcome",
        })
    }

    async fn check_caller_id_verification(
        &self,
        _id: &str,
    ) -> Result<VerificationState, ProviderError> {
        Err(ProviderError::Unsupported {
            operation: "check caller-id verification over the API",
        })
    }

    // ---- regulatory requirements (spec 0119) -------------------------------
    //
    // Response shapes verified against Telnyx's published OpenAPI spec
    // (github.com/team-telnyx/openapi, `openapi/spec3.json`, fetched 2026-09-16) — the
    // interactive docs site serves a JS app shell to a plain HTTP fetch and was not usable
    // as a source. The remaining methods below stay `Unsupported`, exactly like the SIP
    // methods further down: filled in by the rest of this PR's slices.

    async fn list_requirements(
        &self,
        query: &RequirementQuery,
    ) -> Result<Vec<RequirementSpec>, ProviderError> {
        // GET /v2/regulatory_requirements?filter[country_code]&filter[phone_number_type]
        // &filter[action] — NOT /v2/requirements (design's original placeholder guess),
        // which lists every requirement Telnyx has ever defined, unfiltered.
        let q = vec![
            (
                "filter[country_code]".into(),
                query.country.to_ascii_uppercase(),
            ),
            (
                "filter[phone_number_type]".into(),
                query.kind.as_str().to_string(),
            ),
            ("filter[action]".into(), query.action.as_str().to_string()),
        ];
        let body = self
            .get_json("/v2/regulatory_requirements", &q)
            .await?
            .unwrap_or_default();
        Ok(parse_requirements(&body))
    }

    async fn create_requirement_group(
        &self,
        query: &RequirementQuery,
        customer_ref: &str,
    ) -> Result<RequirementGroup, ProviderError> {
        let payload = requirement_group_create_body(query, customer_ref);
        let res = self
            .http
            .post(self.url("/v2/requirement_groups"))
            .bearer_auth(&self.cfg.api_key)
            .json(&payload)
            .send()
            .await
            .map_err(|e| ProviderError::Unavailable {
                detail: e.to_string(),
            })?;
        let res = classify_response("create_requirement_group", res).await?;
        let body: Value = res.json().await.map_err(|e| ProviderError::Malformed {
            detail: e.to_string(),
        })?;
        parse_requirement_group(&body).ok_or_else(|| ProviderError::Malformed {
            detail: "requirement group response has no id".into(),
        })
    }

    async fn get_requirement_group(
        &self,
        id: &RequirementGroupId,
    ) -> Result<Option<RequirementGroup>, ProviderError> {
        let Some(body) = self
            .get_json(&format!("/v2/requirement_groups/{}", id.0), &[])
            .await?
        else {
            return Ok(None);
        };
        Ok(parse_requirement_group(&body))
    }

    async fn submit_requirement_values(
        &self,
        id: &RequirementGroupId,
        values: &[(String, FieldValue)],
    ) -> Result<RequirementGroup, ProviderError> {
        // Resolve every value to a plain string BEFORE building the request body — an
        // `Address` needs its own round trip to `/v2/addresses` first (design D13).
        let mut resolved = Vec::with_capacity(values.len());
        for (requirement_id, value) in values {
            let field_value = self.resolve_field_value(value).await?;
            resolved.push((requirement_id.clone(), field_value));
        }
        let payload = requirement_group_patch_body(&resolved);
        let res = self
            .http
            .patch(self.url(&format!("/v2/requirement_groups/{}", id.0)))
            .bearer_auth(&self.cfg.api_key)
            .json(&payload)
            .send()
            .await
            .map_err(|e| ProviderError::Unavailable {
                detail: e.to_string(),
            })?;
        // Redacted (design D12): the submitted values themselves may be PII (a full
        // name, a resolved address's own fields never leave this call, but the id could
        // still be echoed alongside a rejected sibling value).
        let res = classify_response_redacted("submit_requirement_values", res).await?;
        let body: Value = res.json().await.map_err(|e| ProviderError::Malformed {
            detail: e.to_string(),
        })?;
        parse_requirement_group(&body).ok_or_else(|| ProviderError::Malformed {
            detail: "requirement group response has no id".into(),
        })
    }

    async fn upload_document(
        &self,
        upload: DocumentUpload,
    ) -> Result<UploadedDocument, ProviderError> {
        // POST /v2/documents, multipart field `file` — verified against the published
        // `CreateMultiPartDocServiceDocumentRequest`/`DocServiceDocument` schemas
        // (2026-09-16). `upload.body` streams straight into the multipart part: nothing
        // here reads it into a `Vec<u8>` first, which is the entire point of design D9 —
        // the caller already built a `'static` stream so it could outlive the
        // non-`'static` multipart field it was read from.
        let body = reqwest::Body::wrap_stream(upload.body);
        let part = reqwest::multipart::Part::stream(body)
            .mime_str(upload.content_type)
            .map_err(|e| ProviderError::Malformed {
                detail: e.to_string(),
            })?;
        let form = reqwest::multipart::Form::new().part("file", part);
        let res = self
            .http
            .post(self.url("/v2/documents"))
            .bearer_auth(&self.cfg.api_key)
            .multipart(form)
            .send()
            .await
            .map_err(|e| ProviderError::Unavailable {
                detail: e.to_string(),
            })?;
        // Redacted (design D12): an error here can echo the filename or the document's
        // own scan verdict text back, and nothing about this document is ever logged.
        let res = classify_response_redacted("upload_document", res).await?;
        let body: Value = res.json().await.map_err(|e| ProviderError::Malformed {
            detail: e.to_string(),
        })?;
        parse_uploaded_document(&body).ok_or_else(|| ProviderError::Malformed {
            detail: "document response has no id".into(),
        })
    }

    async fn sub_order_status(
        &self,
        id: &SubOrderId,
    ) -> Result<Option<SubOrderState>, ProviderError> {
        // `phone_numbers` (needed for the per-number `requirements_status`) is present only
        // when this filter is set — verified against `numbers_SubNumberOrder`'s own field
        // description.
        let q = [(
            "filter[include_phone_numbers]".to_string(),
            "true".to_string(),
        )];
        let Some(body) = self
            .get_json(&format!("/v2/sub_number_orders/{}", id.as_str()), &q)
            .await?
        else {
            // Gone at the carrier. The caller (the sweep, design D5) decides what a missing
            // sub-order means for the number's lifecycle; this method only reports the fact.
            return Ok(None);
        };
        let data = body.get("data").unwrap_or(&body);
        Ok(Some(parse_sub_order_state(data)))
    }

    async fn attach_requirement_group(
        &self,
        sub_order: &SubOrderId,
        group: &RequirementGroupId,
    ) -> Result<SubOrderState, ProviderError> {
        // POST /v2/sub_number_orders/:id/requirement_group {requirement_group_id} —
        // verified against the published spec ("Update requirement group for a sub number
        // order"). NOT a PATCH, and the body key is `requirement_group_id`, not the
        // `group_id` design's original placeholder left unverified.
        let payload = json!({ "requirement_group_id": group.0 });
        let res = self
            .http
            .post(self.url(&format!(
                "/v2/sub_number_orders/{}/requirement_group",
                sub_order.as_str()
            )))
            .bearer_auth(&self.cfg.api_key)
            .json(&payload)
            .send()
            .await
            .map_err(|e| ProviderError::Unavailable {
                detail: e.to_string(),
            })?;
        let res = classify_response("attach_requirement_group", res).await?;
        let body: Value = res.json().await.map_err(|e| ProviderError::Malformed {
            detail: e.to_string(),
        })?;
        let data = body.get("data").unwrap_or(&body);
        let mut state = parse_sub_order_state(data);
        // Neither this response nor a plain GET echoes back which group is attached
        // (verified — no such field on either schema). This call alone knows for certain,
        // since it just told the carrier to attach it.
        state.group = Some(group.clone());
        Ok(state)
    }

    // ---- SIP / PBX (spec 0118) ---------------------------------------------

    async fn create_sip_connection(&self, _name: &str) -> Result<SipConnection, ProviderError> {
        Err(ProviderError::Unsupported {
            operation: "create a SIP connection — creating one needs a live account, credentials and a test call in each direction",
        })
    }

    async fn list_sip_connections(&self) -> Result<Vec<SipConnection>, ProviderError> {
        Err(ProviderError::Unsupported {
            operation: "list SIP connections",
        })
    }

    fn verify_webhook(
        &self,
        headers: &WebhookHeaders,
        body: &[u8],
        now: DateTime<Utc>,
    ) -> Result<ProviderEvent, WebhookError> {
        verify_signature(&self.cfg.public_key_b64, headers, body, now, self.tolerance)?;
        normalise(body)
    }
}

/// Map our language code to the provider's `speak` locale.
///
/// Telnyx wants a BCP-47 tag with a region (`it-IT`), while the rest of the product speaks
/// bare ISO-639-1 (`it`). Anything already regioned is passed through, and anything with no
/// known region falls back to English — which is exactly what
/// [`crate::voip::consent::announcement`] already did to the TEXT, so the voice and the
/// words stay in the same language instead of speaking Italian words with a Chinese voice.
pub fn speak_language(lang: &str) -> String {
    let l = lang.trim();
    if l.contains('-') {
        return l.to_string();
    }
    match l.to_lowercase().as_str() {
        "ar" => "ar-SA",
        "bg" => "bg-BG",
        "ca" => "ca-ES",
        "cs" => "cs-CZ",
        "da" => "da-DK",
        "de" => "de-DE",
        "el" => "el-GR",
        "en" => "en-US",
        "es" => "es-ES",
        "fi" => "fi-FI",
        "fr" => "fr-FR",
        "he" => "he-IL",
        "hi" => "hi-IN",
        "hr" => "hr-HR",
        "hu" => "hu-HU",
        "id" => "id-ID",
        "it" => "it-IT",
        "ja" => "ja-JP",
        "ko" => "ko-KR",
        "ms" => "ms-MY",
        "nb" | "no" => "nb-NO",
        "nl" => "nl-NL",
        "pl" => "pl-PL",
        "pt" => "pt-PT",
        "ro" => "ro-RO",
        "ru" => "ru-RU",
        "sk" => "sk-SK",
        "sl" => "sl-SI",
        "sv" => "sv-SE",
        "ta" => "ta-IN",
        "th" => "th-TH",
        "tr" => "tr-TR",
        "uk" => "uk-UA",
        "vi" => "vi-VN",
        "zh" => "zh-CN",
        _ => "en-US",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telephony::MediaCodec;
    use bytes::Bytes;
    use ed25519_dalek::{Signer, SigningKey};

    fn cfg() -> TelnyxConfig {
        TelnyxConfig {
            api_key: "KEY".into(),
            api_base: crate::config::TELNYX_DEFAULT_API_BASE.into(),
            connection_id: "conn-1".into(),
            outbound_voice_profile_id: Some("ovp-1".into()),
            public_key_b64: String::new(),
            default_caller_id: Some("+390212345678".into()),
            media_anchor: "Frankfurt, Germany".into(),
        }
    }

    /// Same config, pointed at a local stub server instead of the real carrier — used
    /// only by the tests that must observe an actual HTTP request/response (streaming a
    /// document, resolving an address), where a pure-function test cannot show what went
    /// over the wire.
    fn cfg_with_base(base: String) -> TelnyxConfig {
        TelnyxConfig {
            api_base: base,
            ..cfg()
        }
    }

    /// A deterministic keypair, so the signature tests are reproducible.
    fn keypair() -> (SigningKey, String) {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let pk_b64 = B64.encode(sk.verifying_key().to_bytes());
        (sk, pk_b64)
    }

    fn sign(sk: &SigningKey, ts: &str, body: &[u8]) -> WebhookHeaders {
        let mut msg = Vec::new();
        msg.extend_from_slice(ts.as_bytes());
        msg.push(b'|');
        msg.extend_from_slice(body);
        WebhookHeaders {
            signature: Some(B64.encode(sk.sign(&msg).to_bytes())),
            timestamp: Some(ts.to_string()),
        }
    }

    fn event_body(event_type: &str, extra: Value) -> Vec<u8> {
        let mut payload = json!({
            "call_control_id": "v3:leg-abc",
            "client_state": encode_client_state("call-42"),
        });
        if let Value::Object(map) = extra {
            for (k, v) in map {
                payload[k] = v;
            }
        }
        serde_json::to_vec(&json!({
            "data": {
                "id": "evt-1",
                "event_type": event_type,
                "occurred_at": "2026-09-09T10:00:00Z",
                "payload": payload,
            }
        }))
        .unwrap()
    }

    // ---- recording download URLs (fixed 1.58.5) --------------------------------
    //
    // Response shape verified against
    // developers.telnyx.com/api-reference/call-recordings/retrieve-a-call-recording
    // (2026-09-16): `data.download_urls.{mp3,wav}`, both optional strings.

    #[test]
    fn recording_download_prefers_mp3_over_wav() {
        let data =
            json!({ "download_urls": { "mp3": "https://x/a.mp3", "wav": "https://x/a.wav" } });
        assert_eq!(
            download_url_from_recording(&data).as_deref(),
            Some("https://x/a.mp3")
        );
    }

    #[test]
    fn recording_download_falls_back_to_wav_when_mp3_is_absent() {
        let data = json!({ "download_urls": { "wav": "https://x/a.wav" } });
        assert_eq!(
            download_url_from_recording(&data).as_deref(),
            Some("https://x/a.wav")
        );
    }

    #[test]
    fn recording_download_is_none_without_a_download_urls_object() {
        // The still-processing shape: the recording row exists at Telnyx but has no
        // link yet.
        assert_eq!(
            download_url_from_recording(&json!({ "status": "processing" })),
            None
        );
    }

    #[test]
    fn recording_download_is_none_when_both_formats_are_missing_or_blank() {
        assert_eq!(
            download_url_from_recording(&json!({ "download_urls": {} })),
            None
        );
        assert_eq!(
            download_url_from_recording(&json!({ "download_urls": { "mp3": "" } })),
            None
        );
        // A blank mp3 must not hide a usable wav.
        assert_eq!(
            download_url_from_recording(
                &json!({ "download_urls": { "mp3": "", "wav": "https://x.test/r.wav" } })
            )
            .as_deref(),
            Some("https://x.test/r.wav")
        );
    }

    // ---- EU posture -----------------------------------------------------------

    #[test]
    fn the_eu_claim_requires_both_the_eu_base_and_an_in_region_anchor() {
        // Either half alone is a residency claim that is not true, and it is the kind of
        // thing that gets asserted on a sales call.
        assert!(cfg().is_eu());

        let mut global_base = cfg();
        global_base.api_base = "https://api.telnyx.com".into();
        assert!(!global_base.is_eu(), "the global base is not EU");

        let mut us_anchor = cfg();
        us_anchor.media_anchor = "Chicago, IL".into();
        assert!(!us_anchor.is_eu(), "an out-of-region anchor is not EU");

        for anchor in ["Frankfurt, Germany", "Amsterdam, Netherlands", "London, UK"] {
            let mut c = cfg();
            c.media_anchor = anchor.into();
            assert!(c.is_eu(), "{anchor}");
        }
    }

    #[test]
    fn the_api_key_never_appears_in_a_debug_dump() {
        let dump = format!("{:?}", cfg());
        assert!(!dump.contains("KEY"), "{dump}");
        assert!(dump.contains("redacted"));
    }

    #[test]
    fn the_provider_declares_the_one_stream_limit_the_architecture_depends_on() {
        let p = TelnyxProvider::new(cfg(), 300);
        assert_eq!(p.metadata().capabilities.max_streams_per_leg, 1);
        assert!(p.metadata().capabilities.bidirectional_media);
        assert!(p.metadata().eu_telephony);
    }

    // ---- signature verification (R24) -----------------------------------------

    #[test]
    fn a_correctly_signed_webhook_verifies() {
        let (sk, pk) = keypair();
        let now = Utc::now();
        let ts = now.timestamp().to_string();
        let body = event_body("call.answered", json!({}));
        assert!(verify_signature(
            &pk,
            &sign(&sk, &ts, &body),
            &body,
            now,
            Duration::minutes(5)
        )
        .is_ok());
    }

    #[test]
    fn an_empty_public_key_rejects_everything_rather_than_accepting_it() {
        // The branch that matters most. A deployment that forgot TELNYX_PUBLIC_KEY would
        // otherwise take call-control instructions from anyone who can reach the endpoint.
        let (sk, _) = keypair();
        let now = Utc::now();
        let ts = now.timestamp().to_string();
        let body = event_body("call.answered", json!({}));
        assert_eq!(
            verify_signature("", &sign(&sk, &ts, &body), &body, now, Duration::minutes(5))
                .unwrap_err(),
            WebhookError::BadSignature
        );
    }

    #[test]
    fn a_tampered_body_fails_verification() {
        let (sk, pk) = keypair();
        let now = Utc::now();
        let ts = now.timestamp().to_string();
        let body = event_body("call.answered", json!({}));
        let headers = sign(&sk, &ts, &body);
        // Same signature, different body — the classic replay-with-substitution.
        let other = event_body("call.hangup", json!({"hangup_cause": "user_busy"}));
        assert_eq!(
            verify_signature(&pk, &headers, &other, now, Duration::minutes(5)).unwrap_err(),
            WebhookError::BadSignature
        );
    }

    #[test]
    fn a_signature_from_another_key_fails() {
        let (_, pk) = keypair();
        let attacker = SigningKey::from_bytes(&[9u8; 32]);
        let now = Utc::now();
        let ts = now.timestamp().to_string();
        let body = event_body("call.answered", json!({}));
        assert_eq!(
            verify_signature(
                &pk,
                &sign(&attacker, &ts, &body),
                &body,
                now,
                Duration::minutes(5)
            )
            .unwrap_err(),
            WebhookError::BadSignature
        );
    }

    #[test]
    fn a_replay_is_refused_even_though_its_signature_is_genuine() {
        // Why the timestamp is checked BEFORE the signature: this body is correctly
        // signed. Only its age gives it away.
        let (sk, pk) = keypair();
        let then = Utc::now() - Duration::minutes(30);
        let ts = then.timestamp().to_string();
        let body = event_body("call.answered", json!({}));
        let headers = sign(&sk, &ts, &body);
        assert_eq!(
            verify_signature(&pk, &headers, &body, Utc::now(), Duration::minutes(5)).unwrap_err(),
            WebhookError::StaleTimestamp
        );
        assert!(verify_signature(&pk, &headers, &body, then, Duration::minutes(5)).is_ok());
    }

    #[test]
    fn a_webhook_from_the_future_is_refused_too() {
        let (sk, pk) = keypair();
        let future = Utc::now() + Duration::minutes(30);
        let ts = future.timestamp().to_string();
        let body = event_body("call.answered", json!({}));
        assert_eq!(
            verify_signature(
                &pk,
                &sign(&sk, &ts, &body),
                &body,
                Utc::now(),
                Duration::minutes(5)
            )
            .unwrap_err(),
            WebhookError::StaleTimestamp
        );
    }

    #[test]
    fn missing_or_malformed_headers_are_refused() {
        let (_, pk) = keypair();
        let now = Utc::now();
        let body = event_body("call.answered", json!({}));
        assert_eq!(
            verify_signature(
                &pk,
                &WebhookHeaders::default(),
                &body,
                now,
                Duration::minutes(5)
            )
            .unwrap_err(),
            WebhookError::MissingSignature
        );
        let junk = WebhookHeaders {
            signature: Some("not base64!!".into()),
            timestamp: Some(now.timestamp().to_string()),
        };
        assert_eq!(
            verify_signature(&pk, &junk, &body, now, Duration::minutes(5)).unwrap_err(),
            WebhookError::BadSignature
        );
        let bad_ts = WebhookHeaders {
            signature: Some(B64.encode([0u8; 64])),
            timestamp: Some("yesterday".into()),
        };
        assert_eq!(
            verify_signature(&pk, &bad_ts, &body, now, Duration::minutes(5)).unwrap_err(),
            WebhookError::BadSignature
        );
    }

    // ---- event normalisation --------------------------------------------------

    #[test]
    fn lifecycle_events_normalise_into_domain_vocabulary() {
        for (raw, expected) in [
            ("call.initiated", ProviderEventKind::Initiated),
            ("call.answered", ProviderEventKind::Answered),
            ("streaming.started", ProviderEventKind::MediaStarted),
            ("streaming.stopped", ProviderEventKind::MediaStopped),
            ("streaming.failed", ProviderEventKind::MediaStopped),
        ] {
            let ev = normalise(&event_body(raw, json!({}))).unwrap();
            assert_eq!(ev.kind, expected, "{raw}");
            assert_eq!(ev.provider, TELNYX_ID);
            assert_eq!(ev.event_id, "evt-1");
            assert_eq!(ev.leg_id, LegId::new("v3:leg-abc"));
            assert_eq!(ev.client_state.as_deref(), Some("call-42"), "{raw}");
        }
    }

    #[test]
    fn hangup_causes_map_and_unknown_ones_stay_visible() {
        for (cause, expected) in [
            ("user_busy", FailureReason::Busy),
            ("call_rejected", FailureReason::Rejected),
            ("timeout", FailureReason::NoAnswer),
            ("invalid_bnumber", FailureReason::Unallocated),
            // Ends the call, but says nothing about whether it succeeded — the state
            // machine decides that from whether anyone answered.
            ("normal_clearing", FailureReason::Unmapped),
            ("something_new", FailureReason::Unmapped),
        ] {
            let body = event_body("call.hangup", json!({ "hangup_cause": cause }));
            assert_eq!(
                normalise(&body).unwrap().kind,
                ProviderEventKind::Hangup { cause: expected },
                "{cause}"
            );
        }
        // A hangup with no cause at all still parses; it must not take the call down.
        let body = event_body("call.hangup", json!({}));
        assert!(matches!(
            normalise(&body).unwrap().kind,
            ProviderEventKind::Hangup { .. }
        ));
    }

    #[test]
    fn a_recording_event_finds_a_playable_url_in_either_map() {
        let body = event_body(
            "call.recording.saved",
            json!({
                "recording_urls": { "mp3": "https://example/rec.mp3", "wav": null },
                "duration_millis": 91_500
            }),
        );
        assert_eq!(
            normalise(&body).unwrap().kind,
            ProviderEventKind::RecordingSaved {
                url: "https://example/rec.mp3".into(),
                recording_id: None,
                duration_secs: 91
            }
        );
        // Falls back to the public map, and to wav when mp3 is absent.
        let body = event_body(
            "call.recording.saved",
            json!({ "public_recording_urls": { "wav": "https://example/rec.wav" } }),
        );
        assert!(matches!(
            normalise(&body).unwrap().kind,
            ProviderEventKind::RecordingSaved { url, .. } if url.ends_with(".wav")
        ));
    }

    #[test]
    fn dtmf_arrives_as_a_single_digit_from_either_field() {
        for extra in [json!({"digit": "1"}), json!({"digits": "1"})] {
            assert_eq!(
                normalise(&event_body("call.dtmf.received", extra))
                    .unwrap()
                    .kind,
                ProviderEventKind::Dtmf { digit: '1' }
            );
        }
        assert!(matches!(
            normalise(&event_body("call.dtmf.received", json!({}))).unwrap_err(),
            WebhookError::Malformed { .. }
        ));
    }

    #[test]
    fn an_unmodelled_event_is_recorded_rather_than_rejected() {
        // Recorded so the idempotency ledger still absorbs its redelivery, and so a
        // renamed provider event shows up as data instead of as silence.
        assert_eq!(
            normalise(&event_body("call.machine.detection.ended", json!({})))
                .unwrap()
                .kind,
            ProviderEventKind::Unhandled {
                raw_type: "call.machine.detection.ended".into()
            }
        );
    }

    #[test]
    fn an_event_with_no_leg_is_malformed() {
        let body = serde_json::to_vec(&json!({
            "data": { "id": "e", "event_type": "call.answered",
                      "occurred_at": "2026-09-09T10:00:00Z", "payload": {} }
        }))
        .unwrap();
        assert!(matches!(
            normalise(&body).unwrap_err(),
            WebhookError::Malformed { .. }
        ));
    }

    #[test]
    fn unknown_fields_do_not_break_parsing() {
        // The provider adds fields without notice. Refusing an event because it grew a key
        // would take the whole call lifecycle down for a cosmetic change.
        let body = serde_json::to_vec(&json!({
            "data": {
                "id": "e", "event_type": "call.answered",
                "occurred_at": "2026-09-09T10:00:00Z",
                "record_type": "event",
                "payload": { "call_control_id": "v3:x", "brand_new_field": 42 }
            },
            "meta": { "attempt": 3 }
        }))
        .unwrap();
        assert_eq!(normalise(&body).unwrap().kind, ProviderEventKind::Answered);
    }

    // ---- number order webhooks (spec 0119 Phase 7, design D5) -----------------
    //
    // `number_order.*` events describe an ORDER, not a call leg, and Telnyx's schema for
    // them carries no `call_control_id` at all — `normalise` must recognise them BEFORE
    // the call-control envelope below requires one, which used to reject every one of
    // these events as Malformed.

    fn number_order_event_body(event_type: &str, payload: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "data": {
                "id": "evt-order-1",
                "event_type": event_type,
                "occurred_at": "2026-09-17T10:00:00Z",
                "payload": payload,
            }
        }))
        .unwrap()
    }

    #[test]
    fn a_number_order_complete_event_is_recognised_before_call_control_id_is_required() {
        let body = number_order_event_body(
            "number_order.complete",
            json!({ "id": "order-1", "sub_number_orders_ids": ["sub-1", "sub-2"] }),
        );
        let event = normalise(&body).unwrap();
        assert_eq!(event.leg_id, LegId::new(""));
        assert_eq!(
            event.kind,
            ProviderEventKind::NumberOrderCompleted {
                order_id: "order-1".into(),
                sub_order_ids: vec!["sub-1".into(), "sub-2".into()],
            }
        );
    }

    #[test]
    fn a_number_order_complete_event_falls_back_to_the_per_number_sub_order_id() {
        // The same defensive fallback `parse_order_ref` uses for the purchase response:
        // `sub_number_orders_ids` is the confirmed source, `phone_numbers[].sub_number_order_id`
        // costs nothing to also accept.
        let body = number_order_event_body(
            "number_order.complete",
            json!({
                "id": "order-2",
                "phone_numbers": [
                    { "id": "pn-1", "sub_number_order_id": "sub-9" },
                    { "id": "pn-2", "sub_number_order_id": "sub-10" }
                ]
            }),
        );
        assert_eq!(
            normalise(&body).unwrap().kind,
            ProviderEventKind::NumberOrderCompleted {
                order_id: "order-2".into(),
                sub_order_ids: vec!["sub-9".into(), "sub-10".into()],
            }
        );
    }

    #[test]
    fn a_number_order_event_with_no_parseable_ids_is_recorded_rather_than_rejected() {
        // Same "recorded, moves nothing" honesty `Unhandled` already gives every other
        // event type this crate does not fully model — never a hard failure, because the
        // webhook is only ever a NUDGE (design D5); the sweep is the source of truth.
        let body = number_order_event_body("number_order.complete", json!({}));
        assert_eq!(
            normalise(&body).unwrap().kind,
            ProviderEventKind::Unhandled {
                raw_type: "number_order.complete".into()
            }
        );
    }

    #[test]
    fn an_unmodelled_number_order_subtype_is_recorded_rather_than_rejected() {
        let body = number_order_event_body("number_order.requirements_completed", json!({}));
        assert_eq!(
            normalise(&body).unwrap().kind,
            ProviderEventKind::Unhandled {
                raw_type: "number_order.requirements_completed".into()
            }
        );
    }

    #[test]
    fn a_call_event_with_no_leg_still_fails_the_way_it_always_has() {
        // Regression: the new event-type-first branch must not swallow the original
        // call-control requirement for anything that is not a number_order event.
        let body = serde_json::to_vec(&json!({
            "data": { "id": "e", "event_type": "call.answered",
                      "occurred_at": "2026-09-09T10:00:00Z", "payload": {} }
        }))
        .unwrap();
        assert!(matches!(
            normalise(&body).unwrap_err(),
            WebhookError::Malformed { .. }
        ));
    }

    #[test]
    fn client_state_round_trips_through_base64() {
        assert_eq!(
            decode_client_state(&encode_client_state("call-42")).as_deref(),
            Some("call-42")
        );
        // Garbage decodes to nothing rather than to a wrong correlation id.
        assert_eq!(decode_client_state("!!!"), None);
    }

    // ---- request bodies -------------------------------------------------------

    #[test]
    fn a_pstn_dial_omits_unsupported_media_encryption_and_is_idempotent() {
        let req = DialRequest {
            to: super::super::E164::parse("+8613800138000").unwrap(),
            from: super::super::E164::parse("+390212345678").unwrap(),
            client_state: "call-42".into(),
            timeout_secs: 30,
            region: "Frankfurt, Germany".into(),
        };
        let body = dial_body(&cfg(), &req);
        assert_eq!(body["to"], "+8613800138000");
        assert_eq!(body["from"], "+390212345678");
        assert_eq!(body["connection_id"], "conn-1");
        assert_eq!(body["outbound_voice_profile_id"], "ovp-1");
        assert_eq!(body["client_state"], encode_client_state("call-42"));
        assert!(body["command_id"].as_str().is_some_and(|s| !s.is_empty()));
        // Telnyx rejects SRTP on PSTN call creation with error 10011. Media streaming is
        // configured separately after the call answers; the dial itself must omit this field.
        assert!(body.get("media_encryption").is_none());

        // Two dials must not share a command id, or the second would be swallowed as a
        // duplicate of the first.
        assert_ne!(dial_body(&cfg(), &req)["command_id"], body["command_id"]);
    }

    #[test]
    fn bidirectional_streaming_uses_rtp_not_mp3() {
        // mp3 accepts at most one submission per second — a whole second of latency on a
        // conversation, which defeats the product.
        let c = MediaStreamConfig {
            url: "wss://media.test/voip/media/tok".into(),
            codec: MediaCodec::L16,
            track: MediaTrack::Inbound,
            bidirectional: true,
        };
        let body = streaming_body(&c);
        assert_eq!(body["stream_url"], "wss://media.test/voip/media/tok");
        assert_eq!(body["stream_track"], "inbound_track");
        assert_eq!(body["stream_codec"], "L16");
        assert_eq!(body["stream_bidirectional_mode"], "rtp");
        assert_eq!(body["stream_bidirectional_codec"], "L16");
        // Telnyx defaults stream_bidirectional_sampling_rate to 8000 when it is omitted.
        // We negotiate L16 at 16 kHz, so leaving this unset would silently downsample the
        // whole call to 8 kHz on the wire while every leg keeps decoding it as 16 kHz.
        assert_eq!(body["stream_bidirectional_sampling_rate"], 16_000);
    }

    #[test]
    fn a_unidirectional_stream_omits_the_bidirectional_fields() {
        let c = MediaStreamConfig {
            url: "wss://media.test/x".into(),
            codec: MediaCodec::Pcmu,
            track: MediaTrack::Both,
            bidirectional: false,
        };
        let body = streaming_body(&c);
        assert_eq!(body["stream_track"], "both_tracks");
        assert_eq!(body["stream_codec"], "PCMU");
        assert!(body.get("stream_bidirectional_mode").is_none());
        assert!(body.get("stream_bidirectional_sampling_rate").is_none());
    }

    // ---- HTTP status mapping --------------------------------------------------

    #[test]
    fn http_statuses_map_onto_the_right_retry_behaviour() {
        use reqwest::StatusCode;
        assert!(classify(StatusCode::OK).is_none());
        assert!(classify(StatusCode::CREATED).is_none());
        assert_eq!(
            classify(StatusCode::UNAUTHORIZED),
            Some(ProviderError::Unauthorized)
        );
        assert_eq!(
            classify(StatusCode::FORBIDDEN),
            Some(ProviderError::Unauthorized)
        );
        assert_eq!(
            classify(StatusCode::PAYMENT_REQUIRED),
            Some(ProviderError::AccountBlocked)
        );
        assert_eq!(
            classify(StatusCode::UNPROCESSABLE_ENTITY),
            Some(ProviderError::DestinationRefused)
        );
        assert_eq!(
            classify(StatusCode::TOO_MANY_REQUESTS),
            Some(ProviderError::RateLimited)
        );
        assert!(matches!(
            classify(StatusCode::BAD_GATEWAY),
            Some(ProviderError::Unavailable { .. })
        ));
        // Credentials and balance must NOT be retried — retrying burns rate limit and
        // fixes nothing.
        assert!(!ProviderError::Unauthorized.is_transient());
        assert!(!ProviderError::AccountBlocked.is_transient());
        assert!(ProviderError::RateLimited.is_transient());
    }

    // ---- what the carrier actually said ---------------------------------------

    #[test]
    fn a_carrier_error_body_is_reduced_to_the_code_and_the_reason() {
        // The exact shape Telnyx returns for the refusal that started this: an outbound
        // voice profile with no Spain in its destinations. The status alone says
        // "destination refused"; only the body says WHICH knob is wrong, and that is the
        // difference between reading one log line and reading the provider's dashboard.
        let body = r#"{"errors":[{"code":"10015",
            "title":"Destination not allowed",
            "detail":"The outbound voice profile does not allow calls to this destination.",
            "meta":{"url":"https://developers.telnyx.com/docs/errors"}}]}"#;
        let d = error_detail(body);
        assert!(d.contains("10015"), "{d}");
        assert!(d.contains("Destination not allowed"), "{d}");
        assert!(d.contains("outbound voice profile"), "{d}");
        // The documentation link is noise in a log line.
        assert!(!d.contains("developers.telnyx.com"), "{d}");
    }

    #[test]
    fn several_carrier_errors_all_survive_into_one_line() {
        let body = r#"{"errors":[{"code":"10005","title":"Unauthorized"},
                                 {"code":"90001","title":"Profile missing"}]}"#;
        let d = error_detail(body);
        assert!(d.contains("10005") && d.contains("90001"), "{d}");
        assert!(!d.contains('\n'), "a log line is one line: {d}");
    }

    #[test]
    fn an_error_body_never_carries_a_telephone_number_into_a_log() {
        // This is the whole reason the body was being thrown away rather than logged.
        // `E164::Display` is masked for the same reason; a carrier's prose must not be
        // the hole in that rule.
        let body = r#"{"errors":[{"code":"10015","title":"Destination not allowed",
            "detail":"Calls to +34665367910 from +390212345678 are not permitted",
            "meta":{"to":"+34665367910"}}]}"#;
        let d = error_detail(body);
        assert!(!d.contains("34665367910"), "leaked the callee: {d}");
        assert!(!d.contains("390212345678"), "leaked the caller: {d}");
        // Redacted, not deleted: the sentence must still read as being about a number.
        assert!(d.contains('+'), "{d}");
        assert!(d.contains("not permitted"), "{d}");
        // A short numeric like the error code is not a telephone number and must survive.
        assert!(d.contains("10015"), "{d}");
    }

    #[test]
    fn an_unreadable_error_body_is_still_reported_redacted_and_bounded() {
        // A proxy or a WAF in front of the carrier answers in HTML. Reporting nothing
        // would put us back where we started, and reporting all of it would push a page
        // of markup through the log pipeline.
        let html = format!("<html><body>{}+34665367910</body></html>", "x".repeat(4000));
        let d = error_detail(&html);
        assert!(!d.is_empty());
        assert!(d.len() <= MAX_ERROR_DETAIL, "unbounded: {} bytes", d.len());
        assert!(!d.contains("34665367910"), "leaked a number: {d}");
    }

    #[test]
    fn an_empty_error_body_says_so_rather_than_logging_nothing() {
        assert!(!error_detail("").is_empty());
        assert!(!error_detail("   ").is_empty());
    }

    #[test]
    fn truncation_never_splits_a_multi_byte_character() {
        // `String::truncate` panics on a char boundary, and a carrier that answers in
        // Japanese must not take the process down on the error path.
        let d = error_detail(&"あ".repeat(4000));
        assert!(d.len() <= MAX_ERROR_DETAIL);
    }

    #[tokio::test]
    async fn a_per_leg_cdr_is_reported_as_unsupported_rather_than_faked() {
        // The provider rates asynchronously through batched usage reports. Returning a
        // made-up zero here would show every call at 100% margin, which is worse than
        // showing it as unreconciled.
        let p = TelnyxProvider::new(cfg(), 300);
        assert_eq!(
            p.fetch_cdr(&LegId::new("v3:x")).await.unwrap_err(),
            ProviderError::Unsupported {
                operation: "fetch a per-leg CDR synchronously"
            }
        );
    }

    // ---- regulatory requirements (spec 0119) -----------------------------------
    //
    // Response shapes verified against Telnyx's published OpenAPI spec
    // (github.com/team-telnyx/openapi, `openapi/spec3.json`, fetched 2026-09-16) —
    // schema `RegulatoryRequirements`. Used as the source of truth in place of the
    // interactive docs site, which serves a JS app shell to a plain HTTP fetch and
    // returns no readable body.

    #[test]
    fn requirement_list_flattens_the_nested_shape_and_maps_field_types() {
        // GET /v2/regulatory_requirements returns one entry PER matching
        // (country, phone_number_type, action) combination, each carrying its own
        // nested `regulatory_requirements` array — not a flat list at `data[]`.
        let body = json!({
            "data": [{
                "country_code": "FR",
                "phone_number_type": "mobile",
                "action": "ordering",
                "regulatory_requirements": [
                    {
                        "id": "req-1",
                        "name": "Proof of address",
                        "description": "A recent utility bill",
                        "example": "600 Congress Avenue",
                        "field_type": "address"
                    },
                    { "id": "req-2", "name": "Full name", "field_type": "textual" },
                    { "id": "req-3", "name": "ID document", "field_type": "document" }
                ]
            }]
        });
        let specs = parse_requirements(&body);
        assert_eq!(specs.len(), 3);
        assert_eq!(specs[0].id, "req-1");
        assert_eq!(
            specs[0].description.as_deref(),
            Some("A recent utility bill")
        );
        assert_eq!(specs[0].example.as_deref(), Some("600 Congress Avenue"));
        assert_eq!(specs[0].kind, RequirementKind::Address);
        assert_eq!(specs[1].kind, RequirementKind::Textual);
        assert_eq!(specs[2].kind, RequirementKind::Document);
    }

    #[test]
    fn an_unrecognised_field_type_falls_back_to_textual() {
        // `datetime` is a real Telnyx value this crate has no separate type for, and any
        // future value not seen yet must degrade to the safe case rather than to Document.
        for unknown in ["datetime", "something_new"] {
            let body = json!({"data": [{"regulatory_requirements": [
                {"id": "r", "name": "n", "field_type": unknown}
            ]}]});
            assert_eq!(
                parse_requirements(&body)[0].kind,
                RequirementKind::Textual,
                "{unknown}"
            );
        }
    }

    #[test]
    fn an_empty_or_missing_data_array_yields_no_requirements() {
        assert!(parse_requirements(&json!({})).is_empty());
        assert!(parse_requirements(&json!({"data": []})).is_empty());
    }

    #[test]
    fn the_group_create_body_matches_the_documented_shape() {
        // POST /v2/requirement_groups requires country_code, phone_number_type, action —
        // verified against the `RequirementGroup` POST requestBody schema.
        let q = requirement_query();
        let body = requirement_group_create_body(&q, "org-42");
        assert_eq!(body["country_code"], "FR");
        assert_eq!(body["phone_number_type"], "mobile");
        assert_eq!(body["action"], "ordering");
        assert_eq!(body["customer_reference"], "org-42");
    }

    #[test]
    fn a_requirement_group_response_parses_its_values_by_field_type() {
        // POST/GET/PATCH /v2/requirement_groups[/{id}] all return the `RequirementGroup`
        // object directly — no `data` envelope, unlike almost every other Telnyx resource.
        let body = json!({
            "id": "grp-1",
            "status": "pending-approval",
            "regulatory_requirements": [
                { "requirement_id": "req-1", "field_value": "600 Congress Ave", "field_type": "address" },
                { "requirement_id": "req-2", "field_value": "doc-99", "field_type": "document" },
                { "requirement_id": "req-3", "field_value": "", "field_type": "textual" }
            ]
        });
        let group = parse_requirement_group(&body).expect("group id present");
        assert_eq!(group.id, RequirementGroupId("grp-1".into()));
        assert_eq!(group.status, GroupStatus::PendingApproval);
        assert_eq!(group.requirements.len(), 3);
        assert_eq!(
            group.requirements[0].1,
            Some(FieldValue::Text("600 Congress Ave".into()))
        );
        assert_eq!(
            group.requirements[1].1,
            Some(FieldValue::Document("doc-99".into()))
        );
        // An empty field_value string means "not submitted yet", not a blank text answer.
        assert_eq!(group.requirements[2].1, None);
    }

    #[test]
    fn a_group_response_tolerates_being_wrapped_in_data_too() {
        // Defensive only: every confirmed Telnyx sample is unwrapped, but a `data`-wrapped
        // shape costs nothing extra to accept and matches this file's convention elsewhere
        // (`purchase_number` reads `body.get("data").unwrap_or(&body)`).
        let body =
            json!({"data": {"id": "grp-2", "status": "approved", "regulatory_requirements": []}});
        let group = parse_requirement_group(&body).expect("group id present");
        assert_eq!(group.id, RequirementGroupId("grp-2".into()));
        assert_eq!(group.status, GroupStatus::Approved);
    }

    #[test]
    fn a_group_response_with_no_id_parses_to_none() {
        assert!(parse_requirement_group(&json!({"status": "approved"})).is_none());
    }

    #[test]
    fn every_documented_group_status_word_maps_and_unknown_words_stay_unknown() {
        for (raw, expected) in [
            ("approved", GroupStatus::Approved),
            ("unapproved", GroupStatus::Unapproved),
            ("pending-approval", GroupStatus::PendingApproval),
            ("pending_approval", GroupStatus::PendingApproval),
            ("declined", GroupStatus::Declined),
            ("expired", GroupStatus::Expired),
            ("no-longer-eligible", GroupStatus::NoLongerEligible),
            ("something_new", GroupStatus::Unknown),
        ] {
            assert_eq!(GroupStatus::parse(raw), expected, "{raw}");
        }
    }

    #[test]
    fn the_group_patch_body_carries_already_resolved_values_as_plain_strings() {
        // PATCH /v2/requirement_groups/:id — verified against the same `RequirementGroup`
        // schema: `regulatory_requirements: [{requirement_id, field_value}]`, both strings.
        // By the time this pure function runs, every `FieldValue` (including `Address`)
        // has already been resolved to a plain string by
        // `TelnyxProvider::resolve_field_value` — this function never sees a `FieldValue`.
        let values = vec![
            ("req-1".to_string(), "Jane Doe".to_string()),
            ("req-2".to_string(), "doc-7".to_string()),
            // An address resolves to the opaque provider id `create_address` returned,
            // never to raw address text.
            ("req-3".to_string(), "addr-42".to_string()),
        ];
        let body = requirement_group_patch_body(&values);
        let reqs = body["regulatory_requirements"].as_array().unwrap();
        assert_eq!(reqs.len(), 3);
        assert_eq!(reqs[0]["requirement_id"], "req-1");
        assert_eq!(reqs[0]["field_value"], "Jane Doe");
        assert_eq!(reqs[1]["requirement_id"], "req-2");
        assert_eq!(reqs[1]["field_value"], "doc-7");
        assert_eq!(reqs[2]["requirement_id"], "req-3");
        assert_eq!(reqs[2]["field_value"], "addr-42");
    }

    // ---- address resolution (spec 0119 D13, PR4) -------------------------------
    //
    // `POST /v2/addresses` — verified against the published `AddressCreate`/`Address`
    // schemas (2026-09-16): required fields are `first_name`, `last_name`,
    // `business_name`, `street_address`, `locality`, `country_code`; the response wraps
    // the created `Address` in `data`, and `data.id` is the opaque id later submitted as
    // the requirement's `field_value`.

    fn address_value() -> super::super::AddressValue {
        super::super::AddressValue {
            first_name: "Jane".into(),
            last_name: "Doe".into(),
            business_name: "Acme SRL".into(),
            street_address: "1 Rue de Paris".into(),
            extended_address: None,
            locality: "Paris".into(),
            administrative_area: None,
            postal_code: "75001".into(),
            country_code: "FR".into(),
        }
    }

    #[test]
    fn the_address_create_body_carries_every_field_the_schema_requires() {
        let body = address_create_body(&address_value());
        assert_eq!(body["first_name"], "Jane");
        assert_eq!(body["last_name"], "Doe");
        assert_eq!(body["business_name"], "Acme SRL");
        assert_eq!(body["street_address"], "1 Rue de Paris");
        assert_eq!(body["locality"], "Paris");
        assert_eq!(body["country_code"], "FR");
        assert_eq!(body["postal_code"], "75001");
        // Optional fields absent from the value must not appear as JSON `null` — the
        // schema treats a present-but-null field the same risk class as a wrong one.
        assert!(body.get("extended_address").is_none());
        assert!(body.get("administrative_area").is_none());
    }

    #[test]
    fn parse_address_id_reads_the_data_wrapped_id() {
        let body = json!({"data": {"id": "addr-1", "record_type": "address"}});
        assert_eq!(parse_address_id(&body).as_deref(), Some("addr-1"));
    }

    #[test]
    fn parse_address_id_is_none_without_an_id() {
        assert!(parse_address_id(&json!({"data": {}})).is_none());
        assert!(parse_address_id(&json!({})).is_none());
    }

    // ---- document upload (spec 0119 D9/D10, PR4) -------------------------------
    //
    // `POST /v2/documents` — verified against the published
    // `CreateMultiPartDocServiceDocumentRequest`/`DocServiceDocument` schemas
    // (2026-09-16): the multipart field is named `file`, and the response wraps the
    // created document in `data` with `id` and `av_scan_status`
    // (`scanned`/`infected`/`pending_scan`/`not_scanned`).

    #[test]
    fn parse_uploaded_document_reads_id_and_scan_status() {
        let body = json!({"data": {"id": "doc-1", "av_scan_status": "scanned"}});
        let doc = parse_uploaded_document(&body).expect("id present");
        assert_eq!(doc.id, "doc-1");
        assert_eq!(doc.av_scan_status, "scanned");
    }

    #[test]
    fn parse_uploaded_document_is_none_without_an_id() {
        assert!(parse_uploaded_document(&json!({"data": {"av_scan_status": "scanned"}})).is_none());
    }

    #[tokio::test]
    async fn upload_document_streams_the_body_without_buffering_it_first() {
        // The property this test exists to prove: the adapter forwards `upload.body` as
        // it arrives (several chunks, D9) rather than collecting it into one buffer
        // before sending — a local stub server is the only way to observe that, since a
        // pure function cannot show what a real HTTP body looked like on the wire.
        use axum::extract::{Multipart, State};
        use axum::routing::post;
        use axum::{Json, Router};
        use futures::StreamExt;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;

        let received_bytes = Arc::new(AtomicU64::new(0));
        let received_chunks = Arc::new(AtomicU64::new(0));
        let app = Router::new()
            .route(
                "/v2/documents",
                post(
                    |State((bytes, chunks)): State<(Arc<AtomicU64>, Arc<AtomicU64>)>,
                     mut mp: Multipart| async move {
                        let mut field = mp.next_field().await.unwrap().expect("a `file` field");
                        assert_eq!(field.name(), Some("file"));
                        let mut total = 0u64;
                        // `chunk()`, not `bytes()`: the latter would buffer the whole
                        // field before this handler could observe more than one piece.
                        while let Some(chunk) = field.chunk().await.unwrap() {
                            total += chunk.len() as u64;
                            chunks.fetch_add(1, Ordering::SeqCst);
                        }
                        bytes.store(total, Ordering::SeqCst);
                        Json(json!({"data": {"id": "doc-1", "av_scan_status": "scanned"}}))
                    },
                ),
            )
            .with_state((received_bytes.clone(), received_chunks.clone()));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let p = TelnyxProvider::new(cfg_with_base(format!("http://{addr}")), 300);

        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(b"%PDF-1.4 ")),
            Ok(Bytes::from_static(b"chunk two ")),
            Ok(Bytes::from_static(b"chunk three")),
        ];
        let expected_len: u64 = chunks
            .iter()
            .map(|c| c.as_ref().unwrap().len() as u64)
            .sum();
        let body = futures::stream::iter(chunks).boxed();
        let upload = DocumentUpload {
            content_type: "application/pdf",
            body,
        };

        let uploaded = p.upload_document(upload).await.unwrap();
        assert_eq!(uploaded.id, "doc-1");
        assert_eq!(uploaded.av_scan_status, "scanned");
        assert_eq!(received_bytes.load(Ordering::SeqCst), expected_len);
        // Not a hard requirement of the wire format, but a red flag if it ever drops to
        // 1: it would mean the multipart body was assembled from one pre-joined buffer.
        assert!(received_chunks.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn submit_requirement_values_resolves_an_address_before_patching_the_group() {
        // End-to-end proof of D13: an `Address` value must reach `/v2/requirement_groups`
        // as the opaque id `POST /v2/addresses` returned, never as address text.
        use axum::extract::{Json as JsonBody, Path};
        use axum::routing::{patch, post};
        use axum::{Json, Router};
        use std::sync::{Arc, Mutex};

        let captured_address_body: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
        let captured_patch_body: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
        let addresses_capture = captured_address_body.clone();
        let patch_capture = captured_patch_body.clone();

        let app = Router::new()
            .route(
                "/v2/addresses",
                post(move |JsonBody(body): JsonBody<Value>| {
                    let capture = addresses_capture.clone();
                    async move {
                        *capture.lock().unwrap() = Some(body);
                        Json(json!({"data": {"id": "addr-99"}}))
                    }
                }),
            )
            .route(
                "/v2/requirement_groups/{id}",
                patch(
                    move |Path(id): Path<String>, JsonBody(body): JsonBody<Value>| {
                        let capture = patch_capture.clone();
                        async move {
                            *capture.lock().unwrap() = Some(body);
                            Json(json!({
                                "id": id,
                                "status": "pending-approval",
                                "regulatory_requirements": [
                                    {
                                        "requirement_id": "req-1",
                                        "field_value": "addr-99",
                                        "field_type": "address"
                                    }
                                ]
                            }))
                        }
                    },
                ),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let p = TelnyxProvider::new(cfg_with_base(format!("http://{addr}")), 300);
        let group = p
            .submit_requirement_values(
                &RequirementGroupId("grp-1".into()),
                &[("req-1".to_string(), FieldValue::Address(address_value()))],
            )
            .await
            .unwrap();

        assert_eq!(group.id, RequirementGroupId("grp-1".into()));
        assert_eq!(group.status, GroupStatus::PendingApproval);

        let address_body = captured_address_body
            .lock()
            .unwrap()
            .clone()
            .expect("POST /v2/addresses was called");
        assert_eq!(address_body["first_name"], "Jane");
        assert_eq!(address_body["last_name"], "Doe");
        assert_eq!(address_body["business_name"], "Acme SRL");

        let patch_body = captured_patch_body
            .lock()
            .unwrap()
            .clone()
            .expect("PATCH /v2/requirement_groups/:id was called");
        let reqs = patch_body["regulatory_requirements"].as_array().unwrap();
        assert_eq!(reqs[0]["requirement_id"], "req-1");
        // The resolved provider address id, never the raw address fields — proves
        // resolution actually happened rather than the value being forwarded unresolved.
        assert_eq!(reqs[0]["field_value"], "addr-99");
        let patch_text = patch_body.to_string();
        assert!(!patch_text.contains("Rue de Paris"));
        assert!(!patch_text.contains("Acme SRL"));
    }

    #[test]
    fn every_documented_order_status_word_maps_and_unknown_words_stay_unknown() {
        // `numbers_SubNumberOrder.status` enum is `pending|success|failure` — verified
        // against the published spec. `cancelled`/`deleted` are accepted too because
        // `POST /v2/sub_number_orders/:id/cancel` exists and design's own `transition()`
        // (spec 0119 R5) already models a cancelled/deleted order as a distinct outcome;
        // recognising the word costs nothing and a spec revision may add it later.
        for (raw, expected) in [
            ("pending", OrderStatus::Pending),
            ("success", OrderStatus::Success),
            ("failure", OrderStatus::Failure),
            ("cancelled", OrderStatus::Cancelled),
            ("canceled", OrderStatus::Cancelled),
            ("deleted", OrderStatus::Deleted),
            ("something_new", OrderStatus::Unknown),
        ] {
            assert_eq!(OrderStatus::parse(raw), expected, "{raw}");
        }
    }

    #[test]
    fn a_fully_met_sub_order_is_approved_regardless_of_the_status_string() {
        // `requirements_met: true` is the one authoritative bit Telnyx documents for "the
        // regulator is satisfied" — no `requirements_status` string for that case is
        // documented anywhere in the published spec, so the boolean wins over the string.
        let data = json!({
            "status": "success",
            "requirements_met": true,
            "phone_numbers": [{ "requirements_status": "requirement-info-pending" }]
        });
        let state = parse_sub_order_state(&data);
        assert_eq!(state.order, OrderStatus::Success);
        assert_eq!(state.requirements, RequirementsStatus::Approved);
        // The adapter never fabricates a group id the response did not echo back.
        assert_eq!(state.group, None);
    }

    #[test]
    fn the_documented_requirement_info_strings_map_onto_domain_states() {
        // Exact strings verified against the published spec's own response example
        // (`SubNumberOrderRequirementGroupResponse`): "requirement-info-pending",
        // "requirement-info-under-review", "requirement-info-exception".
        for (raw, expected) in [
            ("requirement-info-pending", RequirementsStatus::InfoPending),
            (
                "requirement-info-under-review",
                RequirementsStatus::UnderReview,
            ),
            (
                "requirement-info-exception",
                RequirementsStatus::Exception { reason: None },
            ),
        ] {
            let data = json!({
                "status": "pending",
                "requirements_met": false,
                "phone_numbers": [{ "requirements_status": raw }]
            });
            assert_eq!(parse_sub_order_state(&data).requirements, expected, "{raw}");
        }
    }

    #[test]
    fn a_missing_phone_numbers_array_is_unknown_rather_than_a_panic() {
        // `phone_numbers` is only present when `filter[include_phone_numbers]=true` is
        // sent; a response without it must degrade, never crash the sweep.
        let data = json!({ "status": "pending", "requirements_met": false });
        assert_eq!(
            parse_sub_order_state(&data).requirements,
            RequirementsStatus::Unknown
        );
    }

    // ---- purchase order/sub-order id parsing (spec 0119 S3) -------------------

    #[test]
    fn the_order_and_sub_order_ids_come_from_the_verified_top_level_fields() {
        // `NumberOrderWithPhoneNumbers` (verified against the published spec): `id` is the
        // order id, `sub_number_orders_ids` is an array of sub-order ids. The embedded
        // `PhoneNumber` schema — checked directly — carries no `sub_number_order_id` field
        // at all, so that is NOT the primary source design's original guess assumed.
        let data = json!({
            "id": "order-1",
            "sub_number_orders_ids": ["sub-1", "sub-2"],
            "phone_numbers": [{ "id": "pn-1", "phone_number": "+33612345678" }]
        });
        let order_ref = parse_order_ref(&data).expect("both ids present");
        assert_eq!(order_ref.order_id, "order-1");
        assert_eq!(order_ref.sub_order_id, SubOrderId("sub-1".into()));
    }

    #[test]
    fn a_per_phone_number_sub_order_id_is_a_defensive_fallback_only() {
        // Kept in case a differently-shaped response ever carries it, even though the
        // current published schema does not — cheap tolerance, never the primary path.
        let data = json!({
            "id": "order-2",
            "phone_numbers": [{ "id": "pn-1", "sub_number_order_id": "sub-9" }]
        });
        assert_eq!(
            parse_order_ref(&data).unwrap().sub_order_id,
            SubOrderId("sub-9".into())
        );
    }

    #[test]
    fn a_response_with_no_order_id_or_no_sub_order_id_yields_no_order_ref() {
        // Never a fabricated id: `PurchasedNumber.order` documents `None` as "genuinely no
        // order/sub-order concept", and a malformed/partial response must present the same
        // honest gap rather than half an `OrderRef`.
        assert!(parse_order_ref(&json!({"sub_number_orders_ids": ["s"]})).is_none());
        assert!(parse_order_ref(&json!({"id": "order-3"})).is_none());
    }

    fn requirement_query() -> RequirementQuery {
        RequirementQuery {
            country: "fr".into(),
            kind: NumberKind::Mobile,
            action: super::super::RequirementAction::Ordering,
        }
    }
}
