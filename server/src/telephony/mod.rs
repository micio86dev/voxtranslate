//! Telephony provider abstraction (spec 0111, D1).
//!
//! **Provider ids and payloads stop here.** Nothing downstream — orchestration, billing,
//! dashboard, analytics, i18n — may see a Telnyx string, a Telnyx status or a Telnyx URL.
//! The proof that the boundary holds is not a rule anyone has to remember: it is that
//! [`mock::MockTelephonyProvider`] is the provider every automated test runs against, so
//! a leak breaks the build rather than being noticed in review.
//!
//! The trait is deliberately one cohesive interface rather than six small ones. A
//! telephony API *is* one thing: legs, media, recording and rating are the same account
//! and the same session. Splitting it would produce six traits always implemented
//! together and always used together — abstraction with no seam behind it.

pub mod e164;
pub mod mock;
pub mod telnyx;

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

pub use e164::{E164Error, E164};

use crate::voip::pricing::Rate;
use crate::voip::state::FailureReason;

/// Opaque provider-side identifier for one call leg.
///
/// A newtype, not a `String`, so a leg id cannot be passed where a room code, a call id or
/// a recording id is expected. It is opaque on purpose: the value's *shape* is the
/// provider's business, and code outside `telephony::` may only store it and hand it back.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LegId(String);

