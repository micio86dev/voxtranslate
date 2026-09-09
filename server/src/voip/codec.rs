//! Telephone audio ⇄ engine audio (spec 0111, R15/R16).
//!
//! The engines want linear PCM16 at 24 kHz (spec 0043). The telephone gives us 16 kHz
//! L16, or 8 kHz µ-law when the route forces it. Two things happen here, and both have a
//! failure mode that sounds like "the product is bad" rather than like a bug:
//!
//! **1. Companding.** µ-law is a logarithmic 8-bit encoding. Decoding it with the wrong
//! table does not error — it produces audible-but-wrong audio that STT then transcribes
//! badly, and the symptom looks like a poor model.
//!
//! **2. Resampling, and it must be STATEFUL.** Resampling each 20 ms chunk independently
//! is the classic mistake: every chunk boundary becomes a discontinuity, so the stream
//! carries a click at 50 Hz. It is quiet enough to survive a listen and loud enough to
//! wreck STT. [`Resampler`] therefore carries its filter history and output phase across
//! calls, and the tests assert continuity across a chunk boundary specifically.
//!
//! The filter is a windowed-sinc polyphase FIR rather than linear interpolation. Linear
//! interpolation is cheap and fine for a ratio near 1, but 24 kHz → 16 kHz is a
//! *downsample*: without a low-pass first, everything above 8 kHz folds back into the
//! speech band as aliasing, and the thing that suffers is exactly the consonant energy
//! STT relies on.

use std::collections::VecDeque;
use std::f32::consts::PI;

use crate::telephony::MediaCodec;

// ---------------------------------------------------------------------------
// Companding
// ---------------------------------------------------------------------------

/// ITU-T G.711 µ-law → linear PCM16.
pub fn ulaw_to_pcm16(byte: u8) -> i16 {
    // The encoded byte is stored complemented; undo that first.
    let u = !byte;
    let sign = u & 0x80;
    let exponent = (u >> 4) & 0x07;
    let mantissa = u & 0x0F;
    let mut sample = ((mantissa as i32) << 3) + 0x84;
    sample <<= exponent;
    sample -= 0x84;
    let sample = sample.clamp(0, i16::MAX as i32) as i16;
    if sign != 0 {
        -sample
    } else {
        sample
    }
}

/// Linear PCM16 → ITU-T G.711 µ-law.
pub fn pcm16_to_ulaw(sample: i16) -> u8 {
    const BIAS: i32 = 0x84;
    const CLIP: i32 = 32635;
    let mut s = sample as i32;
    let sign = if s < 0 {
        s = -s;
        0x80u8
    } else {
        0
    };
    let s = s.min(CLIP) + BIAS;
    // Exponent is the position of the highest set bit above the bias.
    let exponent = (7i32 - ((s >> 7) as u32).leading_zeros() as i32 + 24).clamp(0, 7);
    let mantissa = ((s >> (exponent + 3)) & 0x0F) as u8;
    !(sign | ((exponent as u8) << 4) | mantissa)
}

/// ITU-T G.711 A-law → linear PCM16.
///
/// Note the sign convention: in A-law a **set** sign bit means POSITIVE, the opposite of
/// µ-law. Copying the µ-law branch here inverts every sample, which does not sound like
/// silence or noise — it sounds like slightly wrong speech, and it is the kind of thing
/// that gets blamed on the carrier.
pub fn alaw_to_pcm16(byte: u8) -> i16 {
    let a = byte ^ 0x55;
    let sign = a & 0x80;
    let exponent = ((a & 0x70) >> 4) as u32;
    let mantissa = (a & 0x0F) as i32;
    let sample = if exponent == 0 {
        (mantissa << 4) | 8
    } else {
        ((mantissa << 4) | 0x108) << (exponent - 1)
    };
    let sample = sample.clamp(0, i16::MAX as i32) as i16;
    if sign != 0 {
        sample
    } else {
        -sample
    }
}

