//! In-process telephony provider for tests (spec 0111, "PROVIDER MOCKING").
//!
//! Every automated test in the suite runs against this. That is the design: a mock that is
//! only reached by a handful of tests documents nothing, while a mock that *is* the test
//! provider turns "no provider detail leaks out of `telephony::`" from a review comment
//! into a compile error.
//!
//! It is a faithful mock, not a stub. In particular it enforces the constraint the whole
//! architecture rests on — **one media stream per leg** (D2) — so orchestration code that
//! tries to bridge-and-fork fails here rather than in production at 3 a.m. It also verifies
//! webhook signatures with the same *shape* Telnyx uses (`{timestamp}|{body}`, a tolerance
//! window) so the webhook handler's tests exercise real branches; only the primitive
//! differs, HMAC-SHA256 instead of Ed25519, because a test should not need a keypair.
//!
//! No paid call can originate here. There is no network code in this file at all.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use super::{
    CallLeg, Cdr, DialRequest, GatherConfig, LegId, MediaStreamConfig, PlayRequest,
    ProviderCapabilities, ProviderError, ProviderEvent, ProviderEventKind, ProviderMetadata,
    RecordingConfig, TelephonyProvider, WebhookError, WebhookHeaders,
};
use crate::voip::pricing::Rate;
use crate::voip::state::FailureReason;

/// Stable id, persisted like any other provider so a test fixture is distinguishable from
/// real traffic in the same table.
pub const MOCK_ID: &str = "mock";

/// Everything the orchestrator asked the provider to do, in order.
///
/// Ordered rather than counted: "did we start recording *before* the consent gate closed"
/// is the kind of question a privacy test has to be able to ask, and a set of counters
/// cannot answer it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockCommand {
    Dial(Box<DialRequest>),
    Answer(LegId),
    Hangup(LegId),
    StartMedia(LegId, Box<MediaStreamConfig>),
    StopMedia(LegId),
    Play(LegId, PlayRequest),
    Gather(LegId, GatherConfig),
    StartRecording(LegId, RecordingConfig),
    StopRecording(LegId),
}

#[derive(Debug, Default)]
struct MockState {
    commands: Vec<MockCommand>,
    live_legs: Vec<LegId>,
    streaming_legs: Vec<LegId>,
    recording_legs: Vec<LegId>,
    /// Queued outcomes for the next N dials. Popped from the front, so a test can script
    /// "first call fails, second succeeds" without a callback.
    dial_outcomes: Vec<Result<(), ProviderError>>,
    cdrs: HashMap<String, Cdr>,
    rate_deck: Vec<Rate>,
    rate_deck_error: Option<ProviderError>,
    seq: u64,
}

pub struct MockTelephonyProvider {
    metadata: ProviderMetadata,
    secret: Vec<u8>,
    tolerance: Duration,
    state: Mutex<MockState>,
}

impl Default for MockTelephonyProvider {
    fn default() -> Self {
        Self::new(b"mock-webhook-secret", Duration::minutes(5))
    }
}

impl MockTelephonyProvider {
    pub fn new(secret: &[u8], tolerance: Duration) -> Self {
        Self {
            metadata: ProviderMetadata {
                id: MOCK_ID,
                display_name: "Mock",
                region: "test".into(),
                // A mock keeps nothing anywhere, so claiming EU telephony would let an
                // EU-gate test pass for the wrong reason.
                eu_telephony: false,
                // A mock owns no number. A call with no org-owned verified number is then
                // refused rather than placed anonymously, which is the behaviour the
                // tenancy tests want to see.
                default_caller_id: None,
                capabilities: ProviderCapabilities {
                    bidirectional_media: true,
                    // The constraint the architecture depends on, reproduced faithfully.
                    max_streams_per_leg: 1,
                    recording: true,
                    dtmf_gather: true,
                    inbound: true,
                    rate_deck: true,
                },
            },
            secret: secret.to_vec(),
            tolerance,
            state: Mutex::new(MockState::default()),
        }
    }

