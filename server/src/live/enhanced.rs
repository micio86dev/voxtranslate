//! Enhanced (`cartesia`) in Live Connect (spec 0121, S5): speaker-side speech-to-text, translation
//! on the server, listener-side text-to-speech.
//!
//! # Why not Meet's shape
//!
//! Meet's Enhanced runs its speech-to-text on the LISTENER's browser, on the remote WebRTC audio.
//! Live Connect has no WebRTC between the phones, so that cannot work here. Instead:
//!
//! 1. every phone runs Cartesia STT on **its own microphone** and sends each segment as a
//!    `translate_text` frame on `/ws/live`;
//! 2. the server translates the text (Groq, the same uncached path Meet's `translate_text` hop
//!    uses, behind the same process-wide admission semaphore) once per DISTINCT target language
//!    among the other seats, and sends every listener the standard caption frames;
//! 3. for a final segment it also tells each listener's phone what to speak: an `enhanced_speak`
//!    frame. The phone speaks it with Cartesia TTS, in the speaker's cloned voice when the host
//!    opted into it for this conversation, else Cartesia's default voice.
//!
//! No audio ever reaches the server in this mode. Binary frames are ignored.
//!
//! # Billing
//!
//! The owner pays every seat at the Enhanced rate, per distinct target language, through the same
//! meter as every other tier ([`super::billing::LiveMeter`]). With no audio frames the meter's
//! `activity` counter is the number of **accepted** `translate_text` frames since the last tick, so
//! a seat that sends nothing pays nothing after the opening charge, exactly like silence on the
//! audio path, and a frame that is dropped (rate cap, echo guard, muted) is never billed.
//!
//! # The text-level echo guard
//!
//! The partner's phone hears the speaker too, so ITS Cartesia STT transcribes the speaker's words
//! and sends them as ITS `translate_text`. The audio path decides that with microphone levels
//! ([`super::echo`]); there are none here. [`TextLog`] is therefore **order based**: a frame is
//! dropped when it is highly similar to something ANOTHER seat sent in the last
//! [`ECHO_WINDOW`] — the real speaker's own STT is normally ahead of a bystander's, so whoever
//! arrived first is taken to be the speaker.
//!
//! A phone also hears ITS OWN text-to-speech. What was last spoken to a language group is
//! remembered for [`SPOKEN_WINDOW`], and a seat of that language whose text is similar to it is an
//! echo of its own phone's TTS, the loop that would otherwise translate and bill a translation of
//! a translation. (The client also pauses STT while it plays TTS; this is the backstop.)
//!
//! A guard like this is also a way to silence somebody: whoever sends the words first "owns" them.
//! So it is deliberately narrow. It only judges text of at least [`TEXT_ECHO_MIN_TOKENS`] words
//! and only drops it at a similarity of at least [`TEXT_ECHO_MIN_SIM`] (a short acknowledgement
//! such as "ok perfecto" is never dropped), its windows are a few seconds, and it never drops a
//! seat for text another seat sent while that other seat was itself being played this seat's
//! words, or after this seat had already said the same thing first. What stays open: an adversary
//! who sends the exact words of a long, predictable phrase a moment before the host says it still
//! wins that one phrase, and a genuine verbatim repeat of a long sentence inside [`SPOKEN_WINDOW`]
//! of its translation is taken for an echo. Both cost one segment, never the conversation.
//!
//! # Cost ceilings
//!
//! Every translated segment is one Groq call per distinct target language, behind a process-wide
//! admission semaphore shared with Standard and Meet. One phone must not be able to spend it: an
//! interim caption is accepted at most [`PARTIALS_PER_SEC`] a second per seat and is cut to its
//! last [`MAX_PARTIAL_CHARS`] characters; a seat has at most [`SEAT_INFLIGHT`] calls in flight at
//! once (interims past that are dropped, finals wait their turn); and the conversation as a whole
//! has a per-minute ceiling of calls ([`TranslationBudget`], `LIVE_ENHANCED_CALLS_PER_MIN`).
//!
//! Nothing here logs or stores text outside the transcript recorder, which already holds it for
//! conversations whose owner opted into saving. Diagnostics carry counts only.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, Semaphore};
use tokio::time::Instant;
use uuid::Uuid;

use super::echo;
use super::routes::{api_err, authorize, owner_can_afford, rate_limited, LiveAuth};
use super::state::SessionLatency;
use super::tiers;
use crate::protocol::ServerMessage;
use crate::rooms::{PeerTx, RoomManager};
use crate::transcripts::{EventKind, TranscriptEvent, TranscriptService};
use crate::translator::Translator;
use crate::AppState;