/// Decode one wire payload into linear PCM16 samples.
///
/// L16 is **network byte order** (big-endian) — getting that backwards produces loud
/// noise rather than silence, which is at least an obvious failure.
pub fn decode(codec: MediaCodec, payload: &[u8]) -> Result<Vec<i16>, CodecError> {
    Ok(match codec {
        MediaCodec::L16 => {
            if !payload.len().is_multiple_of(2) {
                return Err(CodecError::TruncatedFrame);
            }
            payload
                .chunks_exact(2)
                .map(|p| i16::from_be_bytes([p[0], p[1]]))
                .collect()
        }
        MediaCodec::Pcmu => payload.iter().copied().map(ulaw_to_pcm16).collect(),
        MediaCodec::Pcma => payload.iter().copied().map(alaw_to_pcm16).collect(),
        // G.722 and Opus need a real decoder. Refused explicitly rather than decoded as
        // if they were something else: silently mis-decoding audio is far worse than
        // refusing the codec at negotiation time, where it can fall back to PCMU.
        MediaCodec::G722 | MediaCodec::Opus => return Err(CodecError::Unsupported { codec }),
    })
}

/// Encode linear PCM16 samples for the wire.
pub fn encode(codec: MediaCodec, samples: &[i16]) -> Result<Vec<u8>, CodecError> {
    Ok(match codec {
        MediaCodec::L16 => samples.iter().flat_map(|s| s.to_be_bytes()).collect(),
        MediaCodec::Pcmu => samples.iter().copied().map(pcm16_to_ulaw).collect(),
        MediaCodec::Pcma | MediaCodec::G722 | MediaCodec::Opus => {
            return Err(CodecError::Unsupported { codec })
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecError {
    /// An L16 payload with an odd byte count — half a sample.
    TruncatedFrame,
    /// We do not have a decoder/encoder for this codec. Negotiate a different one.
    Unsupported { codec: MediaCodec },
}

// ---------------------------------------------------------------------------
// Resampling
// ---------------------------------------------------------------------------

/// Taps per polyphase branch.
///
/// A Blackman window buys about -74 dB of stopband and pays for it with a wide transition
/// band, roughly `5.5/N` of the upsampled rate. At 24 kHz → 16 kHz the prototype runs at
/// 48 kHz with `N = TAPS × L = 64`, so the transition is ~4 kHz: cutoff at 8 kHz, real
/// rejection from about 10 kHz. That is what actually sets this number — at 24 taps the
/// transition was still open at 11 kHz, which is exactly where the aliasing that lands in
/// the speech band comes from.
///
/// Cost is 32 multiply-adds per output sample: about half a million per second per
/// direction, which is nothing next to the network.
const TAPS: usize = 32;

/// A stateful rational resampler.
///
/// **Create one per direction per call and keep it.** Its whole reason to exist is the
/// state: the filter history and output phase carried across chunks. A fresh resampler
/// per 20 ms chunk produces a discontinuity at every boundary.
pub struct Resampler {
    from_hz: u32,
    to_hz: u32,
    /// Interpolation and decimation factors, reduced.
    l: usize,
    m: usize,
    /// `phases[p][r]` = prototype tap `p + r*L`.
    phases: Vec<Vec<f32>>,
    /// The last `TAPS - 1` input samples, so the next chunk's first outputs see them.
    history: VecDeque<i16>,
    /// Absolute index of the next input sample to arrive.
    consumed: u64,
    /// Output counter, carried so the phase sequence is continuous.
    out_k: u64,
}

impl Resampler {
    pub fn new(from_hz: u32, to_hz: u32) -> Self {
        let g = gcd(from_hz as usize, to_hz as usize).max(1);
        let l = (to_hz as usize) / g;
        let m = (from_hz as usize) / g;
        let phases = build_phases(l, m);
        Self {
            from_hz,
            to_hz,
            l,
            m,
            phases,
            history: VecDeque::from(vec![0i16; TAPS - 1]),
            consumed: 0,
            out_k: 0,
        }
    }

    pub fn from_hz(&self) -> u32 {
        self.from_hz
    }

    pub fn to_hz(&self) -> u32 {
        self.to_hz
    }

    /// Whether this resampler does anything. A 1:1 rate is a legitimate configuration and
    /// [`process`](Self::process) short-circuits it rather than running a filter that is
    /// mathematically the identity.
    pub fn is_identity(&self) -> bool {
        self.from_hz == self.to_hz
    }

    /// Resample one chunk, continuing from the previous call.
    pub fn process(&mut self, input: &[i16]) -> Vec<i16> {
        if self.is_identity() {
            return input.to_vec();
        }
        if input.is_empty() {
            return Vec::new();
        }

        // x[0] is at absolute index `first_abs`; everything before it is zero (start of
        // stream) or already flushed.
        //
        // Signed on purpose: at the very first chunk `consumed` is 0 while the history is
        // pre-filled with TAPS-1 zeros, so the window legitimately starts *before* index
        // zero. Doing this arithmetic in `u64` underflows on the first packet of every
        // call, which is about as bad a time as there is.
        let hist_len = self.history.len() as i64;
        let first_abs = self.consumed as i64 - hist_len;
        let x: Vec<i16> = self
            .history
            .iter()
            .copied()
            .chain(input.iter().copied())
            .collect();
        let last_abs = self.consumed + input.len() as u64 - 1;

        let mut out = Vec::with_capacity(input.len() * self.l / self.m + 2);
        let (l, m) = (self.l as u64, self.m as u64);

        loop {
            let k = self.out_k;
            let phase = ((k * m) % l) as usize;
            let base_abs = (k * m - phase as u64) / l;
            if base_abs > last_abs {
                break;
            }
            let taps = &self.phases[phase];
            let mut acc = 0.0f32;
            for (r, tap) in taps.iter().enumerate() {
                let abs = base_abs as i64 - r as i64;
                let s = if abs < first_abs {
                    // Before the stream started (or already flushed): zero. Only ever
                    // reached in the first few samples of a call.
                    0.0
                } else {
                    x[(abs - first_abs) as usize] as f32
                };
                acc += tap * s;
            }
            out.push(acc.clamp(i16::MIN as f32, i16::MAX as f32) as i16);
            self.out_k += 1;
        }

        self.consumed += input.len() as u64;
        // Keep exactly the history the next call needs.
        for s in input {
            self.history.push_back(*s);
        }
        while self.history.len() > TAPS - 1 {
            self.history.pop_front();
        }
        out
    }
}

fn gcd(mut a: usize, mut b: usize) -> usize {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}

/// Blackman-windowed sinc low-pass, split into `l` polyphase branches.
///
/// Cutoff is `0.5 / max(l, m)` of the upsampled rate — i.e. the Nyquist of whichever of
/// the two rates is lower. That single choice is what makes the same filter correct for
/// both up- and down-sampling.
fn build_phases(l: usize, m: usize) -> Vec<Vec<f32>> {
    let n = TAPS * l;
    let cutoff = 0.5f32 / l.max(m) as f32;
    let center = (n as f32 - 1.0) / 2.0;
    let mut h = vec![0.0f32; n];
    for (i, slot) in h.iter_mut().enumerate() {
        let t = i as f32 - center;
        let sinc = if t.abs() < 1e-6 {
            2.0 * cutoff
        } else {
            (2.0 * PI * cutoff * t).sin() / (PI * t)
        };
        // Blackman window: ~-58 dB sidelobes, which is what keeps aliasing out of the
        // speech band.
        let w = 0.42 - 0.5 * (2.0 * PI * i as f32 / (n as f32 - 1.0)).cos()
            + 0.08 * (4.0 * PI * i as f32 / (n as f32 - 1.0)).cos();
        *slot = sinc * w * l as f32;
    }

    let mut phases = vec![vec![0.0f32; TAPS]; l];
    for (p, branch) in phases.iter_mut().enumerate() {
        for (r, tap) in branch.iter_mut().enumerate() {
            let idx = p + r * l;
            if idx < n {
                *tap = h[idx];
            }
        }
    }
    phases
}

// ---------------------------------------------------------------------------
// Barge-in
// ---------------------------------------------------------------------------

/// Queued synthesized audio for one direction, with a generation counter.
///
/// Barge-in (R15) is not just "stop playing". Translated speech is produced
/// asynchronously: when the far party interrupts, TTS chunks for the *previous* utterance
/// are still in flight and will arrive after the clear. Dropping the queue without a
/// generation counter lets those late chunks refill it, and the caller hears the sentence
/// they just interrupted resume a beat later — which is worse than not supporting
/// barge-in at all, because it sounds like the system ignored them.
///
/// So every chunk is tagged with the generation it was produced for, and a chunk from a
/// superseded generation is discarded on arrival.
#[derive(Debug, Default)]
pub struct PlaybackQueue {
    chunks: VecDeque<Vec<u8>>,
    generation: u64,
    queued_bytes: usize,
}

impl PlaybackQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// The generation a producer should stamp on the chunks it is about to make.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Enqueue a chunk. Returns `false` — and drops it — when it belongs to a superseded
    /// utterance.
    pub fn push(&mut self, generation: u64, chunk: Vec<u8>) -> bool {
        if generation != self.generation {
            return false;
        }
        self.queued_bytes += chunk.len();
        self.chunks.push_back(chunk);
        true
    }

    pub fn pop(&mut self) -> Option<Vec<u8>> {
        let c = self.chunks.pop_front()?;
        self.queued_bytes -= c.len();
        Some(c)
    }

    /// Abandon everything queued and everything still in flight.
    ///
    /// Returns whether anything was actually discarded, so the caller only sends the
    /// provider a `clear` when there was something to clear — an unnecessary clear on a
    /// silent leg is a wasted round trip on the latency path.
    pub fn barge_in(&mut self) -> bool {
        let had = !self.chunks.is_empty();
        self.chunks.clear();
        self.queued_bytes = 0;
        self.generation += 1;
        had
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    pub fn queued_bytes(&self) -> usize {
        self.queued_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Peak absolute error between two signals, as a fraction of full scale.
    fn peak_err(a: &[i16], b: &[i16]) -> f32 {
        a.iter()
            .zip(b)
            .map(|(x, y)| (*x as f32 - *y as f32).abs())
            .fold(0.0f32, f32::max)
            / 32768.0
    }

    fn sine(hz: f32, rate: u32, n: usize, amp: f32) -> Vec<i16> {
        (0..n)
            .map(|i| ((2.0 * PI * hz * i as f32 / rate as f32).sin() * amp * 32767.0) as i16)
            .collect()
    }

    // ---- companding -----------------------------------------------------------

    #[test]
    fn ulaw_round_trips_within_its_own_quantisation() {
        // µ-law is lossy by design; what must hold is that encode∘decode is stable and
        // that the error is the codec's, not ours.
        for byte in 0u8..=255 {
            let pcm = ulaw_to_pcm16(byte);
            let back = pcm16_to_ulaw(pcm);
            assert_eq!(
                ulaw_to_pcm16(back),
                pcm,
                "byte {byte} did not round-trip stably"
            );
        }
    }

    #[test]
    fn ulaw_preserves_sign_and_silence() {
        // 0xFF is µ-law silence. Getting the complement wrong makes it loud, which is at
        // least obvious — but assert it so it stays obvious.
        assert_eq!(ulaw_to_pcm16(0xFF), 0);
        assert!(ulaw_to_pcm16(0x00) < -30_000, "0x00 is large negative");
        assert!(ulaw_to_pcm16(0x80) > 30_000, "0x80 is large positive");
        assert_eq!(pcm16_to_ulaw(0), 0xFF);
    }

    #[test]
    fn alaw_preserves_sign_and_silence() {
        assert_eq!(alaw_to_pcm16(0xD5), 8);
        assert!(alaw_to_pcm16(0x2A) < -4000);
        assert!(alaw_to_pcm16(0xAA) > 4000);
    }

    #[test]
    fn l16_is_big_endian_on_the_wire() {
        // Network byte order. Backwards here produces loud noise, not silence.
        assert_eq!(
            decode(MediaCodec::L16, &[0x12, 0x34]).unwrap(),
            vec![0x1234]
        );
        assert_eq!(
            encode(MediaCodec::L16, &[0x1234, -1]).unwrap(),
            vec![0x12, 0x34, 0xFF, 0xFF]
        );
    }

    #[test]
    fn an_odd_length_l16_frame_is_refused_not_truncated() {
        // Half a sample means the stream is misaligned; guessing would shift every
        // subsequent sample by a byte and turn speech into noise.
        assert_eq!(
            decode(MediaCodec::L16, &[0x12, 0x34, 0x56]).unwrap_err(),
            CodecError::TruncatedFrame
        );
    }

    #[test]
    fn codecs_we_cannot_decode_are_refused_rather_than_misread() {
        for c in [MediaCodec::G722, MediaCodec::Opus] {
            assert_eq!(
                decode(c, &[0, 1, 2, 3]).unwrap_err(),
                CodecError::Unsupported { codec: c }
            );
        }
        // Encoding: A-law out is not implemented; refuse rather than send µ-law bytes
        // labelled as A-law, which would sound like distortion at the far end.
        assert!(matches!(
            encode(MediaCodec::Pcma, &[0]).unwrap_err(),
            CodecError::Unsupported { .. }
        ));
    }

    #[test]
    fn a_full_ulaw_frame_decodes_to_one_sample_per_byte() {
        let frame = vec![0xFFu8; 160]; // 20 ms at 8 kHz
        assert_eq!(decode(MediaCodec::Pcmu, &frame).unwrap().len(), 160);
    }

    // ---- resampling -----------------------------------------------------------

    #[test]
    fn the_rate_ratio_is_reduced_and_the_output_length_follows_it() {
        let mut r = Resampler::new(16_000, 24_000);
        assert_eq!((r.l, r.m), (3, 2), "16k->24k is 3/2");
        let out = r.process(&vec![0i16; 320]); // 20 ms at 16 kHz
        assert!(
            (out.len() as i64 - 480).abs() <= 2,
            "expected ~480 samples, got {}",
            out.len()
        );

        let mut down = Resampler::new(24_000, 16_000);
        assert_eq!((down.l, down.m), (2, 3));
        let out = down.process(&vec![0i16; 480]);
        assert!((out.len() as i64 - 320).abs() <= 2, "got {}", out.len());
    }

    #[test]
    fn an_identity_rate_is_a_passthrough_not_a_filter() {
        let mut r = Resampler::new(24_000, 24_000);
        assert!(r.is_identity());
        let input = sine(440.0, 24_000, 240, 0.5);
        assert_eq!(
            r.process(&input),
            input,
            "identity must not colour the audio"
        );
    }

    #[test]
    fn a_tone_survives_the_round_trip_through_the_phone_rate() {
        // 24k -> 16k -> 24k on a 440 Hz tone. Group delay shifts it, so compare the
        // steady-state middle rather than the edges.
        let input = sine(440.0, 24_000, 4800, 0.5);
        let mut down = Resampler::new(24_000, 16_000);
        let mut up = Resampler::new(16_000, 24_000);
        let mid = down.process(&input);
        let out = up.process(&mid);

        let a = &input[1200..3600];
        // Find the best alignment within a window covering the filters' group delay.
        let best = (0..80)
            .filter(|d| 1200 + d + 2400 <= out.len())
            .map(|d| peak_err(a, &out[1200 + d..1200 + d + 2400]))
            .fold(f32::INFINITY, f32::min);
        assert!(
            best < 0.06,
            "a 440 Hz tone should survive the round trip; peak error {best}"
        );
    }

    #[test]
    fn chunking_the_input_gives_the_same_stream_as_one_big_call() {
        // THE regression this module exists for. Resampling each 20 ms chunk with a fresh
        // resampler puts a discontinuity at every boundary — a 50 Hz click train that is
        // quiet enough to survive a listen and loud enough to wreck STT.
        let input = sine(600.0, 16_000, 3200, 0.6);

        let mut whole = Resampler::new(16_000, 24_000);
        let expected = whole.process(&input);

        let mut chunked = Resampler::new(16_000, 24_000);
        let mut got = Vec::new();
        for chunk in input.chunks(320) {
            got.extend(chunked.process(chunk));
        }

        assert_eq!(got.len(), expected.len(), "sample count must not drift");
        assert!(
            peak_err(&expected, &got) < 1e-4,
            "chunk boundaries introduced a discontinuity: peak error {}",
            peak_err(&expected, &got)
        );
    }

    #[test]
    fn uneven_chunk_sizes_are_also_continuous() {
        // Real media sockets do not deliver tidy 20 ms frames forever.
        let input = sine(900.0, 16_000, 2000, 0.5);
        let mut whole = Resampler::new(16_000, 24_000);
        let expected = whole.process(&input);

        let mut r = Resampler::new(16_000, 24_000);
        let mut got = Vec::new();
        let mut i = 0;
        for len in [7usize, 320, 1, 512, 160, 33].iter().cycle() {
            if i >= input.len() {
                break;
            }
            let end = (i + len).min(input.len());
            got.extend(r.process(&input[i..end]));
            i = end;
        }
        assert_eq!(got.len(), expected.len());
        assert!(peak_err(&expected, &got) < 1e-4);
    }

    #[test]
    fn an_empty_chunk_is_harmless() {
        let mut r = Resampler::new(16_000, 24_000);
        assert!(r.process(&[]).is_empty());
        // …and does not disturb the stream that follows.
        let a = r.process(&sine(440.0, 16_000, 320, 0.5));
        assert!(!a.is_empty());
    }

    #[test]
    fn downsampling_rejects_content_above_the_new_nyquist() {
        // The reason this is a filter and not linear interpolation. An 11 kHz tone
        // downsampled 24k -> 16k without a low-pass folds to 5 kHz — inside the speech
        // band, where STT will faithfully transcribe the wrong thing.
        //
        // 11 kHz and not 7: the new Nyquist is 8 kHz, so 7 kHz is content we are supposed
        // to KEEP. Asserting rejection below the cutoff tests the opposite of the
        // property, and passes for the wrong reason if the filter is too aggressive.
        let input = sine(11_000.0, 24_000, 4800, 0.8);
        let mut down = Resampler::new(24_000, 16_000);
        let out = down.process(&input);

        let steady = &out[400..];
        let rms = (steady.iter().map(|s| (*s as f32).powi(2)).sum::<f32>() / steady.len() as f32)
            .sqrt()
            / 32768.0;
        assert!(
            rms < 0.05,
            "content above the new Nyquist should be attenuated, not folded back; rms {rms}"
        );
    }

    #[test]
    fn a_speech_band_tone_passes_the_downsampler_intact() {
        // The other half of the previous test: the filter must not eat what it should keep.
        let input = sine(1_000.0, 24_000, 4800, 0.5);
        let mut down = Resampler::new(24_000, 16_000);
        let out = down.process(&input);
        let steady = &out[400..];
        let rms = (steady.iter().map(|s| (*s as f32).powi(2)).sum::<f32>() / steady.len() as f32)
            .sqrt()
            / 32768.0;
        // A 0.5-amplitude sine has rms 0.354.
        assert!(
            rms > 0.30,
            "1 kHz should pass essentially untouched; rms {rms}"
        );
    }

    #[test]
    fn a_loud_signal_clips_rather_than_wrapping() {
        // Filter overshoot on a full-scale square wave can exceed i16. Wrapping turns a
        // loud sound into a loud CRACK; clamping just makes it loud.
        let input: Vec<i16> = (0..960)
            .map(|i| if (i / 8) % 2 == 0 { 32767 } else { -32768 })
            .collect();
        let mut r = Resampler::new(16_000, 24_000);
        let out = r.process(&input);
        // A wrap shows up as a near-full-scale jump between adjacent output samples. The
        // real signal, upsampled, cannot move that far in one 24 kHz sample.
        let biggest_jump = out
            .windows(2)
            .map(|w| (w[1] as i32 - w[0] as i32).abs())
            .max()
            .unwrap_or(0);
        assert!(
            biggest_jump < 45_000,
            "a {biggest_jump} jump between adjacent samples means the filter overshoot \
             wrapped instead of clamping — a loud sound became a CRACK"
        );
        assert!(out.iter().any(|s| *s > 20_000));
        assert!(out.iter().any(|s| *s < -20_000));
    }

    #[test]
    fn the_eight_kilohertz_phone_path_also_resamples_cleanly() {
        let mut up = Resampler::new(8_000, 24_000);
        assert_eq!((up.l, up.m), (3, 1));
        let out = up.process(&sine(440.0, 8_000, 160, 0.5));
        assert!((out.len() as i64 - 480).abs() <= 2, "got {}", out.len());
    }

    // ---- barge-in -------------------------------------------------------------

    #[test]
    fn barge_in_drops_the_queue_and_reports_that_it_did() {
        let mut q = PlaybackQueue::new();
        let g = q.generation();
        assert!(q.push(g, vec![1, 2, 3]));
        assert!(q.push(g, vec![4]));
        assert_eq!(q.queued_bytes(), 4);

        assert!(q.barge_in(), "there was audio to discard");
        assert!(q.is_empty());
        assert_eq!(q.queued_bytes(), 0);
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn barge_in_on_a_silent_leg_reports_nothing_to_clear() {
        // So the caller can skip the provider round trip. On the latency path, a
        // needless command is not free.
        let mut q = PlaybackQueue::new();
        assert!(!q.barge_in());
    }

    #[test]
    fn audio_still_in_flight_when_the_interruption_happened_is_discarded() {
        // THE bug this exists for. TTS runs asynchronously: chunks for the interrupted
        // sentence arrive AFTER the clear. Without the generation counter they refill the
        // queue and the caller hears the sentence they just interrupted resume — which
        // reads as "the system ignored me", worse than having no barge-in at all.
        let mut q = PlaybackQueue::new();
        let old = q.generation();
        q.push(old, vec![1]);

        q.barge_in();
        let new = q.generation();
        assert_ne!(old, new);

        assert!(!q.push(old, vec![2]), "a late chunk from the old utterance");
        assert!(q.is_empty(), "and it did not refill the queue");

        assert!(q.push(new, vec![3]), "the new utterance plays normally");
        assert_eq!(q.pop(), Some(vec![3]));
    }

    #[test]
    fn the_queue_is_fifo_so_speech_does_not_come_out_backwards() {
        let mut q = PlaybackQueue::new();
        let g = q.generation();
        for i in 0..5u8 {
            q.push(g, vec![i]);
        }
        for i in 0..5u8 {
            assert_eq!(q.pop(), Some(vec![i]));
        }
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn repeated_barge_ins_keep_advancing_the_generation() {
        // Two interruptions in quick succession must not let the FIRST utterance's
        // in-flight audio through on the second clear.
        let mut q = PlaybackQueue::new();
        let g0 = q.generation();
        q.push(g0, vec![1]);
        q.barge_in();
        let g1 = q.generation();
        q.push(g1, vec![2]);
        q.barge_in();
        let g2 = q.generation();
        assert!(g2 > g1 && g1 > g0);
        assert!(!q.push(g0, vec![9]));
        assert!(!q.push(g1, vec![9]));
        assert!(q.push(g2, vec![9]));
    }
}