    /// Build a provider whose telephony leg claims to be in the EU, for gate tests that
    /// need the *tier* to be the thing that refuses.
    pub fn eu() -> Self {
        let mut p = Self::default();
        p.metadata.eu_telephony = true;
        p.metadata.region = "eu-fra".into();
        p
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MockState> {
        self.state.lock().expect("mock provider mutex poisoned")
    }

    // ---- scripting ---------------------------------------------------------

    /// Make the next dial fail. Queued, so several can be lined up in order.
    pub fn fail_next_dial(&self, err: ProviderError) {
        self.lock().dial_outcomes.push(Err(err));
    }

    pub fn set_cdr(&self, leg: &LegId, cdr: Cdr) {
        self.lock().cdrs.insert(leg.as_str().to_string(), cdr);
    }

    pub fn set_rate_deck(&self, rates: Vec<Rate>) {
        self.lock().rate_deck = rates;
    }

    pub fn fail_rate_deck(&self, err: ProviderError) {
        self.lock().rate_deck_error = Some(err);
    }

    // ---- inspection --------------------------------------------------------

    /// Everything asked of the provider, in order.
    pub fn commands(&self) -> Vec<MockCommand> {
        self.lock().commands.clone()
    }

    pub fn is_live(&self, leg: &LegId) -> bool {
        self.lock().live_legs.contains(leg)
    }

    pub fn is_streaming(&self, leg: &LegId) -> bool {
        self.lock().streaming_legs.contains(leg)
    }

    pub fn is_recording(&self, leg: &LegId) -> bool {
        self.lock().recording_legs.contains(leg)
    }

    // ---- webhook construction ---------------------------------------------

    /// Serialise an event the way this provider's webhooks look on the wire.
    pub fn body_for(&self, event: &MockWebhookBody) -> Vec<u8> {
        serde_json::to_vec(event).expect("mock webhook body serialises")
    }

    /// Sign a body the way the provider would, so a test can build a *valid* webhook and
    /// the invalid cases are then real deviations from it rather than made-up strings.
    pub fn sign(&self, body: &[u8], at: DateTime<Utc>) -> WebhookHeaders {
        let ts = at.timestamp().to_string();
        WebhookHeaders {
            signature: Some(self.signature_for(&ts, body)),
            timestamp: Some(ts),
        }
    }

    fn signature_for(&self, timestamp: &str, body: &[u8]) -> String {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.secret).expect("HMAC accepts any key length");
        mac.update(timestamp.as_bytes());
        mac.update(b"|");
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }
}

/// The wire shape of a mock webhook. Mirrors the *structure* of a real one — an id for
/// idempotency, a leg, our echoed correlation id, a timestamp and a type — without
/// borrowing any provider's field names.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MockWebhookBody {
    pub event_id: String,
    pub leg_id: String,
    #[serde(default)]
    pub client_state: Option<String>,
    pub occurred_at: DateTime<Utc>,
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub cause: Option<String>,
    #[serde(default)]
    pub digit: Option<char>,
    #[serde(default)]
    pub recording_url: Option<String>,
    #[serde(default)]
    pub duration_secs: Option<u64>,
}

impl MockWebhookBody {
    /// A minimal well-formed event.
    pub fn new(event_id: &str, leg: &LegId, event_type: &str, at: DateTime<Utc>) -> Self {
        Self {
            event_id: event_id.into(),
            leg_id: leg.as_str().into(),
            client_state: None,
            occurred_at: at,
            event_type: event_type.into(),
            cause: None,
            digit: None,
            recording_url: None,
            duration_secs: None,
        }
    }

    pub fn with_client_state(mut self, s: &str) -> Self {
        self.client_state = Some(s.into());
        self
    }

    pub fn with_cause(mut self, c: &str) -> Self {
        self.cause = Some(c.into());
        self
    }

    pub fn with_digit(mut self, d: char) -> Self {
        self.digit = Some(d);
        self
    }
}

/// Map a mock cause string onto a domain reason. Deliberately the same shape as the real
/// adapter's mapping, including the "anything unrecognised is `Unmapped`, not
/// `ProviderUnavailable`" rule — an unmapped cause is work to do, and bucketing it into a
/// reason that already means something hides it.
fn cause_to_reason(cause: Option<&str>) -> FailureReason {
    match cause {
        None => FailureReason::Unmapped,
        Some("busy") => FailureReason::Busy,
        Some("rejected") => FailureReason::Rejected,
        Some("no_answer") => FailureReason::NoAnswer,
        Some("unallocated") => FailureReason::Unallocated,
        Some("normal") => FailureReason::Unmapped,
        Some(_) => FailureReason::Unmapped,
    }
}

