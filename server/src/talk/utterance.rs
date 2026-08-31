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
/// still unknown. ~100 ms of PCM each, so this covers roughly ten seconds — enough for a
/// whole sentence, because the direction is now often only settled by the FINAL
/// transcript. At the old 24 (~2.4 s) the head of every late-resolved translation was
/// silently clipped. Overflow still drops the OLDEST frame: a late translation is worse
/// than a clipped one (brief §27).
pub const MAX_HELD_FRAMES: usize = 100;

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
    /// The ORIGINAL text of the last sentence forwarded, so the losing session's copy of
    /// it can be recognised when it lands afterwards. See [`Self::is_trailing_echo`].
    spoken_original: Option<String>,
    /// The sentence has been forwarded but its translated speech is still arriving.
    ///
    /// The engine closes a segment on an idle gap in the TRANSCRIPT, and text finishes
    /// long before the audio it describes — so a final routinely lands while seconds of
    /// its own translation are still streaming. Clearing the direction there orphaned all
    /// of it: held with no direction, then dropped when the next sentence resolved the
    /// other way. That is what "it only says half the sentence" was.
    ///
    /// While draining, the direction still routes audio; it is dropped the moment new
    /// speech arrives, because a direction may outlive its sentence but never its speaker.
    draining: bool,
    /// A final that arrived before the direction was known, kept so a late verdict can
    /// still release it. Dropping it — which is what this did — threw away the single
    /// best piece of evidence about the utterance AND the sentence itself: in production
    /// 12 of 17 utterances ended here and were never spoken.
    ///
    /// One slot PER SIDE, not one in total. Both sessions finalize every sentence over
    /// independent sockets, so which copy arrives first is a race — and keeping only the
    /// winner of that race lost the sentence outright whenever the echo won it.
    pending_user: Option<String>,
    pending_other: Option<String>,
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
            spoken_original: None,
            draining: false,
            pending_user: None,
            pending_other: None,
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
    pub fn note_original(&mut self, lang: &str, text: &str) -> bool {
        let text = text.trim();
        if text.is_empty() || text == self.original {
            return false;
        }
        // The losing session's copy of the sentence we just forwarded. Not new speech, so
        // it must not end the drain — doing so would strand the audio it was protecting.
        // The SIDE is what separates it from someone genuinely saying "Sì." twice: the
        // tail can only come from the session we did not speak.
        if self.is_trailing_echo(lang, text) {
            return false;
        }
        if self.draining {
            // Genuinely new words: the previous sentence's direction stops being current
            // HERE, not at its final.
            self.draining = false;
            self.direction = Direction::Unknown;
            self.held_user.clear();
            self.held_other.clear();
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
            // The direction is not known YET. Park this — the final carries the complete
            // transcript, which is the best evidence we will ever get about what was
            // spoken, so the caller classifies it and releases the sentence if a verdict
            // arrives. Resetting here instead is what made most sentences vanish.
            // Park it on ITS OWN side: the other side is not the echo, it is the same
            // sentence from the other session, and only the verdict can say which is
            // which. Within one side the FIRST copy wins — a provider repeat must not
            // displace the sentence we already have.
            let slot = if lang == self.user_lang {
                &mut self.pending_user
            } else {
                &mut self.pending_other
            };
            if slot.is_none() {
                *slot = Some(frame);
            }
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
        self.finish_sentence();
        Outcome::one(frame)
    }

    /// The sentence has been forwarded. Clear everything about it EXCEPT the direction,
    /// which stays live to route the translated speech still on its way.
    fn finish_sentence(&mut self) {
        let spoken = std::mem::take(&mut self.original);
        self.spoken_original = (!spoken.is_empty()).then_some(spoken);
        self.pending_user = None;
        self.pending_other = None;
        self.resolved_at_len = 0;
        self.held_user.clear();
        self.held_other.clear();
        self.draining = self.direction != Direction::Unknown;
    }

    /// Is `text` the losing session's copy of the sentence we have just forwarded?
    ///
    /// Both sessions finalize every sentence, and the loser's copy always lands AFTER the
    /// winner's — by which point this utterance has reset. Fed through the normal path
    /// that tail re-opened the sentence: it re-latched a direction from the echo's own
    /// text, and `note_original` then refuses to re-classify a latched utterance, so the
    /// NEXT sentence was routed by the previous one's direction. One trailing frame
    /// inverted a whole conversational turn.
    ///
    /// Deliberately narrow: it only holds while the utterance has heard nothing since the
    /// reset. A genuine repeat ("Sì." twice) arrives with its own partials, and those are
    /// exactly the evidence that clears this.
    pub fn is_trailing_echo(&self, lang: &str, text: &str) -> bool {
        self.original.is_empty()
            && self.spoken_original.as_deref() == Some(text.trim())
            && !self.is_target(lang)
    }

    /// A final is parked waiting for a direction, on either side.
    pub fn has_pending_final(&self) -> bool {
        self.pending_user.is_some() || self.pending_other.is_some()
    }

    /// Release a parked final now that a direction exists — `Some` only if the side it
    /// arrived on turned out to be the one worth speaking. Either way the park is
    /// cleared, so a stale final can never leak into the next sentence.
    pub fn take_pending_final(&mut self) -> Option<String> {
        // Both parks are cleared whatever the verdict: the losing side's copy has served
        // its only purpose, and a survivor would leak into the next sentence.
        let user = self.pending_user.take();
        let other = self.pending_other.take();
        let target = self.direction.target(&self.user_lang, &self.other_lang)?;
        let frame = if target == self.user_lang {
            user
        } else {
            other
        }?;
        if self.last_final.as_deref() == Some(frame.as_str()) {
            return None;
        }
        self.last_final = Some(frame.clone());
        Some(frame)
    }

    /// Start the next utterance. The duplicate-final guard is cleared too: a genuinely
    /// repeated sentence, spoken twice, must be translated twice.
    pub fn reset(&mut self) {
        self.reset_keeping_last_final();
        self.last_final = None;
        self.spoken_original = None;
    }

    /// Is the current sentence finished, with only its audio still draining?
    pub fn is_draining(&self) -> bool {
        self.draining
    }

    fn reset_keeping_last_final(&mut self) {
        self.draining = false;
        self.pending_user = None;
        self.pending_other = None;
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
    fn an_unresolved_utterance_says_nothing_until_it_is_closed_out() {
        // Two words in a third language. Nothing may be SPOKEN (brief §6) — but the
        // sentence is parked rather than dropped, because at this point we do not yet
        // know it is a third language; only the classifier can say so. The caller closes
        // it out when the verdict comes back Unknown, which is what `reset` models here.
        let mut u = Utterance::new("it", "es");
        assert_eq!(u.on_frame("es", audio("es", 0)), Outcome::Hold);
        assert_eq!(
            u.on_frame("es", final_msg("guten tag", "es", "buenos dias")),
            Outcome::Hold
        );
        assert_eq!(u.direction(), Direction::Unknown);
        assert!(u.has_pending_final(), "the sentence waits for a verdict");

        // Verdict: Unknown. Nothing is spoken and the slate is wiped.
        u.reset();
        assert!(!u.has_pending_final());
        assert_eq!(u.commit(Direction::UserToOther), Some(Vec::new()));
        assert_eq!(u.take_pending_final(), None);
    }

    #[test]
    fn a_final_that_arrives_before_the_direction_is_parked_not_discarded() {
        let mut u = Utterance::new("it", "es");
        assert_eq!(u.on_frame("es", audio("es", 0)), Outcome::Hold);

        let f = final_msg(
            "Vorrei andare alla stazione",
            "es",
            "Quiero ir a la estación",
        );
        // Unresolved: held, but NOT thrown away — the complete sentence is the best
        // evidence we will ever get about what was spoken.
        assert_eq!(u.on_frame("es", f.clone()), Outcome::Hold);
        assert!(u.has_pending_final());
        assert_eq!(u.original(), "");

        // A late verdict arrives. The held audio flushes and the sentence is released.
        let flushed = u.commit(Direction::UserToOther).expect("latches");
        assert_eq!(flushed, vec![audio("es", 0)]);
        assert_eq!(u.take_pending_final().as_deref(), Some(f.as_str()));
        // Taken exactly once — a second release would speak it twice.
        assert_eq!(u.take_pending_final(), None);
    }

    #[test]
    fn a_parked_final_from_the_echo_side_is_not_spoken() {
        // Italian was spoken, so the `it` session's final is the echo. Releasing it would
        // read the speaker their own words back.
        let mut u = Utterance::new("it", "es");
        let echo = final_msg("Vorrei andare", "it", "Vorrei andare");
        assert_eq!(u.on_frame("it", echo), Outcome::Hold);
        assert!(u.has_pending_final());

        u.commit(Direction::UserToOther);
        assert_eq!(
            u.take_pending_final(),
            None,
            "the echo side is never released"
        );
    }

    #[test]
    fn each_side_parks_its_own_final_so_arrival_order_cannot_lose_the_sentence() {
        // The two upstream sessions are independent sockets and nothing orders their
        // finals. Parking only the FIRST to arrive threw the sentence away every time the
        // echo won that race — a coin flip on every utterance whose direction lands late.
        let real = final_msg("Vorrei andare", "es", "Quiero ir");
        let echo = final_msg("Vorrei andare", "it", "Vorrei andare");

        for order in [
            [("it", &echo), ("es", &real)],
            [("es", &real), ("it", &echo)],
        ] {
            let mut u = Utterance::new("it", "es");
            for (lang, frame) in order {
                assert_eq!(u.on_frame(lang, frame.clone()), Outcome::Hold);
            }
            u.commit(Direction::UserToOther);
            assert_eq!(
                u.take_pending_final().as_deref(),
                Some(real.as_str()),
                "the winning side is released whichever side finalized first"
            );
            assert_eq!(u.take_pending_final(), None, "released exactly once");
        }
    }

    #[test]
    fn a_repeat_from_one_side_does_not_displace_that_sides_first_final() {
        // Providers repeat a final. The first one is the sentence; a repeat must not
        // overwrite it with a later revision that the utterance has no way to prefer.
        let mut u = Utterance::new("it", "es");
        let first = final_msg("Vorrei andare", "es", "Quiero ir");
        let repeat = final_msg("Vorrei andare alla", "es", "Quiero ir a la");
        u.on_frame("es", first.clone());
        u.on_frame("es", repeat);
        u.commit(Direction::UserToOther);
        assert_eq!(u.take_pending_final().as_deref(), Some(first.as_str()));
    }

    #[test]
    fn the_losing_sessions_trailing_final_does_not_reopen_a_spoken_sentence() {
        // Both sessions finalize the same sentence, and the loser's copy always lands
        // AFTER the winner's — by which point the utterance has reset. Left alone, that
        // tail re-latched a direction from the echo's OWN text and carried it into the
        // next sentence, which `note_original` then refuses to re-classify because it is
        // latched. One trailing frame inverted a whole conversational turn.
        let mut u = Utterance::new("it", "es");
        u.note_original("es", "Vorrei andare");
        u.commit(Direction::UserToOther);
        let real = final_msg("Vorrei andare", "es", "Quiero ir");
        assert_eq!(u.on_frame("es", real.clone()), Outcome::one(real));

        // The `it` session's copy of the SAME sentence, arriving into a freshly reset
        // utterance that has heard nothing new yet.
        assert!(u.is_trailing_echo("it", "Vorrei andare"));

        // The next sentence is not a tail: it is new evidence, even when the speaker
        // happens to repeat themselves after something else was said.
        u.note_original("es", "Dove siamo");
        assert!(!u.is_trailing_echo("it", "Vorrei andare"));
    }

    #[test]
    fn the_direction_outlives_the_final_so_trailing_audio_still_plays() {
        // The engine closes a segment after an idle GAP IN THE TRANSCRIPT, but the
        // translated speech for that segment is still streaming — text finishes long
        // before audio does. Clearing the direction at the final therefore orphaned the
        // rest of the sentence: it arrived with no direction, was held, and was dropped
        // when the next sentence resolved the other way. That is "it says half a
        // sentence".
        let mut u = Utterance::new("it", "es");
        u.note_original("es", "Vorrei andare alla stazione");
        u.commit(Direction::UserToOther);
        let f = final_msg(
            "Vorrei andare alla stazione",
            "es",
            "Quiero ir a la estación",
        );
        assert_eq!(u.on_frame("es", f.clone()), Outcome::one(f));

        // The tail of the SAME sentence, arriving after its own final.
        assert_eq!(
            u.direction(),
            Direction::UserToOther,
            "still routing this sentence"
        );
        assert_eq!(
            u.on_frame("es", audio("es", 7)),
            Outcome::one(audio("es", 7)),
            "the rest of the sentence must still be spoken"
        );
        // The echo side's tail is still suppressed — draining is not a free-for-all.
        assert_eq!(u.on_frame("it", audio("it", 7)), Outcome::Hold);
    }

    #[test]
    fn new_speech_ends_the_drain_and_starts_a_fresh_utterance() {
        // The direction may outlive the sentence, never the SPEAKER. The moment new words
        // arrive the slate is clean again, or a turn change would be routed by the
        // previous speaker's direction.
        let mut u = Utterance::new("it", "es");
        u.note_original("es", "Vorrei andare");
        u.commit(Direction::UserToOther);
        u.on_frame("es", final_msg("Vorrei andare", "es", "Quiero ir"));
        assert_eq!(u.direction(), Direction::UserToOther);

        assert!(
            u.note_original("es", "¿Dónde está la estación?"),
            "new speech asks again"
        );
        assert_eq!(
            u.direction(),
            Direction::Unknown,
            "a new sentence is not steered by the previous one"
        );
        assert_eq!(
            u.on_frame("es", audio("es", 0)),
            Outcome::Hold,
            "held again"
        );
    }

    #[test]
    fn the_tail_of_a_spoken_sentence_does_not_end_the_drain() {
        // The losing session repeats the same text after the winner's final. Treating
        // that as new speech would clear the direction and strand the audio all over
        // again — the very bug this drain exists to close.
        let mut u = Utterance::new("it", "es");
        u.note_original("es", "Vorrei andare");
        u.commit(Direction::UserToOther);
        u.on_frame("es", final_msg("Vorrei andare", "es", "Quiero ir"));

        // The tail arrives on the side we did NOT speak — that is what identifies it.
        assert!(
            !u.note_original("it", "Vorrei andare"),
            "the tail is not new evidence"
        );
        assert_eq!(u.direction(), Direction::UserToOther, "still draining");
        assert!(u.is_trailing_echo("it", "Vorrei andare"));
    }

    #[test]
    fn a_genuinely_repeated_sentence_is_not_mistaken_for_a_tail() {
        // "Sì." twice in a row is two sentences. The guard only covers a final landing
        // into an utterance that has heard nothing since the reset — an interim for the
        // new sentence is exactly that evidence.
        let mut u = Utterance::new("it", "es");
        u.note_original("es", "Sì");
        u.commit(Direction::UserToOther);
        let f = final_msg("Sì", "es", "Sí");
        assert_eq!(u.on_frame("es", f.clone()), Outcome::one(f));
        u.note_original("es", "Sì");
        assert!(!u.is_trailing_echo("it", "Sì"));
    }

    #[test]
    fn a_closed_out_utterance_forgets_the_sentence_it_spoke() {
        // `reset` is the full boundary — Stop, or a verdict that closed the sentence out.
        // Carrying the tail guard across it would swallow a legitimate repeat.
        let mut u = Utterance::new("it", "es");
        u.note_original("es", "Sì");
        u.commit(Direction::UserToOther);
        u.on_frame("es", final_msg("Sì", "es", "Sí"));
        assert!(u.is_trailing_echo("it", "Sì"));
        u.reset();
        assert!(!u.is_trailing_echo("it", "Sì"));
    }

    #[test]
    fn a_parked_final_never_leaks_into_the_next_sentence() {
        let mut u = Utterance::new("it", "es");
        u.on_frame("es", final_msg("Vorrei andare", "es", "Quiero ir"));
        assert!(u.has_pending_final());
        u.reset();
        assert!(!u.has_pending_final());
        u.commit(Direction::UserToOther);
        assert_eq!(u.take_pending_final(), None);
    }

    #[test]
    fn the_hold_buffer_covers_a_whole_sentence() {
        // The direction is now often only settled by the FINAL transcript, so the buffer
        // has to outlast a full sentence of translated audio. At the old 24 frames
        // (~2.4 s at ~100 ms each) the head of every late-resolved translation was
        // clipped away. Exercised through the real buffer rather than by comparing two
        // constants, so it fails if the retention behaviour changes for any reason.
        const SIX_SECONDS_OF_FRAMES: u64 = 60;
        let mut u = Utterance::new("it", "es");
        for seq in 0..SIX_SECONDS_OF_FRAMES {
            assert_eq!(u.on_frame("es", audio("es", seq)), Outcome::Hold);
        }
        let flushed = u.commit(Direction::UserToOther).expect("latches");
        assert_eq!(
            flushed.len(),
            SIX_SECONDS_OF_FRAMES as usize,
            "six seconds of translated audio must survive the wait for a direction"
        );
        assert_eq!(flushed[0], audio("es", 0), "the head must not be clipped");
    }

    #[test]
    fn partials_replace_rather_than_append() {
        let mut u = Utterance::new("it", "es");
        u.note_original("es", "Vorrei andare");
        u.note_original("es", "Vorrei andare alla stazione");
        assert_eq!(u.original(), "Vorrei andare alla stazione");
        // A revision that shortens the text must not leave the old tail behind.
        u.note_original("es", "Vorrei un caffè");
        assert_eq!(u.original(), "Vorrei un caffè");
    }

    #[test]
    fn reclassification_is_throttled_but_a_revision_forces_one() {
        let mut u = Utterance::new("it", "es");
        // First real text asks for a verdict.
        assert!(u.note_original("es", "Vorrei andare"));
        // A one-character growth is not worth another model call.
        assert!(!u.note_original("es", "Vorrei andare "));
        assert!(!u.note_original("es", "Vorrei andare a"));
        // Enough new evidence — ask again.
        assert!(u.note_original("es", "Vorrei andare alla stazione"));
        // A rewrite invalidates the previous conclusion regardless of length.
        assert!(u.note_original("es", "Quiero ir a la"));
    }

    #[test]
    fn a_committed_utterance_stops_paying_for_classification() {
        let mut u = Utterance::new("it", "es");
        u.note_original("es", "Vorrei andare");
        u.commit(Direction::UserToOther);
        // The text keeps flowing for the caption, but never triggers another call.
        assert!(!u.note_original("es", "Vorrei andare alla stazione centrale"));
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