/// Longest `translate_text` text a seat may send, in characters. Cartesia's own segments are a
/// sentence or two; this bounds what one frame can spend at the paid translation hop.
pub const MAX_TEXT_CHARS: usize = 1000;
/// Frames per second one seat may send (a burst of the same size is allowed). Cartesia STT emits a
/// handful of partials a second; past this a frame is dropped, never queued, and not billed.
pub const FRAMES_PER_SEC: f64 = 8.0;
/// How long text another seat sent counts as "said first" for the echo guard.
pub const ECHO_WINDOW: Duration = Duration::from_millis(2500);
/// How long text spoken to a language group counts as "my own phone's TTS" for the echo guard.
/// Longer than [`ECHO_WINDOW`]: TTS plays for a moment after it is sent. Kept short on purpose: a
/// real reply a few seconds later that happens to repeat the words must get through.
pub const SPOKEN_WINDOW: Duration = Duration::from_secs(4);
/// Fewest words the text-level echo guard will judge. One or two words ("ok perfecto", "vale
/// gracias") are what people genuinely answer, and are what an adversary can most cheaply guess.
pub const TEXT_ECHO_MIN_TOKENS: usize = 3;
/// Least similarity at which the text-level guard drops anything, whatever `LIVE_ECHO_SIM` says
/// (that knob tunes the audio filter, and can only make this one stricter).
pub const TEXT_ECHO_MIN_SIM: f64 = 0.85;
/// Interim captions one seat may send per second (a burst of the same size is allowed). Finals
/// keep [`FRAMES_PER_SEC`].
pub const PARTIALS_PER_SEC: f64 = 2.0;
/// Longest interim caption that is translated, in characters; a longer one is cut to its LAST
/// this many (the newest words are what a partial is for). A final may be up to
/// [`MAX_TEXT_CHARS`].
pub const MAX_PARTIAL_CHARS: usize = 400;
/// Translation calls one seat may have in flight at once.
pub const SEAT_INFLIGHT: usize = 2;
/// The share of the conversation's call ceiling interim captions may use, in percent; the rest is
/// kept for finals.
const PARTIAL_BUDGET_PERCENT: u32 = 60;
/// The window the call ceiling is counted in.
const BUDGET_WINDOW: Duration = Duration::from_secs(60);
/// Longest one translation may take before the segment is given up on.
pub const TRANSLATE_TIMEOUT: Duration = Duration::from_secs(6);
/// A seat is told `translation_unavailable` at most once in this interval.
pub const NOTICE_EVERY: Duration = Duration::from_secs(10);
/// Segments a seat may have waiting for translation. Interims are coalesced when the worker
/// catches up; a final is only lost if this overflows (and the speaker is told).
pub const QUEUE_CAP: usize = 32;
/// Entries kept per seat in the echo log.
const LOG_PER_SEAT: usize = 8;

/// Token endpoint limits (`POST /api/live/sessions/{id}/enhanced/session`). A guest has no account
/// to throttle, and every token is a real Cartesia credential good for an hour, so the cost
/// ceiling follows the seat, the conversation and the address. Phones refresh at most a few times
/// an hour; these leave room for reconnect storms and nothing more.
pub const TOKENS_PER_SEAT_PER_MIN: u32 = 6;
pub const TOKENS_PER_SESSION_PER_MIN: u32 = 20;
pub const TOKENS_PER_IP_PER_MIN: u32 = 30;

// ---- frames ----------------------------------------------------------------------------

/// The Enhanced frames a phone may send on `/ws/live`. Parsed before the meeting protocol's
/// `ClientMessage`, whose `translate_text` and `enhanced_fallback` are Meet's (different fields).
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum EnhancedFrame {
    #[serde(rename = "translate_text")]
    TranslateText {
        /// The phone's own id for the segment, echoed on `enhanced_speak`: a string of up to 64
        /// characters or a whole number.
        segment_id: Value,
        text: String,
        /// `true` once the segment is complete. Anything else is a partial.
        #[serde(rename = "final", default)]
        is_final: bool,
        // `lang` is accepted and ignored: the seat's declared language is authoritative.
        #[serde(default)]
        #[allow(dead_code)]
        lang: Option<String>,
    },
    #[serde(rename = "enhanced_fallback")]
    Fallback {
        #[serde(default)]
        #[allow(dead_code)]
        reason: Option<String>,
    },
}

impl EnhancedFrame {
    /// `Some` when `text` is one of the two Enhanced frames. A cheap substring check first, so the
    /// audio-path frames (`start`, `stop`, mute) never pay for a second parse.
    pub fn parse(text: &str) -> Option<Self> {
        if !text.contains("translate_text") && !text.contains("enhanced_fallback") {
            return None;
        }
        serde_json::from_str(text).ok()
    }
}

/// A `segment_id` a phone may use: a string of 1 to 64 characters, or a whole number.
pub fn valid_segment_id(v: &Value) -> bool {
    match v {
        Value::String(s) => (1..=64).contains(&s.chars().count()),
        Value::Number(n) => n.is_u64() || n.is_i64(),
        _ => false,
    }
}

/// One segment waiting to be translated.
#[derive(Debug)]
pub struct Segment {
    pub segment_id: Value,
    pub text: String,
    pub is_final: bool,
}

/// The frame that tells a phone what to speak: `{"type":"enhanced_speak", ...}`. `voice_id` is the
/// speaker's cloned voice only for a conversation whose host opted in and only for the host's own
/// words; `null` otherwise (the phone uses Cartesia's default voice).
pub fn speak_frame(
    speaker_id: &str,
    segment_id: &Value,
    lang: &str,
    text: &str,
    voice_id: Option<&str>,
) -> String {
    json!({
        "type": "enhanced_speak",
        "speaker_id": speaker_id,
        "segment_id": segment_id,
        "lang": lang,
        "text": text,
        "voice_id": voice_id,
    })
    .to_string()
}

/// A token bucket in frames: `rate` a second with one second of burst.
#[derive(Debug)]
pub struct FrameGate {
    rate: f64,
    tokens: f64,
    last: Instant,
}

impl FrameGate {
    /// The gate for every frame: [`FRAMES_PER_SEC`].
    pub fn new(now: Instant) -> Self {
        Self::with_rate(FRAMES_PER_SEC, now)
    }

    pub fn with_rate(rate: f64, now: Instant) -> Self {
        Self {
            rate,
            tokens: rate,
            last: now,
        }
    }

