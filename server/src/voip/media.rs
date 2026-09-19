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
// Far-party voice activity (barge-in)
// ---------------------------------------------------------------------------

/// Mean absolute amplitude above which a decoded inbound frame counts as speech, on the
/// codec's own linear PCM16 scale.
///
/// Telnyx sends inbound `media` frames continuously — every ~20 ms, including silence — so
/// "a frame arrived" cannot mean "the far party is speaking": every translated chunk we send
/// would be followed within 20 ms by a silent inbound frame, misread as the far party
/// starting to talk, and immediately cleared. Around 500 (~ -36 dBFS on a full-scale i16)
/// sits well above PSTN line noise and comfortably below real speech, so it discriminates
/// the two without needing per-carrier tuning.
const SPEECH_AMPLITUDE_THRESHOLD: i64 = 500;

/// How long a run of non-speech must last before we consider the far party done talking.
const SPEECH_HANGOVER_MS: u32 = 500;

/// Energy-based voice-activity state for the far party's inbound audio, used for barge-in.
///
/// Deciding "speaking" from decoded energy rather than frame arrival is the fix for the
/// barge-in bug: with a hangover of consecutive non-speech SAMPLES (not frames, so the
/// hangover duration does not depend on the wire codec's frame size), a brief dip mid-word
/// does not read as the sentence ending, and — crucially — nothing about receiving
/// *translated* audio for the far party has any bearing on whether *they* are speaking.
struct FarPartyVad {
    speaking: bool,
    /// Consecutive non-speech samples seen while `speaking`, at the wire rate.
    silence_run: u32,
    hangover_samples: u32,
    /// Set on a not-speaking → speaking transition; consumed and reset by
    /// [`Leg::take_speech_onset`]. Barge-in must fire once per onset, not once per frame for
    /// as long as the far party keeps talking.
    onset: bool,
}

impl FarPartyVad {
    fn new(wire_rate_hz: u32) -> Self {
        Self {
            speaking: false,
            silence_run: 0,
            hangover_samples: (wire_rate_hz as u64 * SPEECH_HANGOVER_MS as u64 / 1000) as u32,
            onset: false,
        }
    }

    /// Feed one frame's decoded samples, at the wire rate (i.e. before resampling to the
    /// engine rate — the wire rate is what the hangover is measured in).
    fn observe(&mut self, samples: &[i16]) {
        if samples.is_empty() {
            return;
        }
        let mean_abs: i64 =
            samples.iter().map(|s| s.unsigned_abs() as i64).sum::<i64>() / samples.len() as i64;
        if mean_abs > SPEECH_AMPLITUDE_THRESHOLD {
            if !self.speaking {
                self.speaking = true;
                self.onset = true;
            }
            self.silence_run = 0;
        } else if self.speaking {
            self.silence_run = self.silence_run.saturating_add(samples.len() as u32);
            if self.silence_run >= self.hangover_samples {
                self.speaking = false;
                self.silence_run = 0;
            }
        }
    }

    /// Report and reset the onset flag: true exactly once per not-speaking → speaking
    /// transition.
    fn take_onset(&mut self) -> bool {
        std::mem::take(&mut self.onset)
    }
}

// ---------------------------------------------------------------------------
// L16 byte-order detection
// ---------------------------------------------------------------------------

/// Minimum number of evidence-carrying frames before a byte-order decision is trusted. One
/// or two frames could tie by chance (e.g. a mostly-silent line); five makes a coin-flip
/// outcome implausible.
const BYTE_ORDER_MIN_EVIDENCE_FRAMES: u32 = 5;

/// A reading must be at least this much smoother (lower summed roughness) than the other to
/// be trusted as a decision rather than a photo finish that could flip on the next frame.
const BYTE_ORDER_DECISIVE_RATIO: u64 = 2;

/// Evidence-bearing frames (~1 s of real audio) after which a still-undecided window is
/// reported inconclusive and its accumulators are halved rather than zeroed — a persistent
/// asymmetry keeps converging, a tie decays, and the totals stay bounded over an hour-long
/// call. This is NOT a stop condition: an `Undetermined` leg keeps measuring for the life of
/// the call, gated on evidence-bearing frames rather than raw frames, because raw-frame
/// counting is exactly what let 5 seconds of digital silence exhaust the old fixed budget
/// before the far party ever said a word.
const BYTE_ORDER_INCONCLUSIVE_WINDOW: u32 = 50;

/// Diagnostics only, never a stop condition: one INFO line once a leg has produced too
/// little evidence (fewer than `BYTE_ORDER_MIN_EVIDENCE_FRAMES`) after this many raw inbound
/// frames — a mostly-silent leg, worth a log line, not a give-up.
const BYTE_ORDER_SILENT_LEG_FRAMES: u32 = 250;

/// Whether an L16 leg's wire byte order has been measured.
///
/// `Decided` is a ONE-WAY LATCH: a leg that has committed never measures again and never
/// changes its mind mid-call — that hysteresis is what makes a good call free of this cost
/// for the rest of its duration, and what prevents a healthy leg from ever flipping on a
/// later coincidental run of scrambled-looking evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ByteOrder {
    /// No trusted measurement yet. The RFC 3551 big-endian default is *in force* but NOT
    /// committed: every further frame is still measured.
    #[default]
    Undetermined,
    /// Measured and committed to little-endian (`true`) or big-endian (`false`).
    Decided(bool),
}

