//! One utterance's life, from first partial to spoken translation (spec 0110).
//!
//! Both translation directions run at once (two upstream sessions, one per configured
//! language), so for every spoken sentence the room produces TWO sets of subtitles and
//! TWO streams of translated audio — and exactly one of them is real. The other is a
//! source→source session echoing the speaker back at themselves, which must never reach
//! the speakers: it is both nonsense and the seed of a feedback loop.
//!
//! This type decides which is which, and holds the audio until it can. The hold is the
//! only latency this feature adds, it applies once per utterance, and it is bounded —
//! never a fixed delay (brief §44).
//!
//! It is deliberately pure: no channels, no tasks, no clock. The handler feeds it frames
//! and plays back whatever it hands out.

use super::direction::Direction;

/// Largest number of translated-audio frames held per language while the direction is
/// still unknown. ~100 ms of PCM each, so this is a couple of seconds of headroom.
/// Overflow drops the OLDEST frame: a late translation is worse than a clipped one
/// (brief §27 — realtime relevance beats delivery of every historical segment).
pub const MAX_HELD_FRAMES: usize = 24;

/// How many more characters must arrive before we pay for another classification. Every
/// partial would otherwise fire a Groq call several times a second.
pub const RESOLVE_STEP_CHARS: usize = 8;

/// The kinds of frame this layer treats differently. Everything else is forwarded
/// untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    SubtitleInterim,
    SubtitleFinal,
    TranslatedAudio,
    Other,
}

impl FrameKind {
    /// Classify a serialized [`crate::protocol::ServerMessage`] without parsing it.
    ///
    /// `ServerMessage` is `#[serde(tag = "type")]`, so the discriminant is always the
    /// first key. Audio frames carry kilobytes of base64 and arrive many times a second;
    /// a full `serde_json` parse of each one to read a field we already know from the
    /// channel it arrived on would be pure waste. The prefixes are pinned by a test
    /// against real `to_json()` output so this cannot drift silently.
    pub fn classify(frame: &str) -> Self {
        if frame.starts_with(r#"{"type":"translated_audio""#) {
            FrameKind::TranslatedAudio
        } else if frame.starts_with(r#"{"type":"subtitle_interim""#) {
            FrameKind::SubtitleInterim
        } else if frame.starts_with(r#"{"type":"subtitle_final""#) {
            FrameKind::SubtitleFinal
        } else {
            FrameKind::Other
        }
    }
}

/// What the handler should do with a frame it just received.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Send these frames on to the browser, in order. Usually one; more when a commit
    /// flushes held audio ahead of the frame that triggered it.
    Send(Vec<String>),
    /// Swallow it. Either it belongs to the suppressed direction, or it is being held
    /// until the direction is known.
    Hold,
}

impl Outcome {
    fn one(frame: String) -> Self {
        Outcome::Send(vec![frame])
    }
}

/// Per-utterance state. Reset at every final.
pub struct Utterance {
    user_lang: String,
    other_lang: String,
    /// Latched once committed: a direction never flips mid-utterance. A speaker who
    /// switches language does so between sentences, and a mid-sentence flip would split
    /// one thought across two voices.
    direction: Direction,
    /// The accumulated ORIGINAL transcript for this utterance, as last reported.
    original: String,
    /// Length of `original` when we last attempted a classification, so growth can be
    /// throttled to [`RESOLVE_STEP_CHARS`].
    resolved_at_len: usize,
    /// Translated audio waiting for a direction, per language.
    held_user: Vec<String>,
    held_other: Vec<String>,
    /// The last final we forwarded, to swallow provider-side repeats.
    last_final: Option<String>,
}

impl Utterance {
    pub fn new(user_lang: impl Into<String>, other_lang: impl Into<String>) -> Self {
        Self {
            user_lang: user_lang.into(),
            other_lang: other_lang.into(),
            direction: Direction::Unknown,
            original: String::new(),
            resolved_at_len: 0,
            held_user: Vec::new(),
            held_other: Vec::new(),
            last_final: None,
        }
    }

    pub fn direction(&self) -> Direction {
        self.direction
    }

    pub fn original(&self) -> &str {
        &self.original
    }

    /// Is `lang` the side whose output should be spoken? `false` while uncommitted.
    fn is_target(&self, lang: &str) -> bool {
        self.direction.target(&self.user_lang, &self.other_lang) == Some(lang)
    }