impl LegId {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LegId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a provider can do. Read by the orchestrator instead of `if provider == "telnyx"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderCapabilities {
    /// Audio can be streamed to us AND written back on the same leg. Without this there
    /// is no translated call at all, only a recorded one.
    pub bidirectional_media: bool,
    /// How many concurrent media streams one leg accepts.
    ///
    /// Telnyx allows exactly **one**, which is why the two legs of a translated call are
    /// never bridged (D2). Carried as a number rather than assumed, so a provider that
    /// allows more is not artificially constrained and one that allows fewer is caught at
    /// startup instead of at 3 a.m.
    pub max_streams_per_leg: u8,
    pub recording: bool,
    /// The provider can speak a sentence in a given language on a leg.
    ///
    /// This is what the consent announcement is played with. It is a capability rather
    /// than an assumption because the disclosure is a legal obligation: a provider that
    /// cannot speak means capture must not start, and that decision has to be made from a
    /// fact rather than from an API call that quietly fails.
    pub speech_synthesis: bool,
    pub dtmf_gather: bool,
    pub inbound: bool,
    /// The provider exposes a machine-readable rate deck (R5 depends on one existing).
    pub rate_deck: bool,
}

/// Static description of a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderMetadata {
    /// Stable id, persisted in `voip_calls.provider` and in `voip_provider_events`. Must
    /// never change once shipped.
    pub id: &'static str,
    pub display_name: &'static str,
    /// Where this provider is configured to anchor signalling, media and storage, e.g.
    /// `"eu-fra"`. Free-form because it is the provider's vocabulary — but it is recorded
    /// on every call so a data-flow claim can be audited after the fact.
    pub region: String,
    /// Whether THIS configuration keeps telephony and media inside the EU.
    ///
    /// Only ever about the telephony leg. It says nothing about where translation
    /// happens, and the EU gate (R22) must therefore check the tier as well — see
    /// `docs/voip-data-flow.md`. Conflating the two is exactly the mistake the GDPR
    /// readiness review found already being made about the product as a whole.
    pub eu_telephony: bool,
    /// The number this account presents when the organization has none of its own.
    ///
    /// Lives here rather than being read from a provider-specific env var deeper in the
    /// stack: `TELNYX_DEFAULT_CALLER_ID` is a Telnyx name, and provider names stop at this
    /// boundary. `None` means the deployment configured none, and a call without an
    /// org-owned verified number is then refused rather than placed anonymously.
    pub default_caller_id: Option<String>,
    pub capabilities: ProviderCapabilities,
}

/// Where a leg's audio is sent, and in what shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaStreamConfig {
    /// Our WebSocket endpoint. Built from `VOIP_MEDIA_WS_BASE` plus a single-use token,
    /// never from anything a client supplied — an attacker-chosen `stream_url` would be
    /// an SSRF with the provider as the confused deputy.
    pub url: String,
    /// Wire codec. `L16` avoids µ-law quantisation on our side of the call.
    pub codec: MediaCodec,
    /// Which direction(s) of the leg to send us.
    pub track: MediaTrack,
    /// Send audio back on the same socket. Required for a translated call.
    pub bidirectional: bool,
}

/// Codecs a provider may stream in. Named after the wire formats rather than after any
/// one provider's spelling of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaCodec {
    /// G.711 µ-law, 8 kHz. The universal PSTN fallback.
    Pcmu,
    /// G.711 A-law, 8 kHz.
    Pcma,
    /// G.722 wideband, 8 kHz frame clock.
    G722,
    /// Opus, 8 or 16 kHz.
    Opus,
    /// Linear PCM 16-bit, 16 kHz. Preferred: our engines want linear PCM anyway, so this
    /// is the only option with no quantisation step on the way in.
    L16,
}

impl MediaCodec {
    /// Sample rate of the wire format, in Hz.
    pub fn sample_rate(self) -> u32 {
        match self {
            Self::Pcmu | Self::Pcma | Self::G722 => 8_000,
            Self::Opus | Self::L16 => 16_000,
        }
    }

    /// Whether samples arrive as linear PCM, i.e. need no companding step.
    pub fn is_linear(self) -> bool {
        matches!(self, Self::L16)
    }

    /// Stable persisted name, recorded per call for quality diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pcmu => "PCMU",
            Self::Pcma => "PCMA",
            Self::G722 => "G722",
            Self::Opus => "OPUS",
            Self::L16 => "L16",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaTrack {
    /// What the far party says.
    Inbound,
    /// What we play to them.
    Outbound,
    Both,
}

/// A request to place an outbound call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialRequest {
    pub to: E164,
    /// Presented caller id. Must be a number the account owns or has verified — the
    /// provider enforces this and so do we, because arbitrary caller-id spoofing is
    /// illegal in most of our markets.
    pub from: E164,
    /// Correlation id carried by every webhook for this leg, so an event can be matched
    /// to a call without trusting the provider's own id mapping.
    pub client_state: String,
    /// Seconds to ring before giving up.
    pub timeout_secs: u32,
    /// Provider-side region hint (anchorsite). Recorded on the call.
    pub region: String,
}

/// A leg the provider has accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallLeg {
    pub id: LegId,
    /// The provider's own session/correlation id, when it has one distinct from the leg.
    pub session_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Audio to play into a leg — announcements, consent prompts, tones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayRequest {
    /// Pre-rendered audio we already hold, base64-encoded in the provider's expected
    /// container. Used for the consent announcement, which we synthesise ourselves so it
    /// is in the recipient's language and says exactly what is true.
    Audio { payload_b64: String },
    /// A provider-hosted or publicly reachable media URL.
    Url { url: String },
    /// Have the provider speak this sentence.
    ///
    /// Used for the consent announcement. The provider's own synthesis is preferred over
    /// rendering audio ourselves because it needs no second vendor, no audio format
    /// negotiation and no cache to go stale — and the announcement is a fixed sentence,
    /// not a conversation, so the quality bar is "intelligible in the right language".
    Speak {
        text: String,
        /// The RECIPIENT's language, not the caller's.
        language: String,
        /// Provider voice id. `None` lets the provider pick for the language.
        voice: Option<String>,
    },
}

/// Collect DTMF from the far party — the consent gate (R20).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatherConfig {
    /// Digits that count as an answer.
    pub valid_digits: String,
    pub max_digits: u8,
    pub timeout_secs: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingConfig {
    /// Record both directions as separate channels where supported, so a transcript can
    /// attribute a line to a speaker without diarisation.
    pub dual_channel: bool,
    /// Play the provider's recording beep as an ADDITIONAL signal. Never a substitute for
    /// the spoken announcement (D7).
    pub beep: bool,
}

/// Authoritative post-call billing record from the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cdr {
    pub leg_id: LegId,
    /// Billable seconds as the PROVIDER counted them, which is what we are actually
    /// invoiced for and need not match our own clock.
    pub billed_seconds: u64,
    /// What this leg cost us, total, in USD.
    pub cost: Decimal,
    /// Negotiated codec, for the quality record.
    pub codec: Option<MediaCodec>,
    /// Carrier hangup cause, already mapped.
    pub hangup_cause: Option<FailureReason>,
}

/// A provider webhook, normalised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderEvent {
    pub provider: &'static str,
    /// The provider's unique id for THIS delivery. The idempotency key (R25): stored
    /// under a UNIQUE constraint so a redelivery is rejected by the database rather than
    /// by application logic that can be raced.
    pub event_id: String,
    pub leg_id: LegId,
    /// Our correlation id, echoed back.
    pub client_state: Option<String>,
    pub occurred_at: DateTime<Utc>,
    pub kind: ProviderEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderEventKind {
    Initiated,
    /// Somebody is calling US (spec 0116).
    ///
    /// Deliberately NOT `Initiated`. That one means "the dial we asked for has started",
    /// and overloading one kind for both directions would make every branch downstream ask
    /// "but which way round?" — which is exactly the question a type should answer.
    Incoming {
        /// The caller, as the carrier reports it.
        from: String,
        /// The number of ours that was rung.
        to: String,
    },
    Ringing,
    Answered,
    Hangup {
        cause: FailureReason,
    },
    MediaStarted,
    MediaStopped,
    RecordingStarted,
    RecordingSaved {
        /// Where the carrier put it. Operational and reconciliation value only — never
        /// served to a client, because on some carriers this URL is publicly fetchable.
        url: String,
        /// The carrier's **durable** handle on the bytes.
        ///
        /// This is what deletion uses. A URL is not a handle: it expires on some
        /// providers and is guessable on others, and neither property belongs in an
        /// erasure path. `None` when the provider does not give us one, which is a fact
        /// worth carrying rather than papering over — an un-deletable recording must be
        /// visible as such.
        recording_id: Option<String>,
        duration_secs: u64,
    },
    Dtmf {
        digit: char,
    },
    /// An event type we do not model. Recorded (so the ledger is complete and the
    /// redelivery is absorbed) but it moves nothing. Kept as a variant rather than
    /// dropped, because silently discarding an unknown event is how you find out later
    /// that the provider renamed the one that mattered.
    Unhandled {
        raw_type: String,
    },
}

/// Why a provider call failed. Separate from [`FailureReason`], which describes the
/// *call*; this describes the *API interaction*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    /// Credentials rejected.
    Unauthorized,
    /// The provider account has no balance / is suspended.
    AccountBlocked,
    /// The provider rate-limited us.
    RateLimited,
    /// The destination was refused by the provider (not permitted on this account, or an
    /// invalid number the provider caught before we did).
    DestinationRefused,
    /// Network, timeout, 5xx.
    Unavailable { detail: String },
    /// The provider replied with something we cannot parse. Distinguished from
    /// `Unavailable` so a contract drift shows up as a contract problem.
    Malformed { detail: String },
    /// The requested operation is not supported by this provider's capabilities.
    Unsupported { operation: &'static str },
}

impl ProviderError {
    /// How the call should be recorded when the provider fails this way.
    pub fn as_failure_reason(&self) -> FailureReason {
        match self {
            Self::DestinationRefused => FailureReason::DestinationNotAllowed,
            _ => FailureReason::ProviderUnavailable,
        }
    }

    /// Whether retrying the same request could plausibly succeed. A dial is NOT retried
    /// automatically even when this is true — an automatic retry on a paid call is how a
    /// transient blip becomes two calls and two invoices.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::RateLimited | Self::Unavailable { .. })
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unauthorized => f.write_str("provider rejected our credentials"),
            Self::AccountBlocked => f.write_str("provider account blocked or out of balance"),
            Self::RateLimited => f.write_str("provider rate limited"),
            Self::DestinationRefused => f.write_str("provider refused the destination"),
            Self::Unavailable { detail } => write!(f, "provider unavailable: {detail}"),
            Self::Malformed { detail } => {
                write!(f, "provider sent something unparseable: {detail}")
            }
            Self::Unsupported { operation } => write!(f, "provider cannot {operation}"),
        }
    }
}

/// Why a webhook was not applied.
///
/// The first four mean the webhook was **refused** and no state changed (R24). [`Internal`]
/// is different in kind and the distinction is load-bearing: it means we could not process
/// a webhook that may well be valid, so the provider must be told to **retry**. Collapsing
/// it into a refusal tells the provider "this will never verify", and a transient database
/// blip during a hangup webhook then loses that event permanently — leaving a call stuck
/// non-terminal with the customer's credits held.
///
/// [`Internal`]: WebhookError::Internal
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebhookError {
    /// No signature header at all.
    MissingSignature,
    /// Signature present but not valid for this body and key.
    BadSignature,
    /// Timestamp outside the tolerance window — a replay, or a clock so far out that we
    /// cannot tell the difference.
    StaleTimestamp,
    /// Body is not the shape we expect.
    Malformed { detail: String },
    /// We failed to process it — storage, not signature. **Retryable.**
    Internal { detail: String },
}

impl WebhookError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::MissingSignature => "missing_signature",
            Self::BadSignature => "bad_signature",
            Self::StaleTimestamp => "stale_timestamp",
            Self::Malformed { .. } => "malformed",
            Self::Internal { .. } => "internal_error",
        }
    }

    /// Whether the provider should try again.
    ///
    /// Only an internal failure is retryable. A bad signature never becomes a good one,
    /// and telling the provider to retry it burns their queue and our rate limit for hours.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Internal { .. })
    }
}

/// The headers a webhook verification needs, lifted out of any HTTP framework so the
/// verification is a pure function that can be tested without a server.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WebhookHeaders {
    pub signature: Option<String>,
    pub timestamp: Option<String>,
}

// ---- number management (spec 0115) ----------------------------------------

/// The provider's own id for a number we own. Opaque here on purpose: releasing or
/// inspecting a number must not need a second lookup, and the shape of the id is the
/// provider's business.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderNumberId(pub String);

impl ProviderNumberId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What kind of number, in the terms a customer thinks in rather than a carrier's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumberKind {
    Local,
    National,
    TollFree,
    Mobile,
}

impl NumberKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::National => "national",
            Self::TollFree => "toll_free",
            Self::Mobile => "mobile",
        }
    }

    /// Unknown input becomes `Local`, the commonest and least surprising kind — the same
    /// fail-to-the-narrow-case habit `RolloutStage::parse` follows.
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "national" => Self::National,
            "toll_free" | "tollfree" | "toll-free" => Self::TollFree,
            "mobile" => Self::Mobile,
            _ => Self::Local,
        }
    }
}

/// A search, in human terms: a country, optionally a place, optionally a kind.
#[derive(Debug, Clone)]
pub struct NumberSearch {
    /// ISO 3166-1 alpha-2.
    pub country: String,
    /// Area or city code, where the country has them and the provider supports filtering.
    pub area_code: Option<String>,
    pub kind: Option<NumberKind>,
    pub limit: u8,
}

/// One number the provider is offering, with what IT costs us — never what we charge.
///
/// The customer price is computed by `voip::pricing::NumberMarkupPolicy` at the edge, so
/// the markup lives in one place and a provider adapter cannot accidentally apply it.
#[derive(Debug, Clone)]
pub struct NumberOffer {
    pub e164: String,
    pub country: String,
    pub kind: NumberKind,
    pub monthly_cost: Decimal,
    pub setup_cost: Decimal,
    pub currency: String,
    /// What a regulator wants before this number can carry traffic, in the provider's
    /// words. `None` means nothing is required — not "we did not ask".
    pub regulatory_requirement: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PurchaseRequest {
    pub e164: String,
    /// Ours, not the provider's: the same key must buy the same number once, however many
    /// times a flaky network makes the client retry.
    pub idempotency_key: String,
}

/// Where a number is in its life. Mirrors what a customer needs to know, not a carrier's
/// internal state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumberStatus {
    Ordering,
    /// A regulator is the blocker. Saying "active" here would be a lie with a fine
    /// attached.
    PendingRegulatory,
    Active,
    Suspended,
    Releasing,
    Released,
    Failed,
}

impl NumberStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ordering => "ordering",
            Self::PendingRegulatory => "pending_regulatory",
            Self::Active => "active",
            Self::Suspended => "suspended",
            Self::Releasing => "releasing",
            Self::Released => "released",
            Self::Failed => "failed",
        }
    }

    /// Unknown provider state becomes `Ordering`, never `Active`: a number we cannot read
    /// the status of must not be presented as caller id.
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "pending_regulatory" | "pending-regulatory" | "pending" => Self::PendingRegulatory,
            "active" => Self::Active,
            "suspended" => Self::Suspended,
            "releasing" => Self::Releasing,
            "released" | "deleted" => Self::Released,
            "failed" => Self::Failed,
            _ => Self::Ordering,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PurchasedNumber {
    pub provider_number_id: ProviderNumberId,
    pub e164: String,
    pub status: NumberStatus,
    pub monthly_cost: Decimal,
    pub setup_cost: Decimal,
    pub currency: String,
    pub regulatory_requirement: Option<String>,
}

/// How the provider proves the caller owns a number they already have elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationMethod {
    /// We call the number and read a code.
    Call,
    Sms,
}

#[derive(Debug, Clone)]
pub struct VerificationStart {
    pub id: String,
    pub method: VerificationMethod,
    /// The code the person will hear or read, when the provider tells us what it is.
    /// `None` when the provider keeps it to itself and only reports the outcome.
    pub code: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationState {
    Pending,
    Verified,
    Rejected,
}

impl VerificationState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Verified => "verified",
            Self::Rejected => "rejected",
        }
    }

    /// Anything unrecognised is `Pending`, never `Verified`. Presenting a number we have
    /// not proved the customer owns is illegal in most of our markets, so the failure
    /// direction here is not a style choice.
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "verified" | "success" | "succeeded" => Self::Verified,
            "rejected" | "failed" | "expired" => Self::Rejected,
            _ => Self::Pending,
        }
    }
}

/// One telephony provider.
#[async_trait]
pub trait TelephonyProvider: Send + Sync {
    fn metadata(&self) -> &ProviderMetadata;

    /// Place an outbound call. Returns as soon as the provider accepts it — `answered`
    /// arrives later, as a webhook.
    async fn dial(&self, req: DialRequest) -> Result<CallLeg, ProviderError>;

    /// Answer an inbound leg (Mode 2).
    async fn answer(&self, leg: &LegId) -> Result<(), ProviderError>;

    /// End a leg. Idempotent by contract: hanging up an already-dead leg is `Ok`, because
    /// teardown races the far party hanging up and losing that race must not surface as an
    /// error the operator has to interpret.
    async fn hangup(&self, leg: &LegId) -> Result<(), ProviderError>;

    async fn start_media_stream(
        &self,
        leg: &LegId,
        cfg: MediaStreamConfig,
    ) -> Result<(), ProviderError>;

    async fn stop_media_stream(&self, leg: &LegId) -> Result<(), ProviderError>;

    /// Play audio into a leg (the consent announcement).
    async fn play(&self, leg: &LegId, req: PlayRequest) -> Result<(), ProviderError>;

    /// Ask for DTMF. Digits arrive as [`ProviderEventKind::Dtmf`] webhooks.
    async fn gather(&self, leg: &LegId, cfg: GatherConfig) -> Result<(), ProviderError>;

    async fn start_recording(&self, leg: &LegId, cfg: RecordingConfig)
        -> Result<(), ProviderError>;

    async fn stop_recording(&self, leg: &LegId) -> Result<(), ProviderError>;

    /// Delete a recording from the provider's storage, permanently.
    ///
    /// Called by GDPR erasure, which is why it takes the recording id rather than the leg:
    /// by the time erasure runs the call is long over, and the leg id names a call, not a
    /// file. A provider that cannot delete must return an error rather than `Ok(())` —
    /// erasure aborts on failure and stays retryable, and a silent success would leave the
    /// bytes in place while telling the person they were erased.
    async fn delete_recording(&self, recording_id: &str) -> Result<(), ProviderError>;

    /// Authoritative cost for a finished leg (R12). `None` while the provider has not
    /// rated it yet — which is normal for a minute or two after hangup.
    async fn fetch_cdr(&self, leg: &LegId) -> Result<Option<Cdr>, ProviderError>;

    /// Pull the current rate deck (D11).
    async fn fetch_rate_deck(&self) -> Result<Vec<Rate>, ProviderError>;

    // ---- number management (spec 0115) ------------------------------------
    //
    // Every one of these may answer `ProviderError::Unsupported`, the way `fetch_cdr` and
    // `fetch_rate_deck` already do. An account that cannot buy numbers, or an API that
    // does not expose the operation, is a fact to report — not something to paper over
    // with a fabricated success.

    /// Numbers available to buy, in the terms a customer searched in.
    async fn search_numbers(&self, q: NumberSearch) -> Result<Vec<NumberOffer>, ProviderError>;

    /// Buy one. `req.idempotency_key` is OURS: the same key must buy the same number once,
    /// however many times a flaky network makes the client retry.
    async fn purchase_number(&self, req: PurchaseRequest)
        -> Result<PurchasedNumber, ProviderError>;

    /// Give one back. Irreversible from the customer's point of view — somebody else may
    /// hold that number an hour later — so the product asks twice before calling this.
    async fn release_number(&self, id: &ProviderNumberId) -> Result<(), ProviderError>;

    /// Where the provider thinks a number is, for the reconcile sweep to compare against
    /// what we think.
    async fn number_status(&self, id: &ProviderNumberId) -> Result<NumberStatus, ProviderError>;

    /// Begin proving that the customer owns a number they hold somewhere else, so it can
    /// be presented as caller id.
    async fn start_caller_id_verification(
        &self,
        e164: &E164,
    ) -> Result<VerificationStart, ProviderError>;

    /// Has it succeeded yet?
    async fn check_caller_id_verification(
        &self,
        id: &str,
    ) -> Result<VerificationState, ProviderError>;

    /// Verify and normalise a webhook. **Pure** — no I/O, no clock of its own — so R24 is
    /// testable exhaustively without a network or a running server.
    fn verify_webhook(
        &self,
        headers: &WebhookHeaders,
        body: &[u8],
        now: DateTime<Utc>,
    ) -> Result<ProviderEvent, WebhookError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_sample_rates_and_linearity_are_stated_not_assumed() {
        assert_eq!(MediaCodec::L16.sample_rate(), 16_000);
        assert_eq!(MediaCodec::Pcmu.sample_rate(), 8_000);
        assert_eq!(MediaCodec::Opus.sample_rate(), 16_000);
        assert!(MediaCodec::L16.is_linear());
        // Everything else needs a companding or decode step before it reaches an engine.
        for c in [
            MediaCodec::Pcmu,
            MediaCodec::Pcma,
            MediaCodec::G722,
            MediaCodec::Opus,
        ] {
            assert!(!c.is_linear(), "{c:?}");
        }
    }

    #[test]
    fn codec_names_are_unique_and_stable() {
        let all = [
            MediaCodec::Pcmu,
            MediaCodec::Pcma,
            MediaCodec::G722,
            MediaCodec::Opus,
            MediaCodec::L16,
        ];
        let names: std::collections::HashSet<&str> = all.iter().map(|c| c.as_str()).collect();
        assert_eq!(names.len(), all.len());
    }

    #[test]
    fn a_leg_id_is_opaque_and_cannot_be_confused_with_another_identifier() {
        let leg = LegId::new("v3:abc");
        assert_eq!(leg.as_str(), "v3:abc");
        assert_eq!(leg.to_string(), "v3:abc");
        assert_eq!(leg, LegId::new("v3:abc"));
        assert_ne!(leg, LegId::new("v3:abd"));
    }

    #[test]
    fn provider_errors_map_onto_call_outcomes_conservatively() {
        // Anything we are unsure about is the provider's fault, not the destination's:
        // blaming the destination would poison an org's allow-list decisions.
        assert_eq!(
            ProviderError::DestinationRefused.as_failure_reason(),
            FailureReason::DestinationNotAllowed
        );
        for e in [
            ProviderError::Unauthorized,
            ProviderError::AccountBlocked,
            ProviderError::RateLimited,
            ProviderError::Unavailable {
                detail: "504".into(),
            },
            ProviderError::Malformed {
                detail: "html".into(),
            },
            ProviderError::Unsupported { operation: "video" },
        ] {
            assert_eq!(e.as_failure_reason(), FailureReason::ProviderUnavailable);
        }
    }

    #[test]
    fn only_network_shaped_failures_are_transient() {
        assert!(ProviderError::RateLimited.is_transient());
        assert!(ProviderError::Unavailable {
            detail: String::new()
        }
        .is_transient());
        // Credentials, balance and a refused destination will not fix themselves, and
        // retrying them burns rate limit for nothing.
        assert!(!ProviderError::Unauthorized.is_transient());
        assert!(!ProviderError::AccountBlocked.is_transient());
        assert!(!ProviderError::DestinationRefused.is_transient());
        assert!(!ProviderError::Malformed {
            detail: String::new()
        }
        .is_transient());
    }

    #[test]
    fn webhook_rejection_codes_are_unique() {
        let all = [
            WebhookError::MissingSignature,
            WebhookError::BadSignature,
            WebhookError::StaleTimestamp,
            WebhookError::Malformed {
                detail: String::new(),
            },
        ];
        let codes: std::collections::HashSet<&str> = all.iter().map(|e| e.code()).collect();
        assert_eq!(codes.len(), all.len());
    }
}