    pub fn admit(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.rate).min(self.rate);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// The text an interim caption is translated from: at most [`MAX_PARTIAL_CHARS`] characters, the
/// LAST ones when it is longer.
pub fn clip_partial(text: &str) -> &str {
    let n = text.chars().count();
    if n <= MAX_PARTIAL_CHARS {
        return text;
    }
    let skip = n - MAX_PARTIAL_CHARS;
    let at = text.char_indices().nth(skip).map_or(0, |(i, _)| i);
    &text[at..]
}

/// How many translation calls a conversation may make per minute, all its seats together.
///
/// A fixed one-minute window (so a burst straddling two windows can reach twice the ceiling for a
/// moment; that is fine for a cost cap). Interim captions may use only [`PARTIAL_BUDGET_PERCENT`]
/// of it, so however many of them there are, finals always have room.
#[derive(Debug)]
pub struct TranslationBudget {
    cap: u32,
    partial_cap: u32,
    window_start: Option<Instant>,
    used: u32,
    partial_used: u32,
}

impl TranslationBudget {
    pub fn new(calls_per_min: u32) -> Self {
        Self {
            cap: calls_per_min,
            partial_cap: (calls_per_min * PARTIAL_BUDGET_PERCENT / 100).max(1),
            window_start: None,
            used: 0,
            partial_used: 0,
        }
    }

    /// Count one call and say whether it may be made.
    pub fn allow(&mut self, now: Instant, is_final: bool) -> bool {
        if self
            .window_start
            .is_none_or(|t| now.saturating_duration_since(t) >= BUDGET_WINDOW)
        {
            self.window_start = Some(now);
            self.used = 0;
            self.partial_used = 0;
        }
        if self.used >= self.cap || (!is_final && self.partial_used >= self.partial_cap) {
            return false;
        }
        self.used += 1;
        if !is_final {
            self.partial_used += 1;
        }
        true
    }
}

/// "Say it at most once in [`NOTICE_EVERY`]".
#[derive(Debug, Default)]
pub struct Notifier {
    last: Option<Instant>,
}

impl Notifier {
    pub fn due(&mut self, now: Instant) -> bool {
        if self
            .last
            .is_some_and(|t| now.saturating_duration_since(t) < NOTICE_EVERY)
        {
            return false;
        }
        self.last = Some(now);
        true
    }
}

/// The error frame a seat gets when its words could not be translated.
pub fn translation_unavailable() -> String {
    ServerMessage::Error {
        message: "translation unavailable".into(),
        code: Some("translation_unavailable".into()),
    }
    .to_json()
}

// ---- the text-level echo guard ---------------------------------------------------------

#[derive(Debug)]
enum Kind {
    /// Text a seat sent (accepted), in the language it declared.
    Sent { seat: usize, lang: String },
    /// Text `from` said, spoken to the phones of a language.
    Spoken { lang: String, from: usize },
}

#[derive(Debug)]
struct Entry {
    kind: Kind,
    at: Instant,
    norm: String,
}

/// Enough text for the guard to judge: [`TEXT_ECHO_MIN_TOKENS`] words, or one very long token
/// (scripts written without spaces give a whole sentence as one token).
fn judgeable(norm: &str) -> bool {
    let words = norm.split_whitespace().count();
    words >= TEXT_ECHO_MIN_TOKENS || (words == 1 && echo::enough(norm))
}

/// What the seats of one conversation recently sent and were told to speak. See the module
/// documentation for what this can and cannot decide.
#[derive(Debug, Default)]
pub struct TextLog {
    entries: Vec<Entry>,
}

impl TextLog {
    /// Whether the phones of `lang` are being played something `from` said (still inside
    /// [`SPOKEN_WINDOW`]): then what those phones send may be their own TTS, not their speech.
    fn hears_tts_of(&self, lang: &str, from: usize) -> bool {
        self.entries.iter().any(
            |e| matches!(&e.kind, Kind::Spoken { lang: l, from: f } if l == lang && *f == from),
        )
    }

    /// Judge a frame of `seat` (declared language `lang`): `true` is "echo, drop it". A frame that
    /// is not an echo is remembered, so what it said can make a later frame of another seat an
    /// echo. Text of fewer than [`TEXT_ECHO_MIN_TOKENS`] words is never judged and never
    /// remembered, and nothing is dropped below [`TEXT_ECHO_MIN_SIM`].
    pub fn judge(&mut self, seat: usize, lang: &str, now: Instant, text: &str, sim: f64) -> bool {
        let norm = echo::normalize(text);
        if !judgeable(&norm) {
            return false;
        }
        let sim = sim.max(TEXT_ECHO_MIN_SIM);
        self.entries
            .retain(|e| now.saturating_duration_since(e.at) <= SPOKEN_WINDOW);
        let is_echo = self.entries.iter().any(|e| {
            let age = now.saturating_duration_since(e.at);
            let relevant = match &e.kind {
                // Another seat said it first, recently, and that seat was not itself hearing this
                // seat's words spoken (then ITS text is the echo), and this seat had not already
                // said it before (then it was first).
                Kind::Sent { seat: s, lang: l } => {
                    *s != seat
                        && age <= ECHO_WINDOW
                        && !self.hears_tts_of(l, seat)
                        && !self.entries.iter().any(|own| {
                            matches!(own.kind, Kind::Sent { seat: o, .. } if o == seat)
                                && own.at < e.at
                                && echo::similarity(&own.norm, &e.norm) >= sim
                        })
                }
                Kind::Spoken { lang: l, .. } => l == lang,
            };
            relevant && echo::similarity(&norm, &e.norm) >= sim
        });
        if is_echo {
            return true;
        }
        // The same words again from the same seat (an interim repeated) add nothing.
        let dup = self
            .entries
            .iter()
            .rev()
            .find(|e| matches!(e.kind, Kind::Sent { seat: s, .. } if s == seat))
            .is_some_and(|e| e.norm == norm);
        if !dup {
            let mine = self
                .entries
                .iter()
                .filter(|e| matches!(e.kind, Kind::Sent { seat: s, .. } if s == seat))
                .count();
            if mine >= LOG_PER_SEAT {
                if let Some(i) = self
                    .entries
                    .iter()
                    .position(|e| matches!(e.kind, Kind::Sent { seat: s, .. } if s == seat))
                {
                    self.entries.remove(i);
                }
            }
            self.entries.push(Entry {
                kind: Kind::Sent {
                    seat,
                    lang: lang.into(),
                },
                at: now,
                norm,
            });
        }
        false
    }