/// Accumulates L16 byte-order evidence across a leg's inbound frames and decides once.
///
/// Only meaningful for L16: µ-law and A-law have no byte-order ambiguity (one byte, one
/// sample), so those legs never call [`observe`](Self::observe) and this type's default
/// (`little_endian() == false`, i.e. keep the RFC 3551 big-endian assumption) is exactly the
/// unchanged behaviour for them.
///
/// **Tier equivalence (spec: "Tier-Uniform Media Pump Behavior").** This detector is owned
/// by [`Leg`], which is constructed from a negotiated [`MediaCodec`] alone
/// (`Leg::new(codec)`, called from exactly one site: `run_leg`'s single `media::pump(...)`
/// call in `voip::session`) — never from an engine or tier. Whether the call selected
/// Standard, Premium, or Enhanced (which `EngineRegistry::resolve_for_phone` substitutes to
/// Standard before a phone leg ever opens, per task 1.15's test) has no bearing on which
/// `ByteOrderDetector` instance a leg gets or how it behaves: there is no tier branch
/// anywhere between engine selection and this struct's construction or use.
#[derive(Debug, Default)]
struct ByteOrderDetector {
    state: ByteOrder,
    /// Evidence-bearing frames seen since the last halving (or since the start).
    evidence_frames: u32,
    /// Evidence-bearing frames seen within the current inconclusive-window count, reset on
    /// every halving. Distinct from `evidence_frames` only in name today, but kept separate
    /// so a future window size need not equal the ratio's own evidence floor.
    window_evidence_frames: u32,
    /// Raw inbound frames seen — diagnostics only (`BYTE_ORDER_SILENT_LEG_FRAMES`), never a
    /// stop condition.
    frames_seen: u32,
    be_total: u64,
    le_total: u64,
    /// How many inconclusive windows this leg has produced. Zero means either "still
    /// gathering evidence" or "decided before the first window elapsed"; non-zero is the
    /// "probably not L16" signal (see `note_inconclusive_window`).
    inconclusive_windows: u32,
    /// Whether the once-per-leg silent/undetermined diagnostics have already fired, so a
    /// long call logs each line exactly once rather than repeating it forever.
    noted_silent_leg: bool,
}

impl ByteOrderDetector {
    fn new() -> Self {
        Self::default()
    }

    /// Whether inbound/outbound L16 bytes should be byte-swapped before decode / after
    /// encode. `false` until a decision commits to little-endian.
    fn little_endian(&self) -> bool {
        matches!(self.state, ByteOrder::Decided(true))
    }

    /// Feed one inbound frame's RAW wire bytes, before any swap. A no-op once decided — the
    /// one-way latch — so a healthy leg computes nothing for the rest of the call.
    /// `codec_confirmed` is [`Leg::confirmed`] at the time of this frame, threaded through
    /// only so the inconclusive-window WARN can carry it: a persistent tie on an
    /// unconfirmed leg is the signature of a route that forced a different codec without
    /// ever sending a `start` frame.
    fn observe(&mut self, wire: &[u8], codec_confirmed: bool) {
        if matches!(self.state, ByteOrder::Decided(_)) {
            return;
        }
        self.frames_seen += 1;
        let ev = codec::l16_byte_order_evidence(wire);
        if ev.be != 0 || ev.le != 0 {
            self.evidence_frames += 1;
            self.window_evidence_frames += 1;
            self.be_total += ev.be;
            self.le_total += ev.le;
        }
        if self.evidence_frames >= BYTE_ORDER_MIN_EVIDENCE_FRAMES {
            if self.be_total >= self.le_total.saturating_mul(BYTE_ORDER_DECISIVE_RATIO) {
                self.decide(true);
                return;
            }
            if self.le_total >= self.be_total.saturating_mul(BYTE_ORDER_DECISIVE_RATIO) {
                self.decide(false);
                return;
            }
        }
        if self.window_evidence_frames >= BYTE_ORDER_INCONCLUSIVE_WINDOW {
            self.note_inconclusive_window(codec_confirmed);
            // Halve rather than zero: a persistent asymmetry keeps converging, a tie
            // decays, and the accumulators stay bounded over an hour-long call.
            self.be_total /= 2;
            self.le_total /= 2;
            self.evidence_frames /= 2;
            self.window_evidence_frames = 0;
        }
        if !self.noted_silent_leg
            && self.evidence_frames < BYTE_ORDER_MIN_EVIDENCE_FRAMES
            && self.frames_seen >= BYTE_ORDER_SILENT_LEG_FRAMES
        {
            self.noted_silent_leg = true;
            tracing::info!(
                frames_seen = self.frames_seen,
                evidence_frames = self.evidence_frames,
                "phone leg L16 byte order still undetermined after a mostly-silent start; \
                 continuing to measure for the rest of the call"
            );
        }
    }

    fn decide(&mut self, little_endian: bool) {
        self.state = ByteOrder::Decided(little_endian);
        crate::metrics::record_voip_byte_order_decided(little_endian);
        // This log line is how the next production call tells us the truth: byte order is
        // undocumented by the provider, so a field here is worth more than a guess in code.
        tracing::info!(
            byte_order = if little_endian {
                "little-endian"
            } else {
                "big-endian"
            },
            "phone leg L16 byte order detected"
        );
    }

    /// A window of `BYTE_ORDER_INCONCLUSIVE_WINDOW` evidence-bearing frames produced no
    /// decisive ratio in either direction — evidence arrived and NEITHER reading won. This
    /// is observably distinct from "not enough evidence yet" (see `observe`'s silent-leg
    /// diagnostic): a persistent tie is the signature of a payload that may not be linear
    /// PCM at all (e.g. µ-law misread as L16 on a route that never confirmed capture
    /// format), which `codec_confirmed` helps a log reader distinguish from noise.
    fn note_inconclusive_window(&mut self, codec_confirmed: bool) {
        self.inconclusive_windows += 1;
        crate::metrics::record_voip_byte_order_inconclusive_window();
        if self.inconclusive_windows == 1 {
            tracing::warn!(
                be_total = self.be_total,
                le_total = self.le_total,
                ratio = BYTE_ORDER_DECISIVE_RATIO,
                evidence_frames = self.evidence_frames,
                codec_confirmed,
                "phone leg L16 byte order evidence is a persistent tie; neither big-endian \
                 nor little-endian is winning — this may mean the payload is not linear PCM"
            );
        }
    }