#[async_trait]
impl TelephonyProvider for MockTelephonyProvider {
    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    async fn dial(&self, req: DialRequest) -> Result<CallLeg, ProviderError> {
        let mut st = self.lock();
        st.commands.push(MockCommand::Dial(Box::new(req)));
        if !st.dial_outcomes.is_empty() {
            let outcome = st.dial_outcomes.remove(0);
            outcome?;
        }
        st.seq += 1;
        let leg = LegId::new(format!("mock-leg-{}", st.seq));
        st.live_legs.push(leg.clone());
        Ok(CallLeg {
            id: leg,
            session_id: Some(format!("mock-session-{}", st.seq)),
            created_at: Utc::now(),
        })
    }

    async fn answer(&self, leg: &LegId) -> Result<(), ProviderError> {
        let mut st = self.lock();
        st.commands.push(MockCommand::Answer(leg.clone()));
        if !st.live_legs.contains(leg) {
            st.live_legs.push(leg.clone());
        }
        Ok(())
    }

    async fn hangup(&self, leg: &LegId) -> Result<(), ProviderError> {
        let mut st = self.lock();
        st.commands.push(MockCommand::Hangup(leg.clone()));
        // Idempotent by contract: hanging up a leg the far party already dropped is not
        // an error, because teardown always races the far side.
        st.live_legs.retain(|l| l != leg);
        st.streaming_legs.retain(|l| l != leg);
        st.recording_legs.retain(|l| l != leg);
        Ok(())
    }

    async fn start_media_stream(
        &self,
        leg: &LegId,
        cfg: MediaStreamConfig,
    ) -> Result<(), ProviderError> {
        let mut st = self.lock();
        st.commands
            .push(MockCommand::StartMedia(leg.clone(), Box::new(cfg)));
        // D2, enforced: one stream per leg. Code that tries to fork a second stream — the
        // bridge-and-fork design this architecture rejected — fails here.
        let already = st.streaming_legs.iter().filter(|l| *l == leg).count();
        if already >= self.metadata.capabilities.max_streams_per_leg as usize {
            return Err(ProviderError::Unsupported {
                operation: "open a second media stream on one leg",
            });
        }
        st.streaming_legs.push(leg.clone());
        Ok(())
    }

    async fn stop_media_stream(&self, leg: &LegId) -> Result<(), ProviderError> {
        let mut st = self.lock();
        st.commands.push(MockCommand::StopMedia(leg.clone()));
        st.streaming_legs.retain(|l| l != leg);
        Ok(())
    }

    async fn play(&self, leg: &LegId, req: PlayRequest) -> Result<(), ProviderError> {
        self.lock()
            .commands
            .push(MockCommand::Play(leg.clone(), req));
        Ok(())
    }

    async fn gather(&self, leg: &LegId, cfg: GatherConfig) -> Result<(), ProviderError> {
        self.lock()
            .commands
            .push(MockCommand::Gather(leg.clone(), cfg));
        Ok(())
    }

    async fn start_recording(
        &self,
        leg: &LegId,
        cfg: RecordingConfig,
    ) -> Result<(), ProviderError> {
        let mut st = self.lock();
        st.commands
            .push(MockCommand::StartRecording(leg.clone(), cfg));
        st.recording_legs.push(leg.clone());
        Ok(())
    }

    async fn stop_recording(&self, leg: &LegId) -> Result<(), ProviderError> {
        let mut st = self.lock();
        st.commands.push(MockCommand::StopRecording(leg.clone()));
        st.recording_legs.retain(|l| l != leg);
        Ok(())
    }

    async fn fetch_cdr(&self, leg: &LegId) -> Result<Option<Cdr>, ProviderError> {
        Ok(self.lock().cdrs.get(leg.as_str()).cloned())
    }

    async fn fetch_rate_deck(&self) -> Result<Vec<Rate>, ProviderError> {
        let st = self.lock();
        if let Some(e) = &st.rate_deck_error {
            return Err(e.clone());
        }
        Ok(st.rate_deck.clone())
    }

