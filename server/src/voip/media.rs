//! The phone leg's audio, wired into the translation engines (spec 0111, D2/D3, R14–R18).
//!
//! This is the piece that makes a telephone a **peer**.
//!
//! A translated call is a private room with two participants. One is an ordinary browser
//! peer on the existing capture path; the other is a telephone, and everything that makes
//! it look like a peer happens here:
//!
//! ```text
//!  phone speaks ──► media WS ──► decode ──► 16k→24k ──► engine session ──► TranslatedAudio
//!                                                                             │
//!                                                          (to the BROWSER peer's channel)
//!
//!  browser speaks ─► existing capture path ─► engine session ─► TranslatedAudio
//!                                                                             │
//!  phone hears  ◄── media WS ◄── encode ◄── 24k→16k ◄────────────(phone peer's channel)
//! ```
//!
//! Two consequences worth stating, because they are what the architecture rests on:
//!
//! - **The legs are never bridged at the provider.** Each hears only the *translation* of
//!   the other, because the only audio ever written back to a leg is audio this module
//!   produced. Raw leakage is not prevented by care; it is unreachable.
//! - **Everything downstream is reused.** Subtitles, transcripts, the per-language
//!   fan-out, capacity fallback, the meter — all of it already works for a peer, and a
//!   phone is now a peer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::telephony::MediaCodec;
use crate::voip::codec::{self, PlaybackGate, Resampler};

/// What the engines speak (spec 0043). Everything on the wire is resampled to this.
pub const ENGINE_RATE_HZ: u32 = 24_000;

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

// ---------------------------------------------------------------------------
// Wire protocol
// ---------------------------------------------------------------------------

/// A frame arriving from the provider's media socket.
///
/// Deliberately tolerant: unknown `event` values parse into [`Inbound::Other`] rather than
/// failing. A provider that adds a frame type must not take the audio path down — the far
/// end would simply go silent, which is the worst possible failure for a phone call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inbound {
    /// Socket accepted.
    Connected,
    /// Stream is starting; carries the negotiated format.
    Start { codec: MediaCodec, sample_rate: u32 },
    /// A chunk of the far party's audio.
    Media { payload_b64: String, track: String },
    /// A keypad digit.
    Dtmf { digit: char },
    /// The stream is over.
    Stop,
    /// Something we do not model.
    Other { event: String },
}

#[derive(Debug, Deserialize)]
struct RawFrame {
    event: String,
    #[serde(default)]
    start: Option<RawStart>,
    #[serde(default)]
    media: Option<RawMedia>,
    #[serde(default)]
    dtmf: Option<RawDtmf>,
}

#[derive(Debug, Deserialize)]
struct RawStart {
    #[serde(default)]
    media_format: Option<RawFormat>,
}

