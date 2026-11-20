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
//! arrived first is taken to be the speaker. When the bystander's STT happens to win the race the
//! guard keeps the echo and drops the real words; that is the honest limit of a guard with no
//! loudness to break the tie, and why it only judges text of at least two words.
//!
//! A phone also hears ITS OWN text-to-speech. What was last spoken to a language group is
//! remembered for [`SPOKEN_WINDOW`], and a seat of that language whose text is similar to it is an
//! echo of its own phone's TTS, the loop that would otherwise translate and bill a translation of
//! a translation. (The client should also pause or echo-cancel STT while it plays TTS; this is the
//! backstop.)
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
use tokio::sync::mpsc;
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
pub const ECHO_WINDOW: Duration = Duration::from_secs(4);
/// How long text spoken to a language group counts as "my own phone's TTS" for the echo guard.
/// Longer than [`ECHO_WINDOW`]: TTS plays for seconds after it is sent.
pub const SPOKEN_WINDOW: Duration = Duration::from_secs(12);
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

/// A token bucket in frames: [`FRAMES_PER_SEC`] a second with one second of burst.
#[derive(Debug)]
pub struct FrameGate {
    tokens: f64,
    last: Instant,
}

impl FrameGate {
    pub fn new(now: Instant) -> Self {
        Self {
            tokens: FRAMES_PER_SEC,
            last: now,
        }
    }

    pub fn admit(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * FRAMES_PER_SEC).min(FRAMES_PER_SEC);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
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
    /// Text a seat sent (accepted).
    Sent { seat: usize },
    /// Text spoken to the phones of a language.
    Spoken { lang: String },
}

#[derive(Debug)]
struct Entry {
    kind: Kind,
    at: Instant,
    norm: String,
}

/// What the seats of one conversation recently sent and were told to speak. See the module
/// documentation for what this can and cannot decide.
#[derive(Debug, Default)]
pub struct TextLog {
    entries: Vec<Entry>,
}

impl TextLog {
    /// Judge a frame of `seat` (declared language `lang`): `true` is "echo, drop it". A frame that
    /// is not an echo is remembered, so what it said can make a later frame of another seat an
    /// echo. Text of fewer than two words is never judged and never remembered.
    pub fn judge(&mut self, seat: usize, lang: &str, now: Instant, text: &str, sim: f64) -> bool {
        let norm = echo::normalize(text);
        if !echo::enough(&norm) {
            return false;
        }
        self.entries
            .retain(|e| now.saturating_duration_since(e.at) <= SPOKEN_WINDOW);
        let is_echo = self.entries.iter().any(|e| {
            let age = now.saturating_duration_since(e.at);
            let relevant = match &e.kind {
                Kind::Sent { seat: s } => *s != seat && age <= ECHO_WINDOW,
                Kind::Spoken { lang: l } => l == lang,
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
            .find(|e| matches!(e.kind, Kind::Sent { seat: s } if s == seat))
            .is_some_and(|e| e.norm == norm);
        if !dup {
            let mine = self
                .entries
                .iter()
                .filter(|e| matches!(e.kind, Kind::Sent { seat: s } if s == seat))
                .count();
            if mine >= LOG_PER_SEAT {
                if let Some(i) = self
                    .entries
                    .iter()
                    .position(|e| matches!(e.kind, Kind::Sent { seat: s } if s == seat))
                {
                    self.entries.remove(i);
                }
            }
            self.entries.push(Entry {
                kind: Kind::Sent { seat },
                at: now,
                norm,
            });
        }
        false
    }

    /// `text` was just sent to the phones whose language is `lang` to be spoken.
    pub fn note_spoken(&mut self, lang: &str, now: Instant, text: &str) {
        let norm = echo::normalize(text);
        if !echo::enough(&norm) {
            return;
        }
        self.entries
            .retain(|e| now.saturating_duration_since(e.at) <= SPOKEN_WINDOW);
        // A bounded log: the newest few per language are all that can still be echoing.
        let mine = self
            .entries
            .iter()
            .filter(|e| matches!(&e.kind, Kind::Spoken { lang: l } if l == lang))
            .count();
        if mine >= LOG_PER_SEAT {
            if let Some(i) = self
                .entries
                .iter()
                .position(|e| matches!(&e.kind, Kind::Spoken { lang: l } if l == lang))
            {
                self.entries.remove(i);
            }
        }
        self.entries.push(Entry {
            kind: Kind::Spoken { lang: lang.into() },
            at: now,
            norm,
        });
    }
}

// ---- translation and delivery ----------------------------------------------------------

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
        while let Some(mut seg) = rx.recv().await {
            while !seg.is_final {
                match rx.try_recv() {
                    Ok(next) => seg = next,
                    Err(_) => break,
                }
            }
            self.process(seg, &mut notice).await;
        }
    }

    async fn process(&self, seg: Segment, notice: &mut Notifier) {
        let original = seg.text.trim().to_string();
        if original.is_empty() {
            return;
        }
        let ts = Utc::now();
        // Everybody else in the room, by distinct language. The speaker never appears: the room
        // excludes its own id, and every frame below is addressed by language except the speaker.
        let langs = self.rooms.get_room_languages(&self.room, &self.speaker_id);
        let targets: Vec<&String> = langs.iter().filter(|l| **l != self.speaker_lang).collect();

        // Listeners who share the speaker's language need no translation: the original is the
        // caption, and they hear the speaker themselves. Sent first, before any Groq round trip.
        if langs.contains(&self.speaker_lang) {
            self.caption(&self.speaker_lang, &original, None, seg.is_final);
        }

        let tr = &self.translator;
        let (src, text) = (self.speaker_lang.as_str(), original.as_str());
        let results = futures::future::join_all(targets.iter().map(|t| async move {
            let r =
                tokio::time::timeout(TRANSLATE_TIMEOUT, tr.translate_uncached(text, src, t)).await;
            ((*t).clone(), r)
        }))
        .await;

        let mut translations: HashMap<String, String> = HashMap::new();
        translations.insert(self.speaker_lang.clone(), original.clone());
        let mut failed = false;
        for (lang, r) in results {
            match r {
                Ok(Ok(t)) if !t.trim().is_empty() => {
                    let t = t.trim().to_string();
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
                // A timeout, a provider error or an empty answer: that language gets nothing for
                // this segment (never the untranslated words, which would be captioned and
                // SPOKEN in the wrong language).
                _ => failed = true,
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

    fn note_spoken(&self, lang: &str, text: &str) {
        self.latency.note_spoken(lang, Instant::now(), text);
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
        assert!(log.judge(1, "es", t + Duration::from_millis(300), "ciao come", SIM));
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
        log.note_spoken("it", t, "ciao come stai oggi amico");
        // An Italian phone's STT hears its own TTS (well after ECHO_WINDOW, within SPOKEN_WINDOW).
        assert!(log.judge(
            0,
            "it",
            t + Duration::from_secs(6),
            "ciao come stai oggi amico",
            SIM
        ));
        // A Spanish phone is not in that language group.
        assert!(!log.judge(
            1,
            "es",
            t + Duration::from_secs(6),
            "ciao come stai oggi amico",
            SIM
        ));
        // And it is forgotten after the spoken window.
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
                "it",
                t + Duration::from_millis(i),
                &format!("detto{i} ancora{i}"),
            );
        }
        assert!(log.entries.len() <= 2 * LOG_PER_SEAT);
    }
}