    fn verify_webhook(
        &self,
        headers: &WebhookHeaders,
        body: &[u8],
        now: DateTime<Utc>,
    ) -> Result<ProviderEvent, WebhookError> {
        let (Some(sig), Some(ts)) = (headers.signature.as_deref(), headers.timestamp.as_deref())
        else {
            return Err(WebhookError::MissingSignature);
        };

        // Timestamp first: a replay of a genuinely-signed old body has a VALID signature,
        // so checking the signature alone would accept it.
        let secs: i64 = ts.parse().map_err(|_| WebhookError::BadSignature)?;
        let sent = DateTime::from_timestamp(secs, 0).ok_or(WebhookError::BadSignature)?;
        if (now - sent).abs() > self.tolerance {
            return Err(WebhookError::StaleTimestamp);
        }

        // Constant-time: a timing oracle on a signature comparison is a real forgery path,
        // and `==` on a String is not constant time.
        let expected = self.signature_for(ts, body);
        if expected.as_bytes().ct_eq(sig.as_bytes()).unwrap_u8() != 1 {
            return Err(WebhookError::BadSignature);
        }

        let parsed: MockWebhookBody =
            serde_json::from_slice(body).map_err(|e| WebhookError::Malformed {
                detail: e.to_string(),
            })?;

        let kind = match parsed.event_type.as_str() {
            "initiated" => ProviderEventKind::Initiated,
            "ringing" => ProviderEventKind::Ringing,
            "answered" => ProviderEventKind::Answered,
            "hangup" => ProviderEventKind::Hangup {
                cause: cause_to_reason(parsed.cause.as_deref()),
            },
            "media_started" => ProviderEventKind::MediaStarted,
            "media_stopped" => ProviderEventKind::MediaStopped,
            "recording_started" => ProviderEventKind::RecordingStarted,
            "recording_saved" => ProviderEventKind::RecordingSaved {
                url: parsed.recording_url.clone().unwrap_or_default(),
                duration_secs: parsed.duration_secs.unwrap_or_default(),
            },
            "dtmf" => match parsed.digit {
                Some(d) => ProviderEventKind::Dtmf { digit: d },
                None => {
                    return Err(WebhookError::Malformed {
                        detail: "dtmf event without a digit".into(),
                    })
                }
            },
            other => ProviderEventKind::Unhandled {
                raw_type: other.to_string(),
            },
        };

        Ok(ProviderEvent {
            provider: MOCK_ID,
            event_id: parsed.event_id,
            leg_id: LegId::new(parsed.leg_id),
            client_state: parsed.client_state,
            occurred_at: parsed.occurred_at,
            kind,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telephony::{MediaCodec, MediaTrack, E164};

    fn n(raw: &str) -> E164 {
        E164::parse(raw).expect("test number")
    }

    fn dial_req() -> DialRequest {
        DialRequest {
            to: n("+8613800138000"),
            from: n("+390212345678"),
            client_state: "call-1".into(),
            timeout_secs: 30,
            region: "eu-fra".into(),
        }
    }

    fn stream_cfg() -> MediaStreamConfig {
        MediaStreamConfig {
            url: "wss://media.example/voip/media/tok".into(),
            codec: MediaCodec::L16,
            track: MediaTrack::Inbound,
            bidirectional: true,
        }
    }

    // ---- call control ---------------------------------------------------------

    #[tokio::test]
    async fn dialing_creates_a_live_leg_and_records_the_request() {
        let p = MockTelephonyProvider::default();
        let leg = p.dial(dial_req()).await.unwrap();
        assert!(p.is_live(&leg.id));
        assert!(
            matches!(p.commands().as_slice(), [MockCommand::Dial(r)] if r.to == n("+8613800138000"))
        );
    }

    #[tokio::test]
    async fn a_scripted_dial_failure_creates_no_leg() {
        let p = MockTelephonyProvider::default();
        p.fail_next_dial(ProviderError::AccountBlocked);
        assert_eq!(
            p.dial(dial_req()).await.unwrap_err(),
            ProviderError::AccountBlocked
        );
        // …and the next dial succeeds, so "first fails, second works" is expressible.
        assert!(p.dial(dial_req()).await.is_ok());
    }

    #[tokio::test]
    async fn hanging_up_twice_is_not_an_error() {
        // Teardown always races the far party dropping the call. Surfacing that race as an
        // error would make every clean hangup look like a fault in the logs.
        let p = MockTelephonyProvider::default();
        let leg = p.dial(dial_req()).await.unwrap().id;
        assert!(p.hangup(&leg).await.is_ok());
        assert!(p.hangup(&leg).await.is_ok());
        assert!(!p.is_live(&leg));
    }

    // ---- the constraint the architecture rests on (D2) -------------------------

    #[tokio::test]
    async fn a_leg_accepts_exactly_one_media_stream() {
        // This is the provider limit that rules out bridge-and-fork. Reproducing it here
        // means an orchestrator that forgets fails in the test suite, not in production.
        let p = MockTelephonyProvider::default();
        let leg = p.dial(dial_req()).await.unwrap().id;
        assert!(p.start_media_stream(&leg, stream_cfg()).await.is_ok());
        assert!(p.is_streaming(&leg));
        assert_eq!(
            p.start_media_stream(&leg, stream_cfg()).await.unwrap_err(),
            ProviderError::Unsupported {
                operation: "open a second media stream on one leg"
            }
        );
        // Stopping frees the budget, so a reconnect is fine.
        p.stop_media_stream(&leg).await.unwrap();
        assert!(!p.is_streaming(&leg));
        assert!(p.start_media_stream(&leg, stream_cfg()).await.is_ok());
    }

    #[tokio::test]
    async fn two_legs_each_get_their_own_stream() {
        // Which is exactly how a translated call is built: two legs, two streams, never
        // bridged.
        let p = MockTelephonyProvider::default();
        let a = p.dial(dial_req()).await.unwrap().id;
        let b = p.dial(dial_req()).await.unwrap().id;
        assert_ne!(a, b);
        assert!(p.start_media_stream(&a, stream_cfg()).await.is_ok());
        assert!(p.start_media_stream(&b, stream_cfg()).await.is_ok());
    }

    #[tokio::test]
    async fn hanging_up_releases_media_and_recording() {
        let p = MockTelephonyProvider::default();
        let leg = p.dial(dial_req()).await.unwrap().id;
        p.start_media_stream(&leg, stream_cfg()).await.unwrap();
        p.start_recording(
            &leg,
            RecordingConfig {
                dual_channel: true,
                beep: true,
            },
        )
        .await
        .unwrap();
        assert!(p.is_recording(&leg));
        p.hangup(&leg).await.unwrap();
        assert!(!p.is_streaming(&leg));
        assert!(!p.is_recording(&leg));
    }

    #[tokio::test]
    async fn the_command_log_preserves_order_so_privacy_tests_can_read_it() {
        // "Was the announcement played before recording started?" is only answerable from
        // an ordered log.
        let p = MockTelephonyProvider::default();
        let leg = p.dial(dial_req()).await.unwrap().id;
        p.play(
            &leg,
            PlayRequest::Audio {
                payload_b64: "AAA".into(),
            },
        )
        .await
        .unwrap();
        p.gather(
            &leg,
            GatherConfig {
                valid_digits: "1".into(),
                max_digits: 1,
                timeout_secs: 10,
            },
        )
        .await
        .unwrap();
        p.start_recording(
            &leg,
            RecordingConfig {
                dual_channel: false,
                beep: true,
            },
        )
        .await
        .unwrap();

        let cmds = p.commands();
        let play_at = cmds.iter().position(|c| matches!(c, MockCommand::Play(..)));
        let rec_at = cmds
            .iter()
            .position(|c| matches!(c, MockCommand::StartRecording(..)));
        assert!(play_at < rec_at, "announcement must precede recording");
    }

    // ---- webhook verification (R24) -------------------------------------------

    fn signed(
        p: &MockTelephonyProvider,
        body: &MockWebhookBody,
        at: DateTime<Utc>,
    ) -> (WebhookHeaders, Vec<u8>) {
        let raw = p.body_for(body);
        (p.sign(&raw, at), raw)
    }

    #[test]
    fn a_correctly_signed_webhook_is_accepted_and_normalised() {
        let p = MockTelephonyProvider::default();
        let now = Utc::now();
        let body = MockWebhookBody::new("evt-1", &LegId::new("leg-1"), "answered", now)
            .with_client_state("call-42");
        let (h, raw) = signed(&p, &body, now);

        let ev = p.verify_webhook(&h, &raw, now).unwrap();
        assert_eq!(ev.provider, MOCK_ID);
        assert_eq!(ev.event_id, "evt-1");
        assert_eq!(ev.leg_id, LegId::new("leg-1"));
        assert_eq!(ev.client_state.as_deref(), Some("call-42"));
        assert_eq!(ev.kind, ProviderEventKind::Answered);
    }

    #[test]
    fn an_unsigned_webhook_is_refused() {
        let p = MockTelephonyProvider::default();
        let now = Utc::now();
        let raw = p.body_for(&MockWebhookBody::new("e", &LegId::new("l"), "ringing", now));
        assert_eq!(
            p.verify_webhook(&WebhookHeaders::default(), &raw, now)
                .unwrap_err(),
            WebhookError::MissingSignature
        );
        // A timestamp without a signature is still unsigned.
        let half = WebhookHeaders {
            signature: None,
            timestamp: Some(now.timestamp().to_string()),
        };
        assert_eq!(
            p.verify_webhook(&half, &raw, now).unwrap_err(),
            WebhookError::MissingSignature
        );
    }

    #[test]
    fn a_tampered_body_invalidates_the_signature() {
        // The attack this stops: a valid webhook replayed with the leg id swapped, to
        // drive somebody else's call.
        let p = MockTelephonyProvider::default();
        let now = Utc::now();
        let body = MockWebhookBody::new("evt-1", &LegId::new("leg-1"), "answered", now);
        let (h, raw) = signed(&p, &body, now);

        let mut tampered: MockWebhookBody = serde_json::from_slice(&raw).unwrap();
        tampered.leg_id = "leg-victim".into();
        let tampered_raw = p.body_for(&tampered);

        assert_eq!(
            p.verify_webhook(&h, &tampered_raw, now).unwrap_err(),
            WebhookError::BadSignature
        );
    }

    #[test]
    fn a_signature_from_a_different_key_is_refused() {
        let mine = MockTelephonyProvider::default();
        let theirs = MockTelephonyProvider::new(b"someone-elses-secret", Duration::minutes(5));
        let now = Utc::now();
        let body = MockWebhookBody::new("evt-1", &LegId::new("leg-1"), "answered", now);
        let (their_headers, raw) = signed(&theirs, &body, now);
        assert_eq!(
            mine.verify_webhook(&their_headers, &raw, now).unwrap_err(),
            WebhookError::BadSignature
        );
    }

    #[test]
    fn a_replayed_webhook_is_refused_even_though_its_signature_is_genuine() {
        // The whole point of the timestamp: this body IS correctly signed. Only its age
        // gives it away, which is why the timestamp is checked before the signature.
        let p = MockTelephonyProvider::default();
        let then = Utc::now() - Duration::minutes(30);
        let body = MockWebhookBody::new("evt-1", &LegId::new("leg-1"), "answered", then);
        let (h, raw) = signed(&p, &body, then);

        assert_eq!(
            p.verify_webhook(&h, &raw, Utc::now()).unwrap_err(),
            WebhookError::StaleTimestamp
        );
        // At the time it was sent it was fine.
        assert!(p.verify_webhook(&h, &raw, then).is_ok());
    }

    #[test]
    fn a_webhook_from_the_future_is_refused_too() {
        // A clock skew large enough to matter is indistinguishable from an attack, and
        // accepting it would widen the replay window in the one direction nobody checks.
        let p = MockTelephonyProvider::default();
        let future = Utc::now() + Duration::minutes(30);
        let body = MockWebhookBody::new("evt-1", &LegId::new("leg-1"), "answered", future);
        let (h, raw) = signed(&p, &body, future);
        assert_eq!(
            p.verify_webhook(&h, &raw, Utc::now()).unwrap_err(),
            WebhookError::StaleTimestamp
        );
    }

    #[test]
    fn a_body_that_is_not_the_expected_shape_is_malformed_not_accepted() {
        let p = MockTelephonyProvider::default();
        let now = Utc::now();
        let raw = b"{\"nope\":true}".to_vec();
        let h = p.sign(&raw, now);
        assert!(matches!(
            p.verify_webhook(&h, &raw, now).unwrap_err(),
            WebhookError::Malformed { .. }
        ));
    }

    #[test]
    fn hangup_causes_map_onto_domain_reasons_and_unknown_ones_stay_visible() {
        let p = MockTelephonyProvider::default();
        let now = Utc::now();
        for (cause, expected) in [
            ("busy", FailureReason::Busy),
            ("rejected", FailureReason::Rejected),
            ("no_answer", FailureReason::NoAnswer),
            ("unallocated", FailureReason::Unallocated),
            // An unrecognised carrier cause must NOT be laundered into
            // `ProviderUnavailable`: that reason already means something, and hiding an
            // unmapped cause inside it makes the gap invisible in analytics.
            ("something-new-from-the-carrier", FailureReason::Unmapped),
        ] {
            let body = MockWebhookBody::new("e", &LegId::new("l"), "hangup", now).with_cause(cause);
            let (h, raw) = signed(&p, &body, now);
            assert_eq!(
                p.verify_webhook(&h, &raw, now).unwrap().kind,
                ProviderEventKind::Hangup { cause: expected },
                "cause {cause}"
            );
        }
    }

    #[test]
    fn an_unmodelled_event_type_is_recorded_rather_than_dropped() {
        // Recorded so the redelivery is still absorbed by the idempotency ledger, and so a
        // renamed provider event shows up as data instead of as silence.
        let p = MockTelephonyProvider::default();
        let now = Utc::now();
        let body = MockWebhookBody::new("e", &LegId::new("l"), "call.machine.detected", now);
        let (h, raw) = signed(&p, &body, now);
        assert_eq!(
            p.verify_webhook(&h, &raw, now).unwrap().kind,
            ProviderEventKind::Unhandled {
                raw_type: "call.machine.detected".into()
            }
        );
    }

    #[test]
    fn a_dtmf_event_without_a_digit_is_malformed() {
        let p = MockTelephonyProvider::default();
        let now = Utc::now();
        let body = MockWebhookBody::new("e", &LegId::new("l"), "dtmf", now);
        let (h, raw) = signed(&p, &body, now);
        assert!(matches!(
            p.verify_webhook(&h, &raw, now).unwrap_err(),
            WebhookError::Malformed { .. }
        ));

        let ok = MockWebhookBody::new("e", &LegId::new("l"), "dtmf", now).with_digit('1');
        let (h, raw) = signed(&p, &ok, now);
        assert_eq!(
            p.verify_webhook(&h, &raw, now).unwrap().kind,
            ProviderEventKind::Dtmf { digit: '1' }
        );
    }

    // ---- capabilities ---------------------------------------------------------

    #[test]
    fn the_mock_does_not_pretend_to_be_in_the_eu() {
        // A gate test that passed because the MOCK claimed EU residency would prove
        // nothing about the real gate.
        assert!(!MockTelephonyProvider::default().metadata().eu_telephony);
        assert!(MockTelephonyProvider::eu().metadata().eu_telephony);
        assert_eq!(MockTelephonyProvider::eu().metadata().region, "eu-fra");
    }

    #[tokio::test]
    async fn cdr_and_rate_deck_are_scriptable_including_their_absence() {
        let p = MockTelephonyProvider::default();
        let leg = p.dial(dial_req()).await.unwrap().id;
        // Not rated yet is the normal state right after hangup, and must not be an error.
        assert_eq!(p.fetch_cdr(&leg).await.unwrap(), None);

        p.set_cdr(
            &leg,
            Cdr {
                leg_id: leg.clone(),
                billed_seconds: 42,
                cost: rust_decimal::Decimal::new(123, 4),
                codec: Some(MediaCodec::L16),
                hangup_cause: Some(FailureReason::Unmapped),
            },
        );
        assert_eq!(p.fetch_cdr(&leg).await.unwrap().unwrap().billed_seconds, 42);

        assert!(p.fetch_rate_deck().await.unwrap().is_empty());
        p.fail_rate_deck(ProviderError::RateLimited);
        assert_eq!(
            p.fetch_rate_deck().await.unwrap_err(),
            ProviderError::RateLimited
        );
    }
}