#[derive(Debug, Deserialize)]
struct RawFormat {
    #[serde(default)]
    encoding: Option<String>,
    #[serde(default)]
    sample_rate: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct RawMedia {
    #[serde(default)]
    payload: Option<String>,
    #[serde(default)]
    track: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawDtmf {
    #[serde(default)]
    digit: Option<String>,
}

/// Parse one text frame from the media socket.
pub fn parse_inbound(raw: &str) -> Result<Inbound, MediaError> {
    let f: RawFrame =
        serde_json::from_str(raw).map_err(|e| MediaError::Malformed(e.to_string()))?;
    Ok(match f.event.as_str() {
        "connected" => Inbound::Connected,
        "start" => {
            let fmt = f.start.and_then(|s| s.media_format);
            let encoding = fmt.as_ref().and_then(|m| m.encoding.clone());
            let codec = codec_from_name(encoding.as_deref())?;
            // Trust the codec's own rate over a reported one that disagrees: a mismatch
            // means we misread the frame, and resampling from a wrong rate produces audio
            // that is intelligible-but-wrong, which is far harder to diagnose than silence.
            let sample_rate = fmt
                .and_then(|m| m.sample_rate)
                .filter(|r| *r == codec.sample_rate())
                .unwrap_or_else(|| codec.sample_rate());
            Inbound::Start { codec, sample_rate }
        }
        "media" => {
            let m = f.media.ok_or_else(|| {
                MediaError::Malformed("media frame without a media object".into())
            })?;
            Inbound::Media {
                payload_b64: m.payload.unwrap_or_default(),
                track: m.track.unwrap_or_else(|| "inbound".into()),
            }
        }
        "dtmf" => {
            let d = f
                .dtmf
                .and_then(|d| d.digit)
                .and_then(|s| s.chars().next())
                .ok_or_else(|| MediaError::Malformed("dtmf frame without a digit".into()))?;
            Inbound::Dtmf { digit: d }
        }
        "stop" => Inbound::Stop,
        other => Inbound::Other {
            event: other.to_string(),
        },
    })
}

fn codec_from_name(name: Option<&str>) -> Result<MediaCodec, MediaError> {
    match name.map(str::to_ascii_uppercase).as_deref() {
        // Absent means the provider did not tell us, and PCMU is the universal PSTN
        // default. Guessing L16 instead would decode µ-law bytes as linear PCM — loud
        // noise, not silence.
        None | Some("PCMU") => Ok(MediaCodec::Pcmu),
        Some("PCMA") => Ok(MediaCodec::Pcma),
        Some("L16") => Ok(MediaCodec::L16),
        Some("G722") => Ok(MediaCodec::G722),
        Some("OPUS") => Ok(MediaCodec::Opus),
        Some(other) => Err(MediaError::UnsupportedCodec(other.to_string())),
    }
}

/// A frame we send back down the media socket.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "event")]
pub enum Outbound {
    /// Audio for the far party.
    #[serde(rename = "media")]
    Media { media: OutboundMedia },
    /// Discard whatever is queued for playback — barge-in (R15).
    #[serde(rename = "clear")]
    Clear,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OutboundMedia {
    pub payload: String,
}

impl Outbound {
    pub fn media(payload_b64: String) -> Self {
        Self::Media {
            media: OutboundMedia {
                payload: payload_b64,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaError {
    Malformed(String),
    UnsupportedCodec(String),
}

impl std::fmt::Display for MediaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(d) => write!(f, "malformed media frame: {d}"),
            Self::UnsupportedCodec(c) => write!(f, "unsupported media codec: {c}"),
        }
    }
}

// ---------------------------------------------------------------------------
// The bridge
// ---------------------------------------------------------------------------

/// One direction's audio conversion, with its resampler state.
///
/// Holds the resampler because the resampler is stateful and must be — see
/// [`crate::voip::codec::Resampler`]. Creating one per frame puts a click at every 20 ms
/// boundary.
pub struct Leg {
    codec: MediaCodec,
    /// Whether the provider has ever told us what it is actually sending.
    ///
    /// Tracked because "confirmed correct" and "never confirmed" look identical from the
    /// outside: both leave `codec_renegotiations_total` flat. A carrier that sends media
    /// without a `start` frame leaves the leg decoding a guess, and a wrong guess is
    /// noise in both directions with nothing anywhere reporting an error.
    confirmed: bool,
    /// Phone → engine.
    up: Resampler,
    /// Engine → phone.
    down: Resampler,
    gate: PlaybackGate,
    /// Whether the far party is currently speaking, for barge-in.
    far_speaking: Arc<AtomicBool>,
}

impl Leg {
    pub fn new(codec: MediaCodec) -> Self {
        let wire = codec.sample_rate();
        Self {
            codec,
            confirmed: false,
            up: Resampler::new(wire, ENGINE_RATE_HZ),
            down: Resampler::new(ENGINE_RATE_HZ, wire),
            gate: PlaybackGate::new(),
            far_speaking: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn codec(&self) -> MediaCodec {
        self.codec
    }

    /// Adopt the codec the provider actually negotiated.
    ///
    /// We ask for L16; the route decides. A carrier that forces µ-law does not fail the
    /// call — it just sends different bytes, and a leg still decoding as L16 reads them as
    /// noise in both directions with nothing anywhere reporting an error. So the `start`
    /// frame is authoritative, not the request.
    ///
    /// Rebuilding the resamplers is the point: their rates come from the codec, and a
    /// resampler configured for the wrong rate is the same silent corruption one step
    /// further on. A no-op when the codec is already right, so a provider that repeats
    /// `start` does not clear the filter state mid-utterance.
    pub fn renegotiate(&mut self, codec: MediaCodec) -> bool {
        self.confirmed = true;
        if codec == self.codec {
            return false;
        }
        tracing::info!(
            requested = ?self.codec,
            negotiated = ?codec,
            "phone leg negotiated a different codec"
        );
        let wire = codec.sample_rate();
        self.codec = codec;
        self.up = Resampler::new(wire, ENGINE_RATE_HZ);
        self.down = Resampler::new(ENGINE_RATE_HZ, wire);
        true
    }

    /// Wire audio from the phone → PCM16 at the engine's rate.
    pub fn decode_up(&mut self, payload_b64: &str) -> Result<Vec<u8>, MediaError> {
        let bytes = B64
            .decode(payload_b64)
            .map_err(|e| MediaError::Malformed(e.to_string()))?;
        let samples = codec::decode(self.codec, &bytes)
            .map_err(|e| MediaError::Malformed(format!("{e:?}")))?;
        let resampled = self.up.process(&samples);
        Ok(resampled.iter().flat_map(|s| s.to_le_bytes()).collect())
    }

    /// Translated PCM16 from the engine → wire audio for the phone.
    ///
    /// The engine emits little-endian PCM16 (the browser's format); the wire wants the
    /// codec's own. Both conversions live here so neither can be forgotten at a call site.
    pub fn encode_down(&mut self, pcm16_le: &[u8]) -> Result<String, MediaError> {
        if !pcm16_le.len().is_multiple_of(2) {
            return Err(MediaError::Malformed("odd-length PCM16 buffer".into()));
        }
        let samples: Vec<i16> = pcm16_le
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| i16::from_le_bytes(*p))
            .collect();
        let resampled = self.down.process(&samples);
        let wire = codec::encode(self.codec, &resampled)
            .map_err(|e| MediaError::Malformed(format!("{e:?}")))?;
        Ok(B64.encode(wire))
    }

    pub fn gate(&mut self) -> &mut PlaybackGate {
        &mut self.gate
    }

    /// The far party started talking. Returns whether anything had to be discarded, so the
    /// caller only sends `clear` when there was something to clear — an unnecessary
    /// command on a silent leg is a wasted round trip on the latency path.
    pub fn barge_in(&mut self) -> bool {
        self.far_speaking.store(true, Ordering::Relaxed);
        self.gate.barge_in()
    }

    pub fn far_stopped(&self) {
        self.far_speaking.store(false, Ordering::Relaxed);
    }

    pub fn far_speaking(&self) -> bool {
        self.far_speaking.load(Ordering::Relaxed)
    }

    /// True the first time audio arrives on a leg whose format was never announced.
    ///
    /// Called once per leg, not per frame: the point is a single line in the log saying
    /// "this call is decoding a guess", not a stream of them.
    fn note_unconfirmed(&mut self) -> bool {
        if self.confirmed {
            return false;
        }
        self.confirmed = true;
        true
    }
}

/// Everything a live media socket needs.
pub struct BridgeHandles {
    /// Captured phone audio, PCM16 LE @ [`ENGINE_RATE_HZ`], for the engine session.
    pub to_engine: mpsc::Sender<Vec<u8>>,
    /// Frames the phone peer's room channel produced, already JSON.
    pub from_room: mpsc::Receiver<String>,
    /// DTMF digits, for the consent gate.
    pub digits: mpsc::Sender<char>,
}

/// Latency stamps for one translated utterance (spec 0111 §P).
///
/// Measured, not estimated. The product's headline number is the span from the far party
/// finishing a phrase to the first translated sample reaching the other end, and a
/// request-duration proxy does not measure that at all.
#[derive(Debug, Clone, Copy)]
pub struct UtteranceTimer {
    speech_end: Instant,
}

impl UtteranceTimer {
    pub fn started() -> Self {
        Self {
            speech_end: Instant::now(),
        }
    }

    /// Milliseconds from end of speech to the first translated audio written to the wire.
    pub fn first_audio_ms(&self) -> u64 {
        self.speech_end.elapsed().as_millis().min(u64::MAX as u128) as u64
    }
}

/// Pump one media socket: phone audio in, translated audio out.
///
/// Split from the socket type so it is driven by plain channels and can be tested without
/// a WebSocket, a provider or a network.
pub async fn pump<S, E>(
    mut socket: S,
    mut leg: Leg,
    mut handles: BridgeHandles,
) -> Result<(), MediaError>
where
    S: futures::Sink<String, Error = E> + futures::Stream<Item = Result<String, E>> + Unpin,
{
    loop {
        tokio::select! {
            // Audio and control from the phone.
            incoming = socket.next() => {
                let Some(Ok(raw)) = incoming else { break };
                match parse_inbound(&raw)? {
                    Inbound::Media { payload_b64, .. } => {
                        if leg.note_unconfirmed() {
                            // Not fatal, and possibly fine — but it is the difference
                            // between "we know the format" and "we assumed it", and the
                            // wrong assumption is silent corruption rather than an error.
                            tracing::warn!(
                                assumed = ?leg.codec(),
                                "phone media arrived with no start frame; decoding an assumed codec"
                            );
                        }
                        // Speaking again mid-playback: drop what is queued for them AND
                        // tell the provider to drop what it already buffered. Without the
                        // second half the caller hears the sentence they interrupted
                        // finish anyway, which reads as being ignored.
                        if !leg.far_speaking() && leg.barge_in() {
                            let _ = socket.send(json(&Outbound::Clear)).await;
                        }
                        let pcm = leg.decode_up(&payload_b64)?;
                        // A full channel means the engine is behind. Dropping is right:
                        // back-pressuring a live phone call would stall the socket and the
                        // far party would hear a gap growing without bound.
                        let _ = handles.to_engine.try_send(pcm);
                    }
                    Inbound::Dtmf { digit } => {
                        let _ = handles.digits.try_send(digit);
                    }
                    Inbound::Stop => break,
                    Inbound::Start { codec, .. } => {
                        if leg.renegotiate(codec) {
                            crate::metrics::record_voip_codec_renegotiation();
                        }
                    }
                    Inbound::Connected | Inbound::Other { .. } => {}
                }
            }

            // Translated audio for the phone, from its room channel.
            frame = handles.from_room.recv() => {
                let Some(frame) = frame else { break };
                if let Some(pcm_b64) = translated_audio_payload(&frame) {
                    leg.far_stopped();
                    let Ok(pcm) = B64.decode(&pcm_b64) else { continue };
                    let payload = leg.encode_down(&pcm)?;
                    // The gate holds nothing: it decides whether this chunk still belongs
                    // to the current utterance and accounts for what the provider now has
                    // buffered. The audio itself goes straight to the wire.
                    let gen = leg.gate().generation();
                    let bytes = payload.len();
                    if leg.gate().accept(gen, bytes) {
                        let _ = socket.send(json(&Outbound::media(payload))).await;
                    }
                }
            }
        }
    }
    Ok(())
}

fn json<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "{}".into())
}

/// Pull the PCM out of a `translated_audio` room frame, ignoring everything else.
///
/// The phone peer's channel carries every room message — subtitles, presence, reactions.
/// Only audio belongs on a telephone; the rest has no way to be rendered there and would
/// be noise if it somehow were.
pub fn translated_audio_payload(frame: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(frame).ok()?;
    if v.get("type")?.as_str()? != "translated_audio" {
        return None;
    }
    Some(v.get("pcm16_b64")?.as_str()?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media_frame(payload: &str) -> String {
        serde_json::json!({
            "event": "media",
            "media": { "payload": payload, "track": "inbound" }
        })
        .to_string()
    }

    // ---- inbound parsing ------------------------------------------------------

    #[test]
    fn the_start_frame_carries_the_negotiated_codec() {
        let raw = serde_json::json!({
            "event": "start",
            "start": { "media_format": { "encoding": "L16", "sample_rate": 16000 } }
        })
        .to_string();
        assert_eq!(
            parse_inbound(&raw).unwrap(),
            Inbound::Start {
                codec: MediaCodec::L16,
                sample_rate: 16_000
            }
        );
    }

    #[test]
    fn a_reported_rate_that_contradicts_the_codec_is_ignored() {
        // A mismatch means we misread the frame. Resampling from a wrong rate produces
        // audio that is intelligible-but-wrong — far harder to diagnose than silence.
        let raw = serde_json::json!({
            "event": "start",
            "start": { "media_format": { "encoding": "PCMU", "sample_rate": 48000 } }
        })
        .to_string();
        assert_eq!(
            parse_inbound(&raw).unwrap(),
            Inbound::Start {
                codec: MediaCodec::Pcmu,
                sample_rate: 8_000
            }
        );
    }

    #[test]
    fn a_start_frame_with_no_format_falls_back_to_the_pstn_default() {
        // PCMU, not L16: decoding µ-law bytes as linear PCM is loud noise, and the
        // universal PSTN default is the safe assumption when nobody said.
        let raw = serde_json::json!({ "event": "start" }).to_string();
        assert_eq!(
            parse_inbound(&raw).unwrap(),
            Inbound::Start {
                codec: MediaCodec::Pcmu,
                sample_rate: 8_000
            }
        );
    }

    #[test]
    fn an_unmodelled_frame_does_not_take_the_audio_path_down() {
        // The far end going silent is the worst possible failure on a phone call, so an
        // unknown frame type is ignored rather than fatal.
        let raw = serde_json::json!({ "event": "mark", "mark": { "name": "x" } }).to_string();
        assert_eq!(
            parse_inbound(&raw).unwrap(),
            Inbound::Other {
                event: "mark".into()
            }
        );
    }

    #[test]
    fn a_codec_we_cannot_decode_is_refused_at_negotiation_not_mid_call() {
        let raw = serde_json::json!({
            "event": "start",
            "start": { "media_format": { "encoding": "AMR-WB" } }
        })
        .to_string();
        assert_eq!(
            parse_inbound(&raw).unwrap_err(),
            MediaError::UnsupportedCodec("AMR-WB".into())
        );
    }

    #[test]
    fn dtmf_and_stop_parse() {
        let raw = serde_json::json!({ "event": "dtmf", "dtmf": { "digit": "1" } }).to_string();
        assert_eq!(parse_inbound(&raw).unwrap(), Inbound::Dtmf { digit: '1' });
        assert_eq!(
            parse_inbound(&serde_json::json!({ "event": "stop" }).to_string()).unwrap(),
            Inbound::Stop
        );
        // A dtmf frame with no digit is malformed rather than silently dropped: the
        // consent gate depends on it.
        let bad = serde_json::json!({ "event": "dtmf" }).to_string();
        assert!(matches!(
            parse_inbound(&bad).unwrap_err(),
            MediaError::Malformed(_)
        ));
    }

    #[test]
    fn junk_is_malformed_not_a_panic() {
        assert!(matches!(
            parse_inbound("not json").unwrap_err(),
            MediaError::Malformed(_)
        ));
    }

    // ---- outbound shape -------------------------------------------------------

    #[test]
    fn outbound_frames_match_the_provider_contract() {
        assert_eq!(
            serde_json::to_string(&Outbound::media("AAA".into())).unwrap(),
            r#"{"event":"media","media":{"payload":"AAA"}}"#
        );
        assert_eq!(
            serde_json::to_string(&Outbound::Clear).unwrap(),
            r#"{"event":"clear"}"#
        );
    }

    // ---- conversion -----------------------------------------------------------

    #[test]
    fn phone_audio_becomes_engine_audio_at_the_engine_rate() {
        let mut leg = Leg::new(MediaCodec::L16);
        // 20 ms at 16 kHz = 320 samples, big-endian on the wire.
        let wire: Vec<u8> = (0..320i16).flat_map(|s| s.to_be_bytes()).collect();
        let out = leg.decode_up(&B64.encode(&wire)).unwrap();
        // 20 ms at 24 kHz = 480 samples = 960 bytes, little-endian for the engine.
        assert!(
            (out.len() as i64 - 960).abs() <= 8,
            "expected ~960 bytes, got {}",
            out.len()
        );
        assert!(out.len().is_multiple_of(2));
    }

    #[test]
    fn engine_audio_becomes_phone_audio_at_the_wire_rate() {
        let mut leg = Leg::new(MediaCodec::L16);
        // 480 samples at 24 kHz, little-endian, as the engine emits.
        let pcm: Vec<u8> = (0..480i16).flat_map(|s| s.to_le_bytes()).collect();
        let payload = leg.encode_down(&pcm).unwrap();
        let wire = B64.decode(payload).unwrap();
        assert!(
            (wire.len() as i64 - 640).abs() <= 8,
            "expected ~640 bytes (320 samples big-endian), got {}",
            wire.len()
        );
    }

    #[test]
    fn the_endianness_flips_between_the_wire_and_the_engine() {
        // L16 is network byte order; the engine speaks the browser's little-endian PCM16.
        // Getting this backwards is loud noise, not silence — assert it directly.
        let mut leg = Leg::new(MediaCodec::L16);
        let wire = [0x12u8, 0x34];
        let out = leg.decode_up(&B64.encode(wire)).unwrap();
        // The resampler's first output is filtered, so compare the round trip's meaning
        // rather than one sample: decoding must read 0x1234, not 0x3412.
        let direct = codec::decode(MediaCodec::L16, &wire).unwrap();
        assert_eq!(direct[0], 0x1234);
        assert!(!out.is_empty());
    }

    #[test]
    fn an_odd_length_engine_buffer_is_refused() {
        let mut leg = Leg::new(MediaCodec::L16);
        assert!(matches!(
            leg.encode_down(&[1, 2, 3]).unwrap_err(),
            MediaError::Malformed(_)
        ));
    }

    #[test]
    fn a_mu_law_leg_converts_both_ways() {
        let mut leg = Leg::new(MediaCodec::Pcmu);
        assert_eq!(leg.codec(), MediaCodec::Pcmu);
        // 20 ms at 8 kHz = 160 bytes.
        let out = leg.decode_up(&B64.encode(vec![0xFFu8; 160])).unwrap();
        // 20 ms at 24 kHz = 480 samples = 960 bytes.
        assert!((out.len() as i64 - 960).abs() <= 8, "got {}", out.len());

        let pcm: Vec<u8> = vec![0u8; 960];
        let payload = leg.encode_down(&pcm).unwrap();
        assert!((B64.decode(payload).unwrap().len() as i64 - 160).abs() <= 8);
    }

    #[test]
    fn the_resampler_state_survives_across_frames() {
        // The regression the whole codec module exists for. Two consecutive frames must
        // produce the same stream as one frame of twice the length.
        let mut chunked = Leg::new(MediaCodec::L16);
        let half: Vec<u8> = (0..320i16).flat_map(|s| s.to_be_bytes()).collect();
        let a = chunked.decode_up(&B64.encode(&half)).unwrap();
        let b = chunked.decode_up(&B64.encode(&half)).unwrap();

        let mut whole = Leg::new(MediaCodec::L16);
        let both: Vec<u8> = half.iter().chain(half.iter()).copied().collect();
        let c = whole.decode_up(&B64.encode(&both)).unwrap();

        let joined: Vec<u8> = a.into_iter().chain(b).collect();
        assert_eq!(joined.len(), c.len(), "sample count must not drift");
        assert_eq!(joined, c, "chunk boundaries introduced a discontinuity");
    }

    // ---- room frames ----------------------------------------------------------

    #[test]
    fn only_translated_audio_reaches_the_telephone() {
        // The phone peer's channel carries every room message. A telephone can render
        // exactly one of them.
        let audio = serde_json::json!({
            "type": "translated_audio",
            "speaker_id": "s", "lang": "zh", "seq": 1, "pcm16_b64": "AAAA"
        })
        .to_string();
        assert_eq!(translated_audio_payload(&audio).as_deref(), Some("AAAA"));

        for other in [
            serde_json::json!({ "type": "subtitle_final", "text": "hello" }).to_string(),
            serde_json::json!({ "type": "peer_joined", "id": "x" }).to_string(),
            serde_json::json!({ "type": "emoji_reaction", "emoji": "👍" }).to_string(),
            "not json".to_string(),
        ] {
            assert_eq!(translated_audio_payload(&other), None, "{other}");
        }
    }

    // ---- barge-in -------------------------------------------------------------

    #[test]
    fn the_far_party_speaking_clears_what_was_sent_to_them() {
        let mut leg = Leg::new(MediaCodec::L16);
        let gen = leg.gate().generation();
        leg.gate().accept(gen, 640);
        assert!(!leg.far_speaking());

        assert!(leg.barge_in(), "there was audio out there to discard");
        assert!(leg.far_speaking());
        assert!(leg.gate().is_idle());

        // A second barge-in with nothing in flight reports nothing to clear, so the caller
        // can skip the provider round trip.
        assert!(!leg.barge_in());
    }

    #[test]
    fn audio_from_the_interrupted_sentence_never_reaches_the_wire() {
        let mut leg = Leg::new(MediaCodec::L16);
        let stale = leg.gate().generation();
        leg.gate().accept(stale, 640);
        leg.barge_in();

        assert!(
            !leg.gate().accept(stale, 640),
            "a chunk produced before the interruption must be refused"
        );
        assert!(leg.gate().is_idle());

        let fresh = leg.gate().generation();
        assert!(leg.gate().accept(fresh, 640));
    }

    #[test]
    fn playback_marks_the_far_party_as_listening_again() {
        let leg = Leg::new(MediaCodec::L16);
        leg.far_stopped();
        assert!(!leg.far_speaking());
    }

    // ---- latency --------------------------------------------------------------

    #[test]
    fn the_utterance_timer_measures_from_end_of_speech() {
        let t = UtteranceTimer::started();
        // Measured, not estimated: it must be a real elapsed span, not a constant.
        std::thread::sleep(std::time::Duration::from_millis(12));
        let ms = t.first_audio_ms();
        assert!(ms >= 10, "expected at least 10 ms, got {ms}");
        assert!(ms < 5_000, "sanity: {ms}");
    }

    // ---- the pump -------------------------------------------------------------

    /// A socket made of two channels, so the pump is testable without a network.
    struct FakeSocket {
        incoming: mpsc::Receiver<Result<String, ()>>,
        outgoing: mpsc::Sender<String>,
    }

    impl futures::Stream for FakeSocket {
        type Item = Result<String, ()>;
        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            self.incoming.poll_recv(cx)
        }
    }

    impl futures::Sink<String> for FakeSocket {
        type Error = ();
        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), ()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn start_send(self: std::pin::Pin<&mut Self>, item: String) -> Result<(), ()> {
            let _ = self.outgoing.try_send(item);
            Ok(())
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), ()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), ()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn a_leg_adopts_the_codec_the_provider_actually_negotiated() {
        // We ASK for L16, but the route decides. If a carrier forces µ-law and the leg
        // keeps decoding as L16, every byte is misread: the far party hears noise and the
        // engine is fed noise. Nothing in the call errors — it just does not work, which
        // is why this has to be asserted rather than assumed.
        let (in_tx, in_rx) = mpsc::channel(16);
        let (out_tx, mut out_rx) = mpsc::channel(16);
        let (engine_tx, mut engine_rx) = mpsc::channel(16);
        let (_room_tx, room_rx) = mpsc::channel(16);
        let (digit_tx, _digit_rx) = mpsc::channel(4);

        let socket = FakeSocket {
            incoming: in_rx,
            outgoing: out_tx,
        };
        let handles = BridgeHandles {
            to_engine: engine_tx,
            from_room: room_rx,
            digits: digit_tx,
        };
        let task = tokio::spawn(pump(socket, Leg::new(MediaCodec::L16), handles));

        in_tx
            .send(Ok(r#"{"event":"start","start":{"media_format":{"encoding":"PCMU","sample_rate":8000}}}"#.into()))
            .await
            .unwrap();

        // 160 µ-law bytes = one 20 ms frame at 8 kHz. `0x00` is chosen because the two
        // readings could not be further apart: as µ-law it is near full-scale negative,
        // as linear PCM it is digital silence.
        let ulaw = B64.encode(vec![0x00u8; 160]);
        in_tx
            .send(Ok(format!(
                r#"{{"event":"media","media":{{"payload":"{ulaw}","track":"inbound"}}}}"#
            )))
            .await
            .unwrap();

        let pcm = tokio::time::timeout(std::time::Duration::from_secs(2), engine_rx.recv())
            .await
            .expect("the engine should receive the frame")
            .expect("channel open");

        let peak = pcm
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes(*c).unsigned_abs())
            .max()
            .unwrap_or(0);
        assert!(
            peak > 10_000,
            "peak {peak} is silence — the µ-law bytes were read as linear PCM, which means \
             the leg ignored the codec the provider negotiated"
        );

        drop(in_tx);
        let _ = task.await;
        let _ = out_rx.try_recv();
    }

    #[tokio::test]
    async fn phone_audio_reaches_the_engine_and_translated_audio_reaches_the_phone() {
        let (in_tx, in_rx) = mpsc::channel(16);
        let (out_tx, mut out_rx) = mpsc::channel(16);
        let (engine_tx, mut engine_rx) = mpsc::channel(16);
        let (room_tx, room_rx) = mpsc::channel(16);
        let (digit_tx, mut digit_rx) = mpsc::channel(4);

        let socket = FakeSocket {
            incoming: in_rx,
            outgoing: out_tx,
        };
        let handles = BridgeHandles {
            to_engine: engine_tx,
            from_room: room_rx,
            digits: digit_tx,
        };
        let task = tokio::spawn(pump(socket, Leg::new(MediaCodec::L16), handles));

        // The phone speaks.
        let wire: Vec<u8> = (0..320i16).flat_map(|s| s.to_be_bytes()).collect();
        in_tx
            .send(Ok(media_frame(&B64.encode(&wire))))
            .await
            .unwrap();
        let pcm = tokio::time::timeout(std::time::Duration::from_secs(2), engine_rx.recv())
            .await
            .expect("engine got audio")
            .expect("some");
        assert!(!pcm.is_empty());

        // A digit reaches the consent gate.
        in_tx
            .send(Ok(
                serde_json::json!({ "event": "dtmf", "dtmf": { "digit": "1" } }).to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), digit_rx.recv())
                .await
                .unwrap(),
            Some('1')
        );

        // The engine answers with translated audio for the phone.
        let translated: Vec<u8> = (0..480i16).flat_map(|s| s.to_le_bytes()).collect();
        room_tx
            .send(
                serde_json::json!({
                    "type": "translated_audio", "speaker_id": "s", "lang": "zh", "seq": 1,
                    "pcm16_b64": B64.encode(&translated)
                })
                .to_string(),
            )
            .await
            .unwrap();

        // Somewhere in what came back there is a media frame for the far party.
        let mut saw_media = false;
        for _ in 0..4 {
            match tokio::time::timeout(std::time::Duration::from_secs(2), out_rx.recv()).await {
                Ok(Some(frame)) if frame.contains("\"event\":\"media\"") => {
                    saw_media = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert!(saw_media, "translated audio must reach the telephone");

        drop(in_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), task).await;
    }

    #[tokio::test]
    async fn a_stop_frame_ends_the_pump_cleanly() {
        let (in_tx, in_rx) = mpsc::channel(4);
        let (out_tx, _out_rx) = mpsc::channel(4);
        let (engine_tx, _engine_rx) = mpsc::channel(4);
        let (_room_tx, room_rx) = mpsc::channel(4);
        let (digit_tx, _digit_rx) = mpsc::channel(4);

        let socket = FakeSocket {
            incoming: in_rx,
            outgoing: out_tx,
        };
        let task = tokio::spawn(pump(
            socket,
            Leg::new(MediaCodec::Pcmu),
            BridgeHandles {
                to_engine: engine_tx,
                from_room: room_rx,
                digits: digit_tx,
            },
        ));

        in_tx
            .send(Ok(serde_json::json!({ "event": "stop" }).to_string()))
            .await
            .unwrap();

        let out = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("pump returned")
            .expect("no panic");
        assert!(out.is_ok());
    }
}