    /// Record the original transcript reported by an interim or final frame.
    ///
    /// The engine reports the ACCUMULATED original for the segment, and it may revise it
    /// (a later partial is not always an extension of the earlier one — see
    /// `qwen::TextUpdate`). So this replaces rather than appends; appending would double
    /// every caption. Returns `true` when the text has moved enough to be worth another
    /// classification.
    pub fn note_original(&mut self, text: &str) -> bool {
        let text = text.trim();
        if text.is_empty() || text == self.original {
            return false;
        }
        let revised = !text.starts_with(self.original.as_str());
        self.original = text.to_string();
        if self.direction != Direction::Unknown {
            // Already latched — keep the text current for the UI, but do not pay to
            // re-classify what we have already committed to.
            return false;
        }
        if revised {
            // The transcript was rewritten, not extended: whatever we concluded from the
            // old text is void, so re-classify immediately.
            self.resolved_at_len = self.original.chars().count();
            return true;
        }
        let len = self.original.chars().count();
        if len >= self.resolved_at_len + RESOLVE_STEP_CHARS {
            self.resolved_at_len = len;
            true
        } else {
            false
        }
    }

    /// Latch a direction and release the held audio for the winning side.
    ///
    /// Returns `None` when nothing latched — an [`Direction::Unknown`] verdict, or a late
    /// second verdict for an utterance already decided. That distinction is the caller's
    /// signal to announce the direction exactly ONCE: a second announcement per utterance
    /// re-renders the chip and, worse, double-counts the resolved metric that tells us
    /// whether people are hearing deliberate silence. Returning a bare `Vec` made "did it
    /// latch?" a judgement call at every call site; this makes it impossible to get wrong.
    ///
    /// The losing side's buffer is dropped on the floor — that audio is the echo we exist
    /// to suppress.
    pub fn commit(&mut self, direction: Direction) -> Option<Vec<String>> {
        if direction == Direction::Unknown || self.direction != Direction::Unknown {
            return None;
        }
        self.direction = direction;
        let (winner, loser) = match direction {
            Direction::UserToOther => (&mut self.held_other, &mut self.held_user),
            Direction::OtherToUser => (&mut self.held_user, &mut self.held_other),
            Direction::Unknown => unreachable!("guarded above"),
        };
        loser.clear();
        Some(std::mem::take(winner))
    }

    /// Route one frame that arrived on `lang`'s channel.
    pub fn on_frame(&mut self, lang: &str, frame: String) -> Outcome {
        match FrameKind::classify(&frame) {
            FrameKind::TranslatedAudio => self.on_audio(lang, frame),
            FrameKind::SubtitleFinal => self.on_final(lang, frame),
            FrameKind::SubtitleInterim => self.on_interim(lang, frame),
            FrameKind::Other => Outcome::one(frame),
        }
    }

    fn on_audio(&mut self, lang: &str, frame: String) -> Outcome {
        match self.direction {
            Direction::Unknown => {
                let buf = if lang == self.user_lang {
                    &mut self.held_user
                } else {
                    &mut self.held_other
                };
                if buf.len() >= MAX_HELD_FRAMES {
                    // We have waited long enough that the head of this translation is no
                    // longer worth playing. Keep the tail, stay bounded.
                    buf.remove(0);
                }
                buf.push(frame);
                Outcome::Hold
            }
            _ if self.is_target(lang) => Outcome::one(frame),
            // The echo direction. Never played, never queued.
            _ => Outcome::Hold,
        }
    }

    fn on_interim(&mut self, lang: &str, frame: String) -> Outcome {
        if self.direction == Direction::Unknown {
            // Both sides are still candidates, so neither caption is trustworthy yet.
            // The UI shows "Listening…" during this window rather than a translation it
            // may have to retract.
            return Outcome::Hold;
        }
        if self.is_target(lang) {
            Outcome::one(frame)
        } else {
            Outcome::Hold
        }
    }

    fn on_final(&mut self, lang: &str, frame: String) -> Outcome {
        if self.direction == Direction::Unknown {
            // No direction was ever established for this utterance — too short, or a
            // third language. Say nothing (brief §6/§16) and start clean.
            self.reset();
            return Outcome::Hold;
        }
        if !self.is_target(lang) {
            // The echo side's final. Dropping it here is also what stops the LOSING
            // session from ending the utterance early for the winning one.
            return Outcome::Hold;
        }
        if self.last_final.as_deref() == Some(frame.as_str()) {
            // Providers do repeat a final. Forwarding it twice speaks the sentence twice.
            return Outcome::Hold;
        }
        self.last_final = Some(frame.clone());
        self.reset_keeping_last_final();
        Outcome::one(frame)
    }