    /// `text`, said by seat `from`, was just sent to the phones whose language is `lang` to be
    /// spoken.
    pub fn note_spoken(&mut self, from: usize, lang: &str, now: Instant, text: &str) {
        let norm = echo::normalize(text);
        if !judgeable(&norm) {
            return;
        }
        self.entries
            .retain(|e| now.saturating_duration_since(e.at) <= SPOKEN_WINDOW);
        // A bounded log: the newest few per language are all that can still be echoing.
        let mine = self
            .entries
            .iter()
            .filter(|e| matches!(&e.kind, Kind::Spoken { lang: l, .. } if l == lang))
            .count();
        if mine >= LOG_PER_SEAT {
            if let Some(i) = self
                .entries
                .iter()
                .position(|e| matches!(&e.kind, Kind::Spoken { lang: l, .. } if l == lang))
            {
                self.entries.remove(i);
            }
        }
        self.entries.push(Entry {
            kind: Kind::Spoken {
                lang: lang.into(),
                from,
            },
            at: now,
            norm,
        });
    }
}

// ---- translation and delivery ----------------------------------------------------------

/// What one translation call came to.
enum Got {
    Text(String),
    /// An interim caption that was not translated this time, on purpose.
    Skipped,
    Failed,
}

/// Everything one speaker's translation worker needs; built once per socket.
pub struct Pipeline {
    pub rooms: Arc<RoomManager>,
    pub translator: Translator,
    /// Present when the conversation keeps its transcript (the owner opted in, or a business one).
    pub transcripts: Option<TranscriptService>,
    pub latency: Arc<SessionLatency>,
    pub room: String,
    pub speaker_id: String,
    pub speaker_name: String,
    pub speaker_lang: String,
    pub speaker_user_id: Option<Uuid>,
    pub call_session: Uuid,
    /// The host's cloned voice, set only for the host's own socket of a conversation that opted
    /// into it (and only while cloning is enabled). Everyone else speaks with the default voice.
    pub voice_id: Option<String>,
    /// The speaker's own channel, for `translation_unavailable`.
    pub out: PeerTx,
}

impl Pipeline {
    /// Process segments in the order they were sent until the socket closes the channel. A partial
    /// that a later segment already supersedes is skipped (the newest wins), so a slow translation
    /// never builds a backlog of stale partials; finals are never skipped.
    pub async fn run(self, mut rx: mpsc::Receiver<Segment>) {
        let mut notice = Notifier::default();
        // This seat's own limit on translation calls in flight, in front of the process-wide
        // admission semaphore every tier shares.
        let inflight = Arc::new(Semaphore::new(SEAT_INFLIGHT));
        let mut round = 0usize;
        while let Some(mut seg) = rx.recv().await {
            while !seg.is_final {
                match rx.try_recv() {
                    Ok(next) => seg = next,
                    Err(_) => break,
                }
            }
            self.process(seg, &mut notice, &inflight, round).await;
            round = round.wrapping_add(1);
        }
    }

    async fn process(
        &self,
        seg: Segment,
        notice: &mut Notifier,
        inflight: &Semaphore,
        round: usize,
    ) {
        let original = seg.text.trim().to_string();
        if original.is_empty() {
            return;
        }
        let ts = Utc::now();
        // Everybody else in the room, by distinct language. The speaker never appears: the room
        // excludes its own id, and every frame below is addressed by language except the speaker.
        let langs = self.rooms.get_room_languages(&self.room, &self.speaker_id);
        let mut targets: Vec<&String> = langs.iter().filter(|l| **l != self.speaker_lang).collect();
        // An interim caption that cannot have all its calls (see `translate_one`) loses the same
        // language every time unless the order rotates.
        if !seg.is_final && targets.len() > 1 {
            let by = round % targets.len();
            targets.rotate_left(by);
        }

        // Listeners who share the speaker's language need no translation: the original is the
        // caption, and they hear the speaker themselves. Sent first, before any Groq round trip.
        if langs.contains(&self.speaker_lang) {
            self.caption(&self.speaker_lang, &original, None, seg.is_final);
        }

        let (this, text, is_final) = (self, original.as_str(), seg.is_final);
        let results = futures::future::join_all(targets.iter().map(|t| async move {
            (
                (*t).clone(),
                this.translate_one(inflight, text, t, is_final).await,
            )
        }))
        .await;

        let mut translations: HashMap<String, String> = HashMap::new();
        translations.insert(self.speaker_lang.clone(), original.clone());
        let mut failed = false;
        for (lang, r) in results {
            match r {
                Got::Text(t) => {
                    self.caption(&lang, &t, Some(&original), seg.is_final);
                    if seg.is_final {
                        // What the phones of this language are about to speak, remembered so a
                        // phone that hears its own TTS is not mistaken for a speaker.
                        self.note_spoken(&lang, &t);
                        self.rooms.broadcast_to_lang_except(
                            &self.room,
                            &lang,
                            &self.speaker_id,
                            &speak_frame(
                                &self.speaker_id,
                                &seg.segment_id,
                                &lang,
                                &t,
                                self.voice_id.as_deref(),
                            ),
                        );
                    }
                    translations.insert(lang, t);
                }
                // An interim caption nobody was allowed to make this time: not an error, the next
                // one replaces it.
                Got::Skipped => {}
                // A timeout, a provider error, an empty answer or the conversation's ceiling: that
                // language gets nothing for this segment (never the untranslated words, which
                // would be captioned and SPOKEN in the wrong language).
                Got::Failed => failed = true,
            }
        }
        if failed && notice.due(Instant::now()) {
            let _ = self.out.send(translation_unavailable());
        }

        // Same persistence as an engine final: one row per utterance, original plus whatever
        // translations succeeded, stamped when it was said.
        if seg.is_final {
            if let Some(svc) = self.transcripts.as_ref() {
                svc.record(TranscriptEvent {
                    session_id: self.call_session,
                    kind: EventKind::Speech,
                    speaker_peer_id: self.speaker_id.clone(),
                    speaker_user_id: self.speaker_user_id,
                    speaker_name: self.speaker_name.clone(),
                    original_text: original,
                    original_lang: self.speaker_lang.clone(),
                    translations,
                    ts,
                });
            }
        }
    }