    /// One summary line per leg, called from teardown on every exit path (see `pump`'s
    /// async-block restructure). Distinguishes three outcomes that used to collapse into
    /// the same runtime state and the same log line: "measured" (`Decided`), "silent line"
    /// (`Undetermined`, no inconclusive windows), and "probably not L16" (`Undetermined`,
    /// at least one inconclusive window).
    ///
    /// `is_l16` is the leg's codec AT TEARDOWN TIME (`Leg::codec() == MediaCodec::L16`),
    /// not whether this detector ever observed anything. It gates the `Undetermined`
    /// metric specifically: a µ-law/A-law leg never calls [`observe`](Self::observe), so
    /// it is permanently `Undetermined` by construction and recording it would make the
    /// counter answer "how many legs are not L16" instead of its documented question
    /// ("did an L16 leg ever confirm its byte order"). The same guard also prevents
    /// double-counting a leg that decided L16 and was later renegotiated away from L16:
    /// the detector resets to `Undetermined` (see [`Leg::renegotiate`]), but `is_l16` is
    /// now false, so teardown does not recount it. Returns whether this call recorded the
    /// `Undetermined` metric, so tests can assert the guard without touching global state.
    fn log_summary(&self, is_l16: bool) -> bool {
        match self.state {
            ByteOrder::Decided(little_endian) => {
                tracing::info!(
                    byte_order = if little_endian {
                        "little-endian"
                    } else {
                        "big-endian"
                    },
                    frames_seen = self.frames_seen,
                    "phone leg L16 byte order summary: decided"
                );
                false
            }
            ByteOrder::Undetermined => {
                tracing::info!(
                    frames_seen = self.frames_seen,
                    evidence_frames = self.evidence_frames,
                    inconclusive_windows = self.inconclusive_windows,
                    "phone leg L16 byte order summary: undetermined"
                );
                if is_l16 {
                    crate::metrics::record_voip_byte_order_undetermined();
                    true
                } else {
                    false
                }
            }
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
    /// Energy-based speech detection on the far party's inbound audio, for barge-in.
    vad: FarPartyVad,
    /// L16-only: which byte order this leg's wire audio actually uses, detected from the
    /// audio itself because Telnyx does not document it. A no-op for every other codec.
    byte_order: ByteOrderDetector,
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
            vad: FarPartyVad::new(wire),
            byte_order: ByteOrderDetector::new(),
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
        // Both the VAD's hangover (in samples at the wire rate) and any in-progress L16
        // byte-order evidence are meaningless for the new codec's own wire format.
        self.vad = FarPartyVad::new(wire);
        self.byte_order = ByteOrderDetector::new();
        true
    }

    /// Wire audio from the phone → PCM16 at the engine's rate.
    ///
    /// Also where the far party's voice-activity state advances: the decoded samples are at
    /// the wire rate, which is exactly what the VAD's hangover is measured in, so there is
    /// no reason to do this anywhere else. Check [`take_speech_onset`](Self::take_speech_onset)
    /// after calling this to learn whether THIS call started a new utterance.
    pub fn decode_up(&mut self, payload_b64: &str) -> Result<Vec<u8>, MediaError> {
        let bytes = B64
            .decode(payload_b64)
            .map_err(|e| MediaError::Malformed(e.to_string()))?;
        let bytes = if self.codec == MediaCodec::L16 {
            // Evidence is accumulated on the raw wire bytes, before any swap: swapping
            // first would feed the detector its own correction and it could never see the
            // roughness that justified deciding in the first place.
            self.byte_order.observe(&bytes, self.confirmed);
            if self.byte_order.little_endian() {
                codec::swap_byte_pairs(&bytes)
            } else {
                bytes
            }
        } else {
            bytes
        };
        let samples = codec::decode(self.codec, &bytes)
            .map_err(|e| MediaError::Malformed(format!("{e:?}")))?;
        self.vad.observe(&samples);
        let resampled = self.up.process(&samples);
        Ok(resampled.iter().flat_map(|s| s.to_le_bytes()).collect())
    }

    /// Translated PCM16 from the engine → wire audio for the phone.
    ///
    /// The engine emits little-endian PCM16 (the browser's format); the wire wants the
    /// codec's own — byte-swapped afterwards if this L16 leg was detected to actually be
    /// little-endian on the wire. Both conversions live here so neither can be forgotten at
    /// a call site.
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
        let wire = if self.codec == MediaCodec::L16 && self.byte_order.little_endian() {
            codec::swap_byte_pairs(&wire)
        } else {
            wire
        };
        Ok(B64.encode(wire))
    }

    pub fn gate(&mut self) -> &mut PlaybackGate {
        &mut self.gate
    }

