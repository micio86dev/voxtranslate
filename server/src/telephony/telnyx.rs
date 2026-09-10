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
    CallLeg, Cdr, DialRequest, GatherConfig, LegId, MediaStreamConfig, MediaTrack, PlayRequest,
    ProviderCapabilities, ProviderError, ProviderEvent, ProviderEventKind, ProviderMetadata,
    RecordingConfig, TelephonyProvider, WebhookError, WebhookHeaders,
};
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
        classify(res.status()).map_or(Ok(()), Err)
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
        // Anchorsite is what actually decides where media is handled. Sent per call rather
        // than relying on the connection's default, so a call carries its own residency
        // decision and the recorded `provider_region` is the truth rather than a guess.
        "media_encryption": "SRTP",
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

/// Normalise a verified body into a domain event.
fn normalise(body: &[u8]) -> Result<ProviderEvent, WebhookError> {
    let env: TelnyxEnvelope =
        serde_json::from_slice(body).map_err(|e| WebhookError::Malformed {
            detail: e.to_string(),
        })?;

    let leg = env
        .data
        .payload
        .call_control_id
        .clone()
        .ok_or_else(|| WebhookError::Malformed {
            detail: "event has no call_control_id".into(),
        })?;

    let kind = match env.data.event_type.as_str() {
        "call.initiated" => ProviderEventKind::Initiated,
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

/// Parse the public pricing response into rate rows.
///
/// Written defensively on purpose. The response shape is not something to guess at, and
/// the consequence of getting it wrong is the safe one: rows we cannot read simply do not
/// become rates, an unknown destination has no rate, and a call with no rate is **refused**
/// rather than dialed at a made-up price (R5). A sync that produces nothing is visible
/// immediately — every call stops — instead of quietly mispricing.
fn parse_rate_deck(body: &Value, fetched_at: DateTime<Utc>) -> Vec<Rate> {
    let rows = match body.get("data") {
        Some(Value::Array(rows)) => rows,
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    for row in rows {
        let Some(prefix) = row
            .get("prefix")
            .or_else(|| row.get("destination_prefix"))
            .and_then(Value::as_str)
            .map(|s| s.trim().trim_start_matches('+').to_string())
            .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
        else {
            continue;
        };
        let Some(cost) = row
            .get("cost_per_minute")
            .or_else(|| row.get("rate"))
            .or_else(|| row.get("price"))
            .and_then(numeric)
        else {
            continue;
        };
        out.push(Rate {
            prefix,
            cost_per_minute: cost,
            description: row
                .get("description")
                .or_else(|| row.get("destination"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            fetched_at,
        });
    }
    out
}

/// A money value that may arrive as a JSON number or as a string. Parsed through `Decimal`
/// in both cases — never through `f64` — so a rate deck cannot be the place binary error
/// enters the billing path.
fn numeric(v: &Value) -> Option<rust_decimal::Decimal> {
    match v {
        Value::String(s) => s.trim().parse().ok(),
        Value::Number(n) => n.to_string().parse().ok(),
        _ => None,
    }
}

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

        if let Some(err) = classify(res.status()) {
            return Err(err);
        }

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
        classify(res.status()).map_or(Ok(()), Err)
    }

    async fn stop_recording(&self, leg: &LegId) -> Result<(), ProviderError> {
        self.action(leg, "record_stop", json!({})).await
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

    async fn fetch_rate_deck(&self) -> Result<Vec<Rate>, ProviderError> {
        let res = self
            .http
            .get(self.url("/v2/public/pricing"))
            .query(&[("primitive", "voice")])
            .bearer_auth(&self.cfg.api_key)
            .send()
            .await
            .map_err(|e| ProviderError::Unavailable {
                detail: e.to_string(),
            })?;
        if let Some(err) = classify(res.status()) {
            return Err(err);
        }
        let body: Value = res.json().await.map_err(|e| ProviderError::Malformed {
            detail: e.to_string(),
        })?;
        Ok(parse_rate_deck(&body, Utc::now()))
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
    fn a_dial_carries_an_idempotency_key_so_a_retry_is_not_a_second_paid_call() {
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
        assert_eq!(body["media_encryption"], "SRTP");

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

    // ---- rate deck ------------------------------------------------------------

    #[test]
    fn rate_rows_parse_through_decimal_never_through_a_float() {
        let now = Utc::now();
        let body = json!({ "data": [
            { "prefix": "+39", "cost_per_minute": "0.0123", "description": "Italy" },
            { "destination_prefix": "8613", "rate": 0.045, "destination": "China Mobile" },
        ]});
        let rates = parse_rate_deck(&body, now);
        assert_eq!(rates.len(), 2);
        assert_eq!(
            rates[0].prefix, "39",
            "the leading + is stripped for matching"
        );
        assert_eq!(rates[0].cost_per_minute, "0.0123".parse().unwrap());
        assert_eq!(rates[1].prefix, "8613");
        assert_eq!(rates[1].cost_per_minute, "0.045".parse().unwrap());
        assert_eq!(rates[1].description, "China Mobile");
    }

    #[test]
    fn unreadable_rows_are_dropped_and_that_fails_the_call_not_the_price() {
        // The safe direction: a row we cannot read does not become a rate, an unknown
        // destination has no rate, and a call with no rate is REFUSED. A sync that yields
        // nothing stops every call immediately instead of quietly mispricing them.
        let now = Utc::now();
        let body = json!({ "data": [
            { "prefix": "39" },                                  // no price
            { "cost_per_minute": "0.01" },                        // no prefix
            { "prefix": "not-digits", "cost_per_minute": "0.01" },
            { "prefix": "", "cost_per_minute": "0.01" },
            { "prefix": "44", "cost_per_minute": "nonsense" },
        ]});
        assert!(parse_rate_deck(&body, now).is_empty());

        // A response shaped nothing like the expected one yields nothing, rather than
        // panicking or inventing rows.
        assert!(parse_rate_deck(&json!({"error": "nope"}), now).is_empty());
        assert!(parse_rate_deck(&json!([]), now).is_empty());
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
}