    /// One translation call, inside the seat's and the conversation's limits. A final waits for
    /// its turn on the seat's limiter; an interim that cannot have one right now is dropped.
    async fn translate_one(
        &self,
        inflight: &Semaphore,
        text: &str,
        target: &str,
        is_final: bool,
    ) -> Got {
        let _permit = if is_final {
            match inflight.acquire().await {
                Ok(p) => p,
                Err(_) => return Got::Failed,
            }
        } else {
            match inflight.try_acquire() {
                Ok(p) => p,
                Err(_) => return Got::Skipped,
            }
        };
        if !self.latency.allow_translation(Instant::now(), is_final) {
            return if is_final { Got::Failed } else { Got::Skipped };
        }
        let r = tokio::time::timeout(
            TRANSLATE_TIMEOUT,
            self.translator
                .translate_uncached(text, &self.speaker_lang, target),
        )
        .await;
        match r {
            Ok(Ok(t)) if !t.trim().is_empty() => Got::Text(t.trim().to_string()),
            _ => Got::Failed,
        }
    }

    fn note_spoken(&self, lang: &str, text: &str) {
        let seat = self.latency.seat_of(&self.speaker_id).unwrap_or(0);
        self.latency.note_spoken(seat, lang, Instant::now(), text);
    }

    /// The caption frames the engine tiers send, for the listeners of `lang` (never the speaker).
    /// `translated_from` is the speaker's original when `text` is a translation of it.
    fn caption(&self, lang: &str, text: &str, translated_from: Option<&str>, is_final: bool) {
        let msg = if is_final {
            let mut translations = HashMap::new();
            translations.insert(lang.to_string(), text.to_string());
            ServerMessage::SubtitleFinal {
                speaker_id: self.speaker_id.clone(),
                speaker_name: self.speaker_name.clone(),
                original: translated_from.unwrap_or(text).to_string(),
                lang: self.speaker_lang.clone(),
                translations,
            }
        } else {
            ServerMessage::SubtitleInterim {
                speaker_id: self.speaker_id.clone(),
                speaker_name: self.speaker_name.clone(),
                text: text.to_string(),
                lang: self.speaker_lang.clone(),
                original: translated_from.map(str::to_string),
            }
        };
        self.rooms
            .broadcast_to_lang_except(&self.room, lang, &self.speaker_id, &msg.to_json());
    }
}

// ---- the token endpoint ----------------------------------------------------------------

/// `POST /api/live/sessions/{id}/enhanced/session` — a Cartesia access token for the phone that
/// asks, authorised by its Live Connect token (host or guest: a guest has no account).
///
/// Refused unless the token names THIS session, the seat is a current one of a session that is
/// still `waiting`/`active`, and the conversation runs on Enhanced right now (not Standard, not
/// after the fallback). Rate limited per seat, per conversation and per address, and the owner
/// must still be able to afford the tier. The JSON is exactly what Meet's endpoint returns
/// ([`crate::api::enhanced_session_body`]); no voice id travels here, a listener's phone learns
/// the speaker's voice only inside `enhanced_speak`. The token is never logged.
pub async fn token(
    State(state): State<AppState>,
    headers: HeaderMap,
    LiveAuth(claims): LiveAuth,
    Path(id): Path<Uuid>,
) -> Result<Response, Response> {
    let auth = authorize(&state, &claims, id).await?;
    let not_enhanced = || {
        api_err(
            StatusCode::CONFLICT,
            "not_enhanced",
            "this conversation does not use the Enhanced tier",
        )
    };
    if !tiers::is_enhanced(
        &state.engines,
        auth.engine_id.as_deref(),
        state.live.is_downgraded(auth.session_id),
    ) {
        return Err(not_enhanced());
    }
    let (Some(cartesia), Some(engine)) = (
        state.config.cartesia.as_ref(),
        state.engines.get(crate::engine::CARTESIA_ID),
    ) else {
        return Err(not_enhanced());
    };

    let ip = crate::observability::client_ip(&headers);
    let window = Duration::from_secs(60);
    if !state.rate_limiter.allow(
        &format!("livetok-seat:{}", auth.peer_id),
        TOKENS_PER_SEAT_PER_MIN,
        window,
    ) || !state.rate_limiter.allow(
        &format!("livetok-sess:{}", auth.session_id),
        TOKENS_PER_SESSION_PER_MIN,
        window,
    ) || !state
        .rate_limiter
        .allow(&format!("livetok-ip:{ip}"), TOKENS_PER_IP_PER_MIN, window)
    {
        return Err(rate_limited());
    }

    // The owner pays for what the token is used for; do not mint one they cannot cover.
    if let Some(owner) = auth.owner_user_id {
        let ok = owner_can_afford(&state, owner, &*engine)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "live: enhanced token balance check failed");
                api_err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "unavailable",
                    "could not check the balance",
                )
            })?;
        if !ok {
            return Err(api_err(
                StatusCode::PAYMENT_REQUIRED,
                "insufficient_balance",
                "The conversation's owner has no credit left.",
            ));
        }
    }

    let (token, expires_at) = crate::api::mint_cartesia_token(&state.http, cartesia)
        .await
        .map_err(|e| {
            // The error text is Cartesia's status and body, never the token (there is none yet).
            tracing::error!(error = %e, "live: enhanced token mint failed");
            api_err(
                StatusCode::BAD_GATEWAY,
                "enhanced_unavailable",
                "the Enhanced service is unavailable",
            )
        })?;
    Ok(Json(crate::api::enhanced_session_body(
        cartesia, &token, expires_at,
    ))
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIM: f64 = 0.6;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn the_two_enhanced_frames_parse_and_the_rest_do_not() {
        let f = EnhancedFrame::parse(
            r#"{"type":"translate_text","segment_id":"s1","text":"ciao a tutti","final":true,"lang":"xx"}"#,
        );
        assert!(matches!(
            f,
            Some(EnhancedFrame::TranslateText { is_final: true, ref text, .. }) if text == "ciao a tutti"
        ));
        // `final` defaults to false: a partial.
        assert!(matches!(
            EnhancedFrame::parse(r#"{"type":"translate_text","segment_id":3,"text":"ciao"}"#),
            Some(EnhancedFrame::TranslateText {
                is_final: false,
                ..
            })
        ));
        assert!(matches!(
            EnhancedFrame::parse(r#"{"type":"enhanced_fallback","reason":"token_refused"}"#),
            Some(EnhancedFrame::Fallback { .. })
        ));
        assert!(matches!(
            EnhancedFrame::parse(r#"{"type":"enhanced_fallback"}"#),
            Some(EnhancedFrame::Fallback { .. })
        ));
        // Not ours: Meet's translate_text has no segment_id; the audio path's frames never match.
        assert!(EnhancedFrame::parse(
            r#"{"type":"translate_text","request_id":"r","text":"x","source":"it","target":"en"}"#
        )
        .is_none());
        assert!(EnhancedFrame::parse(r#"{"type":"start"}"#).is_none());
        assert!(EnhancedFrame::parse("not json translate_text").is_none());
    }

    #[test]
    fn a_segment_id_is_a_short_string_or_a_whole_number() {
        assert!(valid_segment_id(&json!("a")));
        assert!(valid_segment_id(&json!(7)));
        assert!(valid_segment_id(&json!("x".repeat(64))));
        assert!(!valid_segment_id(&json!("x".repeat(65))));
        assert!(!valid_segment_id(&json!("")));
        assert!(!valid_segment_id(&json!(1.5)));
        assert!(!valid_segment_id(&json!(null)));
        assert!(!valid_segment_id(&json!({"a": 1})));
    }

    #[test]
    fn the_speak_frame_carries_the_voice_only_when_there_is_one() {
        let with: Value =
            serde_json::from_str(&speak_frame("p1", &json!("s1"), "es", "hola", Some("v1")))
                .unwrap();
        assert_eq!(with["type"], "enhanced_speak");
        assert_eq!(with["speaker_id"], "p1");
        assert_eq!(with["segment_id"], "s1");
        assert_eq!(with["lang"], "es");
        assert_eq!(with["text"], "hola");
        assert_eq!(with["voice_id"], "v1");
        let without: Value =
            serde_json::from_str(&speak_frame("p1", &json!(4), "es", "hola", None)).unwrap();
        assert!(without["voice_id"].is_null());
        assert_eq!(without["segment_id"], 4);
    }

    #[test]
    fn the_frame_gate_allows_a_burst_then_the_rate_and_refills() {
        let t = t0();
        let mut g = FrameGate::new(t);
        let burst = (0..20).filter(|_| g.admit(t)).count();
        assert_eq!(burst, 8, "one second of burst");
        assert!(!g.admit(t + Duration::from_millis(50)));
        // 125 ms is one frame at 8 a second.
        assert!(g.admit(t + Duration::from_millis(200)));
        // A steady 8/s is never throttled.
        let mut g = FrameGate::new(t);
        for i in 0..80u64 {
            assert!(g.admit(t + Duration::from_millis(i * 125)), "frame {i}");
        }
    }

    #[test]
    fn the_notice_is_due_once_per_interval() {
        let t = t0();
        let mut n = Notifier::default();
        assert!(n.due(t));
        assert!(!n.due(t + Duration::from_secs(9)));
        assert!(n.due(t + Duration::from_secs(10)));
    }

    #[test]
    fn what_another_seat_sent_first_is_an_echo_and_the_first_sender_is_not() {
        let t = t0();
        let mut log = TextLog::default();
        // Seat 0 (the real speaker) is ahead.
        assert!(!log.judge(0, "it", t, "ciao come stai oggi", SIM));
        // Seat 1's phone heard it, a moment later, partially and then whole: both dropped.
        assert!(log.judge(
            1,
            "es",
            t + Duration::from_millis(300),
            "ciao come stai",
            SIM
        ));
        assert!(log.judge(
            1,
            "es",
            t + Duration::from_millis(700),
            "Ciao, come stai oggi?",
            SIM
        ));
        // The speaker's own next partial is never compared with the speaker's own words.
        assert!(!log.judge(
            0,
            "it",
            t + Duration::from_millis(900),
            "ciao come stai oggi bene",
            SIM
        ));
        // A genuine reply is different words.
        assert!(!log.judge(
            1,
            "es",
            t + Duration::from_millis(1500),
            "hola estoy muy bien gracias",
            SIM
        ));
    }

    #[test]
    fn the_guard_forgets_after_the_window_and_does_not_judge_one_word() {
        let t = t0();
        let mut log = TextLog::default();
        assert!(!log.judge(0, "it", t, "ciao come stai", SIM));
        // Past the window the same words from another seat are a new utterance.
        assert!(!log.judge(
            1,
            "es",
            t + ECHO_WINDOW + Duration::from_millis(1),
            "ciao come stai",
            SIM
        ));
        // One word is never judged, nor remembered.
        let mut log = TextLog::default();
        assert!(!log.judge(0, "it", t, "grazie", SIM));
        assert!(!log.judge(1, "es", t, "grazie", SIM));
        // A seat repeating itself is not its own echo.
        let mut log = TextLog::default();
        assert!(!log.judge(0, "it", t, "ciao come stai", SIM));
        assert!(!log.judge(0, "it", t + Duration::from_secs(1), "ciao come stai", SIM));
    }

    #[test]
    fn a_phone_hearing_its_own_tts_is_an_echo_of_what_was_spoken_to_its_language() {
        let t = t0();
        let mut log = TextLog::default();
        // The Spanish speaker's words were translated to Italian and spoken on the Italian phones.
        log.note_spoken(1, "it", t, "ciao come stai oggi amico");
        // An Italian phone's STT hears its own TTS (after ECHO_WINDOW, within SPOKEN_WINDOW).
        assert!(log.judge(
            0,
            "it",
            t + Duration::from_secs(3),
            "ciao come stai oggi amico",
            SIM
        ));
        // A Spanish phone is not in that language group.
        assert!(!log.judge(
            1,
            "es",
            t + Duration::from_secs(3),
            "ciao come stai oggi amico",
            SIM
        ));
        // And it is forgotten after the spoken window.
        let mut log = TextLog::default();
        log.note_spoken(1, "it", t, "ciao come stai oggi amico");
        assert!(!log.judge(
            0,
            "it",
            t + SPOKEN_WINDOW + Duration::from_secs(1),
            "ciao come stai oggi amico",
            SIM
        ));
    }

    #[test]
    fn the_log_stays_bounded() {
        let t = t0();
        let mut log = TextLog::default();
        for i in 0..100u64 {
            log.judge(
                0,
                "it",
                t + Duration::from_millis(i),
                &format!("parola{i} altra{i}"),
                SIM,
            );
            log.note_spoken(
                1,
                "it",
                t + Duration::from_millis(i),
                &format!("detto{i} ancora{i}"),
            );
        }
        assert!(log.entries.len() <= 2 * LOG_PER_SEAT);
    }

    #[test]
    fn a_short_acknowledgement_is_never_taken_for_an_echo() {
        let t = t0();
        for (a, b) in [
            ("ok perfecto", "ok perfecto"),
            ("vale gracias", "Vale, gracias!"),
            ("si certo", "sì certo"),
            ("ok", "ok"),
        ] {
            // Said first by one seat, and again by the other right after.
            let mut log = TextLog::default();
            assert!(!log.judge(0, "it", t, a, SIM), "{a}");
            assert!(
                !log.judge(1, "es", t + Duration::from_millis(200), b, SIM),
                "{b}"
            );
            // Spoken to its own language a moment ago.
            let mut log = TextLog::default();
            log.note_spoken(0, "es", t, a);
            assert!(
                !log.judge(1, "es", t + Duration::from_millis(200), b, SIM),
                "{b}"
            );
        }
        // Three words is the least that is judged.
        let mut log = TextLog::default();
        assert!(!log.judge(0, "it", t, "va bene grazie", SIM));
        assert!(log.judge(
            1,
            "es",
            t + Duration::from_millis(200),
            "va bene grazie",
            SIM
        ));
    }

    #[test]
    fn a_lookalike_below_the_floor_is_not_an_echo_whatever_the_knob_says() {
        let t = t0();
        let mut log = TextLog::default();
        assert!(!log.judge(0, "it", t, "ciao come stai oggi amico", 0.3));
        // Four of its five words: 0.8 of it, under the 0.85 floor even with the knob at 0.3.
        assert!(!log.judge(
            1,
            "es",
            t + Duration::from_millis(200),
            "ciao come va oggi amico",
            0.3
        ));
        assert!(echo::similarity("ciao come va oggi amico", "ciao come stai oggi amico") < 0.85);
    }

    #[test]
    fn the_windows_are_a_few_seconds() {
        let t = t0();
        // What another seat sent: dropped just inside ECHO_WINDOW, a new utterance just after.
        let mut log = TextLog::default();
        assert!(!log.judge(0, "it", t, "ciao come stai oggi", SIM));
        assert!(log.judge(
            1,
            "es",
            t + ECHO_WINDOW - Duration::from_millis(1),
            "ciao come stai oggi",
            SIM
        ));
        let mut log = TextLog::default();
        assert!(!log.judge(0, "it", t, "ciao come stai oggi", SIM));
        assert!(!log.judge(
            1,
            "es",
            t + ECHO_WINDOW + Duration::from_millis(1),
            "ciao come stai oggi",
            SIM
        ));
        // What was spoken to a language: same for SPOKEN_WINDOW.
        let mut log = TextLog::default();
        log.note_spoken(1, "it", t, "ciao come stai oggi");
        assert!(log.judge(
            0,
            "it",
            t + SPOKEN_WINDOW - Duration::from_millis(1),
            "ciao come stai oggi",
            SIM
        ));
        let mut log = TextLog::default();
        log.note_spoken(1, "it", t, "ciao come stai oggi");
        assert!(!log.judge(
            0,
            "it",
            t + SPOKEN_WINDOW + Duration::from_millis(1),
            "ciao come stai oggi",
            SIM
        ));
        assert_eq!(ECHO_WINDOW, Duration::from_millis(2500));
        assert_eq!(SPOKEN_WINDOW, Duration::from_secs(4));
    }

    #[test]
    fn a_seat_being_played_my_words_is_the_echo_not_me() {
        let t = t0();
        // The Spanish seat 0's phone is being played what the Italian seat 1 just said, and its own
        // STT sends those words. Seat 1 then says them for real: seat 1 is not the echo.
        let mut log = TextLog::default();
        log.note_spoken(1, "es", t, "vale lo hago ahora mismo");
        assert!(!log.judge(0, "xx", t + Duration::from_millis(100), "zzz yyy www", SIM));
        let mut log = TextLog::default();
        // Seat 0 (es) sent the text; seat 1 said it before, and seat 0 is hearing it spoken.
        assert!(!log.judge(0, "es", t, "vale lo hago ahora mismo", SIM));
        log.note_spoken(
            1,
            "es",
            t + Duration::from_millis(100),
            "vale lo hago ahora mismo",
        );
        assert!(
            !log.judge(
                1,
                "it",
                t + Duration::from_millis(500),
                "vale lo hago ahora mismo",
                SIM
            ),
            "seat 0 was hearing seat 1: its text is the echo, seat 1 is the speaker"
        );
        // Without that, the same order is the ordinary "seat 0 said it first" case.
        let mut log = TextLog::default();
        assert!(!log.judge(0, "es", t, "vale lo hago ahora mismo", SIM));
        assert!(log.judge(
            1,
            "it",
            t + Duration::from_millis(500),
            "vale lo hago ahora mismo",
            SIM
        ));
        // Hearing somebody else's words says nothing about seat 1.
        let mut log = TextLog::default();
        assert!(!log.judge(0, "es", t, "vale lo hago ahora mismo", SIM));
        log.note_spoken(
            2,
            "es",
            t + Duration::from_millis(100),
            "otra cosa muy distinta ahora",
        );
        assert!(log.judge(
            1,
            "it",
            t + Duration::from_millis(500),
            "vale lo hago ahora mismo",
            SIM
        ));
    }

    #[test]
    fn a_seat_that_said_it_first_is_not_its_own_echo() {
        let t = t0();
        let mut log = TextLog::default();
        // Seat 1 starts the sentence; seat 0 says the same thing later (outside the window, so it
        // is accepted); then seat 1 finishes it. Seat 1 was first.
        assert!(!log.judge(1, "es", t, "ciao come stai", SIM));
        assert!(!log.judge(
            0,
            "it",
            t + Duration::from_secs(3),
            "ciao come stai oggi",
            SIM
        ));
        assert!(!log.judge(
            1,
            "es",
            t + Duration::from_millis(3300),
            "ciao come stai oggi",
            SIM
        ));
        // A seat that had NOT said it is still judged.
        let mut log = TextLog::default();
        assert!(!log.judge(0, "it", t, "ciao come stai oggi", SIM));
        assert!(log.judge(
            1,
            "es",
            t + Duration::from_millis(300),
            "ciao come stai",
            SIM
        ));
    }

    #[test]
    fn a_long_token_of_a_script_without_spaces_is_still_judged() {
        let t = t0();
        let mut log = TextLog::default();
        let line = "今日はとても良い天気ですね本当に";
        assert!(!log.judge(0, "ja", t, line, SIM));
        assert!(log.judge(1, "zh", t + Duration::from_millis(200), line, SIM));
        // A short one is not.
        let mut log = TextLog::default();
        assert!(!log.judge(0, "ja", t, "ありがとう", SIM));
        assert!(!log.judge(1, "zh", t + Duration::from_millis(200), "ありがとう", SIM));
    }

    #[test]
    fn interim_captions_have_their_own_smaller_gate() {
        let t = t0();
        let mut g = FrameGate::with_rate(PARTIALS_PER_SEC, t);
        assert_eq!((0..10).filter(|_| g.admit(t)).count(), 2, "a burst of two");
        assert!(!g.admit(t + Duration::from_millis(400)));
        assert!(
            g.admit(t + Duration::from_millis(500)),
            "2 a second refills"
        );
    }

    #[test]
    fn an_interim_is_cut_to_its_last_four_hundred_characters() {
        let short = "a".repeat(MAX_PARTIAL_CHARS);
        assert_eq!(clip_partial(&short), short);
        let long = format!("{}{}", "x".repeat(600), "fine");
        let clipped = clip_partial(&long);
        assert_eq!(clipped.chars().count(), MAX_PARTIAL_CHARS);
        assert!(clipped.ends_with("fine"));
        // Never in the middle of a character.
        let wide = "é".repeat(MAX_PARTIAL_CHARS + 7);
        assert_eq!(clip_partial(&wide).chars().count(), MAX_PARTIAL_CHARS);
        assert_eq!(clip_partial(""), "");
    }

    #[test]
    fn the_call_ceiling_reserves_room_for_finals_and_resets_each_minute() {
        let t = t0();
        let mut b = TranslationBudget::new(100);
        // Interim captions get 60 of the 100.
        assert_eq!((0..200).filter(|_| b.allow(t, false)).count(), 60);
        // The finals still have the other 40, and no more.
        assert_eq!((0..200).filter(|_| b.allow(t, true)).count(), 40);
        assert!(!b.allow(t + Duration::from_secs(59), true));
        // A new minute starts again.
        assert!(b.allow(t + Duration::from_secs(60), false));
        assert!(b.allow(t + Duration::from_secs(60), true));
    }
}