    /// Start the next utterance. The duplicate-final guard is cleared too: a genuinely
    /// repeated sentence, spoken twice, must be translated twice.
    pub fn reset(&mut self) {
        self.reset_keeping_last_final();
        self.last_final = None;
    }

    fn reset_keeping_last_final(&mut self) {
        self.direction = Direction::Unknown;
        self.original.clear();
        self.resolved_at_len = 0;
        self.held_user.clear();
        self.held_other.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ServerMessage;
    use std::collections::HashMap;

    fn audio(lang: &str, seq: u64) -> String {
        ServerMessage::TranslatedAudio {
            speaker_id: "src".into(),
            lang: lang.into(),
            seq,
            pcm16_b64: "AAAA".into(),
        }
        .to_json()
    }

    fn interim(text: &str, original: &str) -> String {
        ServerMessage::SubtitleInterim {
            speaker_id: "src".into(),
            speaker_name: "src".into(),
            text: text.into(),
            lang: "auto".into(),
            original: Some(original.into()),
        }
        .to_json()
    }

    fn final_msg(original: &str, lang: &str, translated: &str) -> String {
        let mut translations = HashMap::new();
        translations.insert(lang.to_string(), translated.to_string());
        ServerMessage::SubtitleFinal {
            speaker_id: "src".into(),
            speaker_name: "src".into(),
            original: original.into(),
            lang: "auto".into(),
            translations,
        }
        .to_json()
    }

    /// The cheap prefix classifier is only safe as long as it matches what the protocol
    /// actually serializes. Pin it against real frames, not string literals.
    #[test]
    fn frame_kinds_match_real_serialized_messages() {
        assert_eq!(
            FrameKind::classify(&audio("es", 0)),
            FrameKind::TranslatedAudio
        );
        assert_eq!(
            FrameKind::classify(&interim("hola", "ciao")),
            FrameKind::SubtitleInterim
        );
        assert_eq!(
            FrameKind::classify(&final_msg("ciao", "es", "hola")),
            FrameKind::SubtitleFinal
        );
        assert_eq!(
            FrameKind::classify(&ServerMessage::BalanceExhausted.to_json()),
            FrameKind::Other
        );
    }

    #[test]
    fn audio_is_held_until_the_direction_is_known_then_only_the_winner_plays() {
        let mut u = Utterance::new("it", "es");
        // Italian is being spoken, so both sessions produce audio: es is the translation,
        // it is the echo. Neither may play yet.
        assert_eq!(u.on_frame("es", audio("es", 0)), Outcome::Hold);
        assert_eq!(u.on_frame("it", audio("it", 0)), Outcome::Hold);
        assert_eq!(u.on_frame("es", audio("es", 1)), Outcome::Hold);

        // Direction resolves: user (it) spoke, so the Spanish output is the real one.
        let flushed = u
            .commit(Direction::UserToOther)
            .expect("the first verdict latches");
        assert_eq!(flushed, vec![audio("es", 0), audio("es", 1)]);

        // From here the winner streams through and the echo is dropped on arrival.
        assert_eq!(
            u.on_frame("es", audio("es", 2)),
            Outcome::one(audio("es", 2))
        );
        assert_eq!(u.on_frame("it", audio("it", 1)), Outcome::Hold);
    }

    #[test]
    fn the_echo_direction_never_reaches_the_speakers() {
        // This is the feedback-loop guard: playing the it→it echo through the phone's
        // speaker is exactly how the loop in brief §9 starts.
        let mut u = Utterance::new("it", "es");
        u.commit(Direction::UserToOther);
        assert_eq!(u.on_frame("it", audio("it", 0)), Outcome::Hold);
        assert_eq!(u.on_frame("it", interim("ciao", "ciao")), Outcome::Hold);
        assert_eq!(
            u.on_frame("it", final_msg("ciao", "it", "ciao")),
            Outcome::Hold
        );
    }

    #[test]
    fn held_audio_is_bounded_and_drops_the_oldest() {
        let mut u = Utterance::new("it", "es");
        for seq in 0..(MAX_HELD_FRAMES as u64 + 5) {
            assert_eq!(u.on_frame("es", audio("es", seq)), Outcome::Hold);
        }
        let flushed = u.commit(Direction::UserToOther).expect("latched");
        assert_eq!(flushed.len(), MAX_HELD_FRAMES);
        // The tail survived, the head was dropped — a clipped translation, never a stale
        // one.
        assert_eq!(flushed[0], audio("es", 5));
        assert_eq!(
            flushed[MAX_HELD_FRAMES - 1],
            audio("es", MAX_HELD_FRAMES as u64 + 4)
        );
    }

    #[test]
    fn a_duplicate_final_does_not_speak_twice() {
        let mut u = Utterance::new("it", "es");
        u.commit(Direction::UserToOther);
        let f = final_msg("ciao", "es", "hola");
        assert_eq!(u.on_frame("es", f.clone()), Outcome::one(f.clone()));

        // The provider repeats it. The utterance has already reset, so it arrives with
        // no direction — and is swallowed either way.
        assert_eq!(u.on_frame("es", f.clone()), Outcome::Hold);

        // A genuinely repeated sentence in a NEW utterance must still be translated.
        u.reset();
        u.commit(Direction::UserToOther);
        assert_eq!(u.on_frame("es", f.clone()), Outcome::one(f));
    }

    #[test]
    fn an_unresolved_utterance_says_nothing_and_resets() {
        // Two words in a third language: no direction, no speech, and the next utterance
        // starts clean rather than inheriting held audio.
        let mut u = Utterance::new("it", "es");
        assert_eq!(u.on_frame("es", audio("es", 0)), Outcome::Hold);
        assert_eq!(
            u.on_frame("es", final_msg("guten tag", "es", "buenos dias")),
            Outcome::Hold
        );
        assert_eq!(u.direction(), Direction::Unknown);
        assert_eq!(u.original(), "");
        // Nothing was retained to leak into the next sentence.
        assert_eq!(u.commit(Direction::UserToOther), Some(Vec::new()));
    }

    #[test]
    fn partials_replace_rather_than_append() {
        let mut u = Utterance::new("it", "es");
        u.note_original("Vorrei andare");
        u.note_original("Vorrei andare alla stazione");
        assert_eq!(u.original(), "Vorrei andare alla stazione");
        // A revision that shortens the text must not leave the old tail behind.
        u.note_original("Vorrei un caffè");
        assert_eq!(u.original(), "Vorrei un caffè");
    }

    #[test]
    fn reclassification_is_throttled_but_a_revision_forces_one() {
        let mut u = Utterance::new("it", "es");
        // First real text asks for a verdict.
        assert!(u.note_original("Vorrei andare"));
        // A one-character growth is not worth another model call.
        assert!(!u.note_original("Vorrei andare "));
        assert!(!u.note_original("Vorrei andare a"));
        // Enough new evidence — ask again.
        assert!(u.note_original("Vorrei andare alla stazione"));
        // A rewrite invalidates the previous conclusion regardless of length.
        assert!(u.note_original("Quiero ir a la"));
    }

    #[test]
    fn a_committed_utterance_stops_paying_for_classification() {
        let mut u = Utterance::new("it", "es");
        u.note_original("Vorrei andare");
        u.commit(Direction::UserToOther);
        // The text keeps flowing for the caption, but never triggers another call.
        assert!(!u.note_original("Vorrei andare alla stazione centrale"));
        assert_eq!(u.original(), "Vorrei andare alla stazione centrale");
    }

    #[test]
    fn a_direction_never_flips_mid_utterance() {
        let mut u = Utterance::new("it", "es");
        // Committing Unknown is a no-op, not a latch: the caller must keep holding, and
        // `None` is what tells it so.
        assert_eq!(u.commit(Direction::Unknown), None);
        assert_eq!(u.direction(), Direction::Unknown);
        assert!(u.commit(Direction::UserToOther).is_some());
        // A late, contradicting verdict is ignored: one sentence, one voice — and `None`
        // stops the caller announcing the direction a second time.
        assert_eq!(u.commit(Direction::OtherToUser), None);
        assert_eq!(u.direction(), Direction::UserToOther);
    }

    #[test]
    fn other_frames_pass_straight_through() {
        // Errors, balance updates and moderation warnings are not ours to gate.
        let mut u = Utterance::new("it", "es");
        let msg = ServerMessage::LowBalance { balance: 0.2 }.to_json();
        assert_eq!(u.on_frame("es", msg.clone()), Outcome::one(msg));
    }
}