    /// Report and reset whether the far party just transitioned from not-speaking to
    /// speaking, per the most recent [`decode_up`](Self::decode_up) call. This is the
    /// barge-in trigger: true exactly once per onset, not once per frame while they keep
    /// talking, and never as a side effect of translated audio being sent to them.
    pub fn take_speech_onset(&mut self) -> bool {
        self.vad.take_onset()
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

    /// One byte-order summary line for this leg, called from `pump`'s teardown on every
    /// exit path — not only the normal `break`s, but also a `MediaError` early return. A
    /// leg that never observed L16 evidence at all (µ-law, A-law, or a leg that decided
    /// before this call) still gets a summary; a `Decided` leg's summary is cheap because
    /// [`ByteOrderDetector::observe`] already stopped touching it.
    ///
    /// The leg's CURRENT codec (`self.codec`), not whether the detector ever ran, decides
    /// whether an `Undetermined` outcome is metric-worthy — see
    /// [`ByteOrderDetector::log_summary`]. Returns whether the `Undetermined` metric was
    /// recorded, for tests.
    pub fn log_byte_order_summary(&self) -> bool {
        self.byte_order.log_summary(self.codec == MediaCodec::L16)
    }
}

/// Why [`pump`] returned on the `Ok` path.
///
/// The two reasons look identical from the outside — both end the loop and both leave the
/// telephone unreachable — but `run_leg` must treat them very differently. A provider-
/// ended stream is what a completely ordinary hangup looks like from here (see the module
/// doc on `voip::session` for the observed ordering against the carrier's hangup webhook),
/// so failing the call on sight races that webhook. A room-ended stream means the peer was
/// removed out from under a socket that is still open — the room made the decision, not
/// the carrier, and there is no webhook coming to settle it, so ending the call
/// immediately is correct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpExit {
    /// The provider ended its media stream: the socket closed (`None`, or an error we do
    /// not otherwise distinguish), or it sent an `Inbound::Stop` frame.
    ProviderEnded,
    /// The room side ended: the phone peer's `from_room` channel closed.
    RoomEnded,
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
/// a WebSocket, a provider or a network. The `Ok` value reports WHY the pump ended — see
/// [`PumpExit`] — so the caller can tell a provider-ended stream from a room-ended one
/// instead of treating every non-error return the same way.
pub async fn pump<S, E>(
    mut socket: S,
    mut leg: Leg,
    mut handles: BridgeHandles,
) -> Result<PumpExit, MediaError>
where
    S: futures::Sink<String, Error = E> + futures::Stream<Item = Result<String, E>> + Unpin,
{
    // Wrapped in its own async block — rather than driven directly by `pump`'s own
    // `Result` return — so a `?` anywhere inside (a parse or codec error mid-loop) still
    // runs `leg.log_byte_order_summary()` below before `pump` returns, exactly like every
    // other exit. Without this, the teardown summary only fired on the loop's normal
    // `break` paths, silently skipping the diagnostics on the one exit that most needs
    // them: a call that errored out mid-stream.
    let exit: Result<PumpExit, MediaError> = async {
        Ok(loop {
        tokio::select! {
            // Audio and control from the phone.
            incoming = socket.next() => {
                let Some(Ok(raw)) = incoming else { break PumpExit::ProviderEnded };
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
                        let pcm = leg.decode_up(&payload_b64)?;
                        // Speaking again mid-playback: drop what is queued for them AND
                        // tell the provider to drop what it already buffered. Without the
                        // second half the caller hears the sentence they interrupted
                        // finish anyway, which reads as being ignored. Gated on the VAD's
                        // ONSET (decided from this frame's decoded energy), not on frame
                        // arrival — Telnyx sends inbound frames continuously, including
                        // silence, so "a frame arrived" is not "they are speaking".
                        if leg.take_speech_onset() && leg.gate().barge_in() {
                            let _ = socket.send(json(&Outbound::Clear)).await;
                        }
                        // A full channel means the engine is behind. Dropping is right:
                        // back-pressuring a live phone call would stall the socket and the
                        // far party would hear a gap growing without bound.
                        let _ = handles.to_engine.try_send(pcm);
                    }
                    Inbound::Dtmf { digit } => {
                        let _ = handles.digits.try_send(digit);
                    }
                    Inbound::Stop => break PumpExit::ProviderEnded,
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
                let Some(frame) = frame else { break PumpExit::RoomEnded };
                if let Some(pcm_b64) = translated_audio_payload(&frame) {
                    // Sending the far party translated audio says nothing about whether
                    // THEY are speaking — that used to be conflated here, and it is the
                    // root cause of the original barge-in bug (see the VAD above).
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
        })
    }
    .await;
    leg.log_byte_order_summary();
    exit
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

    /// A 440 Hz tone at the given amplitude (0.0-1.0), as PCM16 samples.
    fn sine_i16(rate_hz: f32, n: usize, amp: f32) -> Vec<i16> {
        (0..n)
            .map(|i| {
                ((2.0 * std::f32::consts::PI * 440.0 * i as f32 / rate_hz).sin() * amp * 32767.0)
                    as i16
            })
            .collect()
    }

    /// One 20 ms L16 frame (320 samples at 16 kHz) loud enough to register as speech —
    /// well above [`SPEECH_AMPLITUDE_THRESHOLD`].
    fn loud_be_frame() -> Vec<u8> {
        sine_i16(16_000.0, 320, 0.3)
            .iter()
            .flat_map(|s| s.to_be_bytes())
            .collect()
    }

    fn silent_l16_frame() -> Vec<u8> {
        vec![0u8; 640] // 320 samples, all zero — silence reads the same either byte order.
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

        // A loud inbound frame is the far party starting to talk — the ONSET, decided from
        // this frame's decoded energy rather than from the frame merely having arrived.
        leg.decode_up(&B64.encode(loud_be_frame())).unwrap();
        assert!(
            leg.take_speech_onset(),
            "a loud frame after silence is an onset"
        );
        assert!(
            leg.gate().barge_in(),
            "there was audio out there to discard"
        );
        assert!(leg.gate().is_idle());

        // The far party continuing to talk is not a second onset, so a second barge-in
        // check finds nothing new to clear.
        leg.decode_up(&B64.encode(loud_be_frame())).unwrap();
        assert!(
            !leg.take_speech_onset(),
            "still the same utterance, not a new one"
        );
    }

    #[test]
    fn audio_from_the_interrupted_sentence_never_reaches_the_wire() {
        let mut leg = Leg::new(MediaCodec::L16);
        let stale = leg.gate().generation();
        leg.gate().accept(stale, 640);
        leg.gate().barge_in();

        assert!(
            !leg.gate().accept(stale, 640),
            "a chunk produced before the interruption must be refused"
        );
        assert!(leg.gate().is_idle());

        let fresh = leg.gate().generation();
        assert!(leg.gate().accept(fresh, 640));
    }

    #[test]
    fn far_party_stops_speaking_only_after_the_hangover_not_when_playback_arrives() {
        // THE regression: this leg used to be told the far party stopped speaking whenever
        // TRANSLATED audio arrived for them — nothing to do with whether they were actually
        // still talking. A translated chunk sent moments after they started meant the very
        // next silent inbound frame (at most 20 ms later) read as a fresh onset and cleared
        // the chunk that had just gone out. There is no call left in the public API that
        // lets playback influence this at all; speaking now ends only after a run of silent
        // SAMPLES on the inbound leg.
        let mut leg = Leg::new(MediaCodec::L16);
        leg.decode_up(&B64.encode(loud_be_frame())).unwrap();
        assert!(leg.take_speech_onset());

        // One 20 ms silent frame is far short of the ~500 ms hangover.
        leg.decode_up(&B64.encode(silent_l16_frame())).unwrap();
        assert!(
            !leg.take_speech_onset(),
            "still within the hangover, not a new utterance yet"
        );

        // Enough consecutive silence crosses the hangover (16 kHz / 2 = 8000 samples, i.e.
        // 25 frames of 320 samples; comfortably exceeded here).
        for _ in 0..30 {
            leg.decode_up(&B64.encode(silent_l16_frame())).unwrap();
        }
        leg.decode_up(&B64.encode(loud_be_frame())).unwrap();
        assert!(
            leg.take_speech_onset(),
            "speaking resumed after the hangover elapsed, so this is a genuine new onset"
        );
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

    // ---- L16 byte order ---------------------------------------------------------

    #[test]
    fn a_leg_detects_little_endian_wire_audio_from_its_own_energy() {
        let mut leg = Leg::new(MediaCodec::L16);

        // Feed enough frames of a genuinely little-endian tone to cross the detector's
        // minimum evidence and decisive-ratio thresholds — a clean stand-in for real
        // speech, which reads far smoother one way than the other.
        let mut last_pcm = Vec::new();
        for _ in 0..10 {
            let le: Vec<u8> = sine_i16(16_000.0, 320, 0.4)
                .iter()
                .flat_map(|s| s.to_le_bytes())
                .collect();
            last_pcm = leg.decode_up(&B64.encode(le)).unwrap();
        }

        // The decoded engine PCM (little-endian i16 @ 24 kHz) is a smooth tone, not
        // full-scale garbage — which is what reading it as big-endian would have produced.
        let engine: Vec<i16> = last_pcm
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes(*c))
            .collect();
        let peak = engine.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        assert!(
            peak < 20_000,
            "peak {peak} should track the ~0.4-amplitude tone, not clip to full-scale garbage"
        );
        let mean_jump: f32 = engine
            .windows(2)
            .map(|w| (w[1] as i32 - w[0] as i32).unsigned_abs() as f32)
            .sum::<f32>()
            / (engine.len().saturating_sub(1).max(1)) as f32;
        assert!(
            mean_jump < peak as f32 * 0.5,
            "a correctly-decoded tone moves smoothly sample to sample; mean_jump {mean_jump} peak {peak}"
        );

        // And encode_down now emits little-endian wire bytes for the phone.
        let steady: i16 = 1000;
        let engine_pcm: Vec<u8> = (0..200).flat_map(|_| steady.to_le_bytes()).collect();
        let wire = B64.decode(leg.encode_down(&engine_pcm).unwrap()).unwrap();
        let tail = &wire[wire.len() - 8..];
        let le_val = i16::from_le_bytes([tail[0], tail[1]]);
        assert!(
            (le_val as i32 - steady as i32).abs() < 100,
            "expected ~{steady} reading the tail of the wire bytes as little-endian, got {le_val}"
        );
    }

    #[test]
    fn a_leg_keeps_big_endian_when_the_audio_is_already_big_endian() {
        let mut leg = Leg::new(MediaCodec::L16);
        for _ in 0..10 {
            leg.decode_up(&B64.encode(loud_be_frame())).unwrap();
        }
        let steady: i16 = 1000;
        let engine_pcm: Vec<u8> = (0..200).flat_map(|_| steady.to_le_bytes()).collect();
        let wire = B64.decode(leg.encode_down(&engine_pcm).unwrap()).unwrap();
        let tail = &wire[wire.len() - 8..];
        // Unswapped: reading the tail as BIG-endian recovers the steady value.
        let be_val = i16::from_be_bytes([tail[0], tail[1]]);
        assert!(
            (be_val as i32 - steady as i32).abs() < 100,
            "expected ~{steady} reading the tail of the wire bytes as big-endian, got {be_val}"
        );
    }

    #[test]
    fn a_leg_that_starts_silent_still_detects_byte_order_when_speech_arrives() {
        // THE production reproduction: 300 silent frames (past today's 250-frame cap)
        // then loud little-endian speech. The old `gave_up` latch would have permanently
        // committed to big-endian after frame 250, regardless of what evidence arrived
        // afterward — freezing the leg for the rest of the call.
        let mut leg = Leg::new(MediaCodec::L16);
        for _ in 0..300 {
            leg.decode_up(&B64.encode(silent_l16_frame())).unwrap();
        }
        for _ in 0..10 {
            let le: Vec<u8> = sine_i16(16_000.0, 320, 0.4)
                .iter()
                .flat_map(|s| s.to_le_bytes())
                .collect();
            leg.decode_up(&B64.encode(le)).unwrap();
        }

        let steady: i16 = 1000;
        let engine_pcm: Vec<u8> = (0..200).flat_map(|_| steady.to_le_bytes()).collect();
        let wire = B64.decode(leg.encode_down(&engine_pcm).unwrap()).unwrap();
        let tail = &wire[wire.len() - 8..];
        let le_val = i16::from_le_bytes([tail[0], tail[1]]);
        assert!(
            (le_val as i32 - steady as i32).abs() < 100,
            "the leg should have decided little-endian once real speech arrived, \
             not stayed permanently latched to big-endian after 300 silent frames; got {le_val}"
        );
    }

    #[test]
    fn an_undetermined_leg_is_observably_distinct_from_a_big_endian_decision() {
        // After silence only, the leg must be structurally `Undetermined`, NOT
        // `Decided(false)` — the two used to collapse into the same runtime state
        // (`gave_up = true`, permanent big-endian fallback) and the same log line.
        let mut leg = Leg::new(MediaCodec::L16);
        for _ in 0..300 {
            leg.decode_up(&B64.encode(silent_l16_frame())).unwrap();
        }
        assert_eq!(leg.byte_order.state, ByteOrder::Undetermined);
        assert!(!leg.byte_order.little_endian());
        // The summary must not claim a decision was reached.
        assert_eq!(leg.byte_order.inconclusive_windows, 0);
    }

    #[test]
    fn a_payload_where_neither_reading_wins_is_reported_as_its_own_case() {
        // Bytes that read equally rough (or equally smooth) in both directions for at
        // least one whole inconclusive window must be reported distinctly from the
        // silent-leg case above: `inconclusive_windows > 0`, still `Undetermined`.
        let mut leg = Leg::new(MediaCodec::L16);
        // A deterministic pseudo-random byte sequence: neither the big-endian nor the
        // little-endian reading of noise like this is coherent, so both readings produce
        // large, similarly-sized roughness — evidence arrives, but neither wins the 2x
        // decisive ratio, which is exactly "probably not L16".
        let frame: Vec<u8> = (0..640u32).map(|i| ((i * 137 + 51) % 256) as u8).collect();
        for _ in 0..(BYTE_ORDER_INCONCLUSIVE_WINDOW as usize + 5) {
            leg.decode_up(&B64.encode(&frame)).unwrap();
        }
        assert!(
            leg.byte_order.inconclusive_windows > 0,
            "a persistent tie must be counted as at least one inconclusive window"
        );
        assert_eq!(leg.byte_order.state, ByteOrder::Undetermined);
    }

    #[test]
    fn a_decided_leg_never_changes_its_mind() {
        // The one-way latch: once decided, 500 frames of the OPPOSITE evidence must not
        // flip the decision, and `observe` must have early-returned (no accumulation).
        let mut leg = Leg::new(MediaCodec::L16);
        for _ in 0..10 {
            let le: Vec<u8> = sine_i16(16_000.0, 320, 0.4)
                .iter()
                .flat_map(|s| s.to_le_bytes())
                .collect();
            leg.decode_up(&B64.encode(le)).unwrap();
        }
        assert_eq!(leg.byte_order.state, ByteOrder::Decided(true));
        let totals_before = (leg.byte_order.be_total, leg.byte_order.le_total);

        for _ in 0..500 {
            leg.decode_up(&B64.encode(loud_be_frame())).unwrap();
        }
        assert_eq!(
            leg.byte_order.state,
            ByteOrder::Decided(true),
            "a decided leg must never change its mind"
        );
        assert_eq!(
            (leg.byte_order.be_total, leg.byte_order.le_total),
            totals_before,
            "observe() must early-return once decided — no further accumulation"
        );
    }

    #[test]
    fn both_directions_flip_together_at_the_moment_of_decision() {
        // Extends the existing swap-direction pair for a decision reached LATE in the
        // leg (after many silent frames), rather than within the first 10 frames: both
        // `decode_up` (inbound) and `encode_down` (outbound) must consult the same
        // `ByteOrder` state consistently once it commits.
        let mut leg = Leg::new(MediaCodec::L16);
        for _ in 0..300 {
            leg.decode_up(&B64.encode(silent_l16_frame())).unwrap();
        }
        assert_eq!(leg.byte_order.state, ByteOrder::Undetermined);
        for _ in 0..10 {
            let le: Vec<u8> = sine_i16(16_000.0, 320, 0.4)
                .iter()
                .flat_map(|s| s.to_le_bytes())
                .collect();
            leg.decode_up(&B64.encode(le)).unwrap();
        }
        assert_eq!(leg.byte_order.state, ByteOrder::Decided(true));

        // Outbound: encode_down must now swap to emit little-endian wire bytes.
        let steady: i16 = 1000;
        let engine_pcm: Vec<u8> = (0..200).flat_map(|_| steady.to_le_bytes()).collect();
        let wire = B64.decode(leg.encode_down(&engine_pcm).unwrap()).unwrap();
        let tail = &wire[wire.len() - 8..];
        let le_val = i16::from_le_bytes([tail[0], tail[1]]);
        assert!((le_val as i32 - steady as i32).abs() < 100);

        // Inbound: a later frame must also be read with the swap applied.
        let le: Vec<u8> = sine_i16(16_000.0, 320, 0.4)
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let out = leg.decode_up(&B64.encode(&le)).unwrap();
        let engine: Vec<i16> = out
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes(*c))
            .collect();
        let peak = engine.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        assert!(
            peak < 20_000,
            "a late-arriving frame must still be read with the committed swap; peak {peak}"
        );
    }

    #[test]
    fn renegotiation_resets_the_detector_to_undetermined() {
        let mut leg = Leg::new(MediaCodec::L16);
        for _ in 0..10 {
            let le: Vec<u8> = sine_i16(16_000.0, 320, 0.4)
                .iter()
                .flat_map(|s| s.to_le_bytes())
                .collect();
            leg.decode_up(&B64.encode(le)).unwrap();
        }
        assert_eq!(leg.byte_order.state, ByteOrder::Decided(true));
        leg.renegotiate(MediaCodec::Pcmu);
        // A non-L16 codec never observes at all, so the reset state must be Undetermined.
        assert_eq!(leg.byte_order.state, ByteOrder::Undetermined);
    }

    #[test]
    fn the_accumulators_stay_bounded_over_a_long_call() {
        // ~50,000 inconclusive evidence frames must not overflow or panic, and the leg
        // must remain Undetermined (the halving window keeps this bounded).
        let mut leg = Leg::new(MediaCodec::L16);
        let frame: Vec<u8> = (0..640u32).map(|i| ((i * 137 + 51) % 256) as u8).collect();
        for _ in 0..50_000 {
            leg.decode_up(&B64.encode(&frame)).unwrap();
        }
        assert_eq!(leg.byte_order.state, ByteOrder::Undetermined);
        assert!(leg.byte_order.inconclusive_windows > 0);
    }

    #[test]
    fn a_mu_law_leg_never_attempts_byte_order_detection() {
        // µ-law has no byte-order ambiguity: one byte, one sample. Feeding it audio that
        // would be extremely decisive evidence for an L16 leg must have zero effect.
        let mut leg = Leg::new(MediaCodec::Pcmu);
        for _ in 0..20 {
            leg.decode_up(&B64.encode(vec![0x00u8; 160])).unwrap();
        }
        // No panic and no observable behaviour change is the whole assertion here: there is
        // no byte order to detect, so nothing about encode_down's shape should differ from
        // the untouched codec round trip already covered by `a_mu_law_leg_converts_both_ways`.
        let pcm: Vec<u8> = vec![0u8; 960];
        assert!(leg.encode_down(&pcm).is_ok());
    }

    #[test]
    fn a_non_l16_leg_does_not_pollute_the_undetermined_metric() {
        // A µ-law leg never calls `observe`, so it is permanently `Undetermined` by
        // construction. `voxtranslate_voip_byte_order_undetermined_total` documents
        // "L16 phone legs that ended their call without ever confirming a byte order" —
        // a µ-law leg's teardown must not count against that question at all.
        let leg = Leg::new(MediaCodec::Pcmu);
        assert!(
            !leg.log_byte_order_summary(),
            "a non-L16 leg's teardown must not record the undetermined metric"
        );
    }

    #[test]
    fn a_leg_renegotiated_away_from_l16_does_not_recount_as_undetermined() {
        // A leg that decided L16 and was later renegotiated to a different codec resets
        // its detector to `Undetermined` (see `renegotiate`), but it is no longer an L16
        // leg by the time teardown runs — it must not be recounted against the same
        // metric a second time.
        let mut leg = Leg::new(MediaCodec::L16);
        for _ in 0..10 {
            let le: Vec<u8> = sine_i16(16_000.0, 320, 0.4)
                .iter()
                .flat_map(|s| s.to_le_bytes())
                .collect();
            leg.decode_up(&B64.encode(le)).unwrap();
        }
        assert_eq!(leg.byte_order.state, ByteOrder::Decided(true));
        leg.renegotiate(MediaCodec::Pcmu);
        assert_eq!(leg.byte_order.state, ByteOrder::Undetermined);
        assert!(
            !leg.log_byte_order_summary(),
            "a leg renegotiated away from L16 must not be recounted as an undetermined L16 leg"
        );
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
        assert_eq!(
            out,
            Ok(PumpExit::ProviderEnded),
            "a Stop frame is the provider ending the stream, not the room"
        );
    }

    #[tokio::test]
    async fn the_room_channel_closing_ends_the_pump_with_the_room_ended_reason() {
        // The complement of the Stop-frame case above: the SOCKET is still open (the phone
        // side never sent anything) but the room removed this peer's channel out from
        // under it — e.g. the room closed, or the call was reclaimed. `run_leg` must be
        // able to tell the two apart: only one of them has a carrier hangup webhook coming.
        let (in_tx, in_rx) = mpsc::channel(4);
        let (out_tx, _out_rx) = mpsc::channel(4);
        let (engine_tx, _engine_rx) = mpsc::channel(4);
        let (room_tx, room_rx) = mpsc::channel(4);
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

        drop(room_tx);

        let out = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("pump returned")
            .expect("no panic");
        assert_eq!(out, Ok(PumpExit::RoomEnded));

        drop(in_tx);
    }

    #[tokio::test]
    async fn continuous_silent_inbound_frames_never_clear_translated_audio() {
        // THE regression this fix exists for. Telnyx sends inbound `media` frames
        // continuously, including silence, every ~20 ms. The old barge-in check fired on
        // FRAME ARRIVAL rather than on decoded energy, so the silent frame that always
        // followed a translated chunk within 20 ms read as the far party interrupting and
        // cleared what had just been sent — every chunk was cut to a few ms.
        let (in_tx, in_rx) = mpsc::channel(64);
        let (out_tx, mut out_rx) = mpsc::channel(64);
        let (engine_tx, mut engine_rx) = mpsc::channel(64);
        let (room_tx, room_rx) = mpsc::channel(64);
        let (digit_tx, _digit_rx) = mpsc::channel(4);

        let socket = FakeSocket {
            incoming: in_rx,
            outgoing: out_tx,
        };
        let handles = BridgeHandles {
            to_engine: engine_tx.clone(),
            from_room: room_rx,
            digits: digit_tx,
        };
        let task = tokio::spawn(pump(socket, Leg::new(MediaCodec::L16), handles));

        let silence = media_frame(&B64.encode(silent_l16_frame()));
        let mut media_frames_seen = 0u32;
        let mut clear_frames_seen = 0u32;

        for seq in 0..5u32 {
            in_tx.send(Ok(silence.clone())).await.unwrap();
            // Let the pump actually process the silent frame before the translated chunk,
            // rather than racing both into the select! at once.
            let _ =
                tokio::time::timeout(std::time::Duration::from_millis(50), engine_rx.recv()).await;

            let translated: Vec<u8> = (0..480i16)
                .map(|s| s.wrapping_add(seq as i16))
                .flat_map(|s| s.to_le_bytes())
                .collect();
            room_tx
                .send(
                    serde_json::json!({
                        "type": "translated_audio", "speaker_id": "s", "lang": "zh",
                        "seq": seq, "pcm16_b64": B64.encode(&translated)
                    })
                    .to_string(),
                )
                .await
                .unwrap();

            // Drain whatever came back before starting the next iteration.
            while let Ok(Some(frame)) =
                tokio::time::timeout(std::time::Duration::from_millis(100), out_rx.recv()).await
            {
                if frame.contains("\"event\":\"media\"") {
                    media_frames_seen += 1;
                }
                if frame.contains("\"event\":\"clear\"") {
                    clear_frames_seen += 1;
                }
            }
        }

        assert_eq!(
            clear_frames_seen, 0,
            "continuous silence must never trigger a clear"
        );
        assert_eq!(
            media_frames_seen, 5,
            "every translated chunk must reach the wire"
        );

        drop(in_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), task).await;
    }

    /// The count for a labelless Prometheus counter line in one `render()` snapshot.
    fn count_metric(out: &str, name: &str) -> u64 {
        out.lines()
            .find(|l| l.starts_with(&format!("{name} ")))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn the_leg_teardown_summary_fires_on_the_media_error_exit_path() {
        // The teardown summary must fire on the `?` early-return path too, not only on
        // the loop's normal `break` exits. Observed via the `undetermined` counter (this
        // leg never sees enough evidence to decide), which only `log_byte_order_summary`
        // increments.
        let metric = "voxtranslate_voip_byte_order_undetermined_total";
        let before = count_metric(&crate::metrics::render(0, 0), metric);

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
            Leg::new(MediaCodec::L16),
            BridgeHandles {
                to_engine: engine_tx,
                from_room: room_rx,
                digits: digit_tx,
            },
        ));

        // A "media" event with no `media` object is malformed: `parse_inbound(&raw)?`
        // early-returns out of `pump` via `?`, WITHOUT reaching the loop's normal `break`.
        in_tx
            .send(Ok(serde_json::json!({ "event": "media" }).to_string()))
            .await
            .unwrap();

        let out = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("pump returned")
            .expect("no panic");
        assert!(
            matches!(out, Err(MediaError::Malformed(_))),
            "expected the parse error to propagate: {out:?}"
        );

        let after = count_metric(&crate::metrics::render(0, 0), metric);
        // `>`, not `==`: this counter is process-global, and `cargo test` runs other
        // pump-driven tests in this same module concurrently in the same binary — any of
        // them ending in `Undetermined` can bump it between the two snapshots. This
        // assertion only needs to know that THIS leg's teardown fired at least once.
        assert!(
            after > before,
            "the teardown summary must fire on the `?` early-return path too: before={before}, after={after}"
        );
    }

    #[tokio::test]
    async fn a_loud_inbound_frame_after_playback_clears_exactly_once() {
        let (in_tx, in_rx) = mpsc::channel(64);
        let (out_tx, mut out_rx) = mpsc::channel(64);
        let (engine_tx, _engine_rx) = mpsc::channel(64);
        let (room_tx, room_rx) = mpsc::channel(64);
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

        // Translated audio reaches the phone first, so there is something to discard.
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

        let mut saw_media = false;
        for _ in 0..4 {
            match tokio::time::timeout(std::time::Duration::from_secs(1), out_rx.recv()).await {
                Ok(Some(frame)) if frame.contains("\"event\":\"media\"") => {
                    saw_media = true;
                    break;
                }
                _ => continue,
            }
        }
        assert!(
            saw_media,
            "setup: translated audio must reach the wire before the interruption"
        );

        // The phone interrupts: several loud frames in a row are one onset, not several.
        let loud = media_frame(&B64.encode(loud_be_frame()));
        for _ in 0..3 {
            in_tx.send(Ok(loud.clone())).await.unwrap();
        }

        let mut clears = 0u32;
        loop {
            match tokio::time::timeout(std::time::Duration::from_millis(200), out_rx.recv()).await {
                Ok(Some(frame)) if frame.contains("\"event\":\"clear\"") => clears += 1,
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert_eq!(
            clears, 1,
            "only the onset of speech should clear, not every loud frame"
        );

        drop(in_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), task).await;
    }
}
