//! Telling the person on the telephone what is happening to their voice
//! (spec 0111, R19–R21, D7).
//!
//! The recipient has no VoxTranslate interface. There is no banner to show them, no
//! checkbox, no settings page — so a UI disclosure shown to *our* user discloses nothing
//! to *them*. Everything here exists because of that asymmetry.
//!
//! Three rules, and none of them is negotiable by configuration:
//!
//! 1. **If it is captured, they are told first.** The announcement is played, and its
//!    completion recorded, before any recording or transcript is persisted.
//! 2. **The announcement is true.** It states what is actually enabled on this call and
//!    nothing else. Claiming a recording that is not happening is as wrong as hiding one
//!    that is.
//! 3. **It is in their language**, from a fixed reviewed table — never generated at call
//!    time. A consent notice invented by a model on the fly is not a notice anyone can
//!    review, and this is the one string in the product a lawyer will actually read.
//!
//! Legal review is required and is **not** encoded here: jurisdictions differ on
//! one-party versus all-party consent, and this module deliberately expresses *policy
//! mechanics* rather than legal conclusions.

use std::collections::HashMap;
use std::fmt;
use std::sync::OnceLock;

/// How consent is obtained, per organization, with optional per-country override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentPolicy {
    /// Tell them; do not ask. Appropriate where notice alone is sufficient.
    NoticeOnly,
    /// Tell them and require a DTMF keypress before capture starts.
    PressKey,
    /// Tell them and ask for a spoken agreement, which the VoxTranslate user confirms in
    /// the dialer. We do not try to detect the word "yes" — a false positive here is a
    /// recording nobody agreed to.
    Verbal,
    /// No consent step. Legitimate **only** when nothing is being captured; see
    /// [`plan`] for what happens if it is configured alongside capture.
    Disabled,
}

impl ConsentPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoticeOnly => "notice_only",
            Self::PressKey => "press_key",
            Self::Verbal => "verbal",
            Self::Disabled => "disabled",
        }
    }

    /// Unknown values fall back to the **most protective** policy, not to the most
    /// convenient one. A row we cannot read must not become "capture silently".
    pub fn parse(raw: &str) -> Self {
        match raw {
            "notice_only" => Self::NoticeOnly,
            "press_key" => Self::PressKey,
            "verbal" => Self::Verbal,
            "disabled" => Self::Disabled,
            _ => Self::PressKey,
        }
    }
}

impl fmt::Display for ConsentPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where the consent conversation got to. Persisted in `voip_calls.consent_status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentStatus {
    /// Nothing is being captured, or the policy only requires notice.
    NotRequired,
    /// Asked, waiting for an answer. **Capture must not start in this state.**
    Pending,
    Granted,
    Denied,
    Timeout,
}

impl ConsentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotRequired => "not_required",
            Self::Pending => "pending",
            Self::Granted => "granted",
            Self::Denied => "denied",
            Self::Timeout => "timeout",
        }
    }

    /// Whether recording/transcription may run.
    ///
    /// `Pending` is deliberately false: the window between asking and being answered is
    /// exactly when an eager implementation would start capturing.
    pub fn permits_capture(self) -> bool {
        matches!(self, Self::NotRequired | Self::Granted)
    }
}

/// What the organization wants to capture on this call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CaptureIntent {
    pub recording: bool,
    pub transcription: bool,
    /// AI summary / sentiment. Runs over the transcript, so it cannot be on without it —
    /// [`CaptureIntent::normalised`] enforces that rather than trusting the caller.
    pub ai_analysis: bool,
}

impl CaptureIntent {
    /// Nothing is persisted about what was said.
    pub fn captures_nothing(&self) -> bool {
        !self.recording && !self.transcription
    }

    /// AI analysis without a transcript is not a coherent request — there would be
    /// nothing to analyse. Dropping it here means the announcement cannot promise
    /// analysis that will not happen.
    pub fn normalised(self) -> Self {
        Self {
            ai_analysis: self.ai_analysis && self.transcription,
            ..self
        }
    }
}

/// What happens when consent is refused or times out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusedAction {
    /// Keep talking, capture nothing. Friendlier, and legal in more places.
    ContinueUnrecorded,
    /// End the call politely. For organizations that may not hold an unrecorded call.
    End,
}

impl RefusedAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ContinueUnrecorded => "continue_unrecorded",
            Self::End => "end",
        }
    }

    pub fn parse(raw: &str) -> Self {
        match raw {
            "end" => Self::End,
            _ => Self::ContinueUnrecorded,
        }
    }
}

/// Which announcement to speak. A whole sentence per case, never assembled from
/// fragments: word order and agreement differ across the languages we speak, and a
/// disclosure stitched together from clauses is a disclosure nobody can review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnouncementBody {
    /// AI translation only. Nothing is kept.
    TranslationOnly,
    /// Translation, and the conversation is transcribed.
    Transcription,
    /// Translation, and the call is recorded.
    Recording,
    /// Translation, and the call is both recorded and transcribed.
    RecordingAndTranscription,
}

impl AnnouncementBody {
    /// Key into the disclosure copy table.
    pub fn key(self) -> &'static str {
        match self {
            Self::TranslationOnly => "translation_only",
            Self::Transcription => "transcription",
            Self::Recording => "recording",
            Self::RecordingAndTranscription => "recording_and_transcription",
        }
    }
}

/// The sentence appended when the recipient must answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnouncementGate {
    PressKey,
    Verbal,
}

impl AnnouncementGate {
    pub fn key(self) -> &'static str {
        match self {
            Self::PressKey => "gate_press_key",
            Self::Verbal => "gate_verbal",
        }
    }
}

/// What to play, and whether to wait for an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disclosure {
    /// `None` when there is genuinely nothing to disclose. Playing an announcement that
    /// claims nothing wastes the first seconds of the call and trains people to ignore it.
    pub body: Option<AnnouncementBody>,
    pub gate: Option<AnnouncementGate>,
    /// Language the announcement is spoken in — the recipient's, not the caller's.
    pub language: String,
    /// Status to write the moment the plan is made.
    pub initial_status: ConsentStatus,
    /// Whether capture may begin as soon as the announcement finishes.
    pub capture_after_announcement: bool,
    /// Set when the configuration was incoherent and was corrected. Surfaced to the
    /// operator and written to `audit_logs`, because an admin's setting was overridden
    /// and silently doing that is its own kind of wrong.
    pub policy_escalated: Option<&'static str>,
}

/// Decide what the recipient hears and whether we wait for them.
///
/// `ai_disclosure` is the EU AI Act–oriented switch: the recipient is hearing a
/// synthesized voice, and where that must be disclosed it is disclosed even when nothing
/// is being captured.
pub fn plan(
    policy: ConsentPolicy,
    intent: CaptureIntent,
    ai_disclosure: bool,
    recipient_language: &str,
) -> Disclosure {
    let intent = intent.normalised();
    let language = recipient_language.trim().to_lowercase();
    let mut escalated = None;

    // Capture with the consent step switched off is not a configuration we honour. The
    // obligation to inform does not have an off switch, so the policy is raised to notice
    // and the override is recorded. Refusing the call outright would punish the customer
    // for an admin's mistake; capturing silently would punish the recipient.
    let policy = if policy == ConsentPolicy::Disabled && !intent.captures_nothing() {
        escalated = Some(
            "consent policy was 'disabled' while recording or transcription was enabled; \
             raised to 'notice_only' — capture is never silent",
        );
        ConsentPolicy::NoticeOnly
    } else {
        policy
    };

    let body = match (intent.recording, intent.transcription) {
        (true, true) => Some(AnnouncementBody::RecordingAndTranscription),
        (true, false) => Some(AnnouncementBody::Recording),
        (false, true) => Some(AnnouncementBody::Transcription),
        // Nothing kept: speak only if AI disclosure is required, and then only about the
        // synthesized voice — never about a recording that is not happening.
        (false, false) => ai_disclosure.then_some(AnnouncementBody::TranslationOnly),
    };

    // A gate only makes sense when something is being captured. Asking someone to press 1
    // to allow a recording that is not happening is theatre.
    let gate = if intent.captures_nothing() {
        None
    } else {
        match policy {
            ConsentPolicy::PressKey => Some(AnnouncementGate::PressKey),
            ConsentPolicy::Verbal => Some(AnnouncementGate::Verbal),
            ConsentPolicy::NoticeOnly | ConsentPolicy::Disabled => None,
        }
    };

    let initial_status = if gate.is_some() {
        ConsentStatus::Pending
    } else {
        ConsentStatus::NotRequired
    };

    Disclosure {
        body,
        gate,
        language,
        initial_status,
        capture_after_announcement: !intent.captures_nothing() && gate.is_none(),
        policy_escalated: escalated,
    }
}

// ---------------------------------------------------------------------------
// The spoken copy
// ---------------------------------------------------------------------------

/// The reviewed disclosure sentences, one set per language.
///
/// Compiled in rather than fetched or generated, for the reason in the module docs: this
/// is the one string in the product a lawyer will actually read, and a notice invented at
/// call time is not a notice anyone can review. It covers **every** language the product
/// speaks — an English announcement to someone who does not speak English discloses
/// nothing, which is worse than the notification fallback the rest of the server uses.
static DISCLOSURE_JSON: &str = include_str!("../../assets/voip-disclosure.json");

fn table() -> &'static HashMap<String, HashMap<String, String>> {
    static TABLE: OnceLock<HashMap<String, HashMap<String, String>>> = OnceLock::new();
    TABLE.get_or_init(|| {
        // A panic, deliberately, and not a silent empty table. The alternative —
        // `unwrap_or_default()` — would start every call with no notice to read and no
        // error anywhere, which is the compliance failure this whole module exists to
        // prevent. Loud beats quiet when the quiet answer is "record them anyway".
        //
        // It is also unreachable in a shipped binary twice over: the file is
        // `include_str!`-ed, so it is fixed at build time, and `every_language_carries_
        // every_sentence` plus `the_table_speaks_every_language_the_product_does` parse
        // it on every CI run. `lib.rs` calls `languages()` while mounting the VoIP routes
        // so that if it ever did fail, it fails during boot rather than during a call.
        serde_json::from_str(DISCLOSURE_JSON).expect("voip-disclosure.json is valid and complete")
    })
}

/// Every language the announcement exists in.
pub fn languages() -> Vec<&'static str> {
    let mut v: Vec<&str> = table().keys().map(String::as_str).collect();
    v.sort_unstable();
    v
}

/// Whether we can speak to someone in this language.
pub fn speaks(language: &str) -> bool {
    table().contains_key(&base_language(language))
}

/// `pt-BR` → `pt`. The table is keyed by base language, matching the rest of the product.
fn base_language(language: &str) -> String {
    language
        .trim()
        .to_lowercase()
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .to_string()
}

fn lookup(language: &str, key: &str) -> Option<&'static str> {
    table()
        .get(&base_language(language))
        .and_then(|m| m.get(key))
        .map(String::as_str)
}

/// What a caller hears before the tone, in their own language (spec 0118).
///
/// The same table, the same fallback and the same `fell_back` signal as
/// [`announcement`] — because it is the same person, on the same call, a minute later.
/// Without this a Mandarin-speaking caller heard the consent announcement in Mandarin and
/// was then told in English to leave a message: exactly the split the i18n rule exists to
/// prevent, in an app whose whole promise is that it speaks their language.
///
/// The language is known: `identify_caller` resolved it at admission and wrote it to
/// `voip_calls.target_language`. A missed call is one nobody PICKED UP, which is a
/// different thing from one where nobody was identified.
pub fn voicemail_prompt(language: &str) -> (String, bool) {
    let lang = base_language(language);
    let fell_back = !speaks(&lang);
    let text = lookup(&lang, "voicemail")
        .or_else(|| lookup("en", "voicemail"))
        .unwrap_or_default()
        .to_string();
    (text, fell_back)
}

/// The exact words the recipient hears, assembled from whole reviewed sentences.
///
/// Returns `None` when there is nothing to say — which is a real outcome, not an error:
/// an announcement that claims nothing wastes the first seconds of the call and teaches
/// people to ignore the one that matters.
///
/// An unknown language falls back to English **and says so** in the returned
/// [`Announcement::fell_back`], because "we spoke English at someone who does not read it"
/// is a compliance fact, not a cosmetic one. It must reach the audit log.
pub fn announcement(disclosure: &Disclosure, refused: RefusedAction) -> Option<Announcement> {
    let body_key = disclosure.body?.key();
    let lang = base_language(&disclosure.language);
    let fell_back = !speaks(&lang);
    let pick = |key: &str| -> String {
        lookup(&lang, key)
            .or_else(|| lookup("en", key))
            .unwrap_or_default()
            .to_string()
    };

    let mut text = pick(body_key);
    if let Some(gate) = disclosure.gate {
        text.push(' ');
        text.push_str(&pick(gate.key()));
        text.push(' ');
        text.push_str(&pick(match refused {
            RefusedAction::ContinueUnrecorded => "refuse_continue",
            RefusedAction::End => "refuse_end",
        }));
    }

    Some(Announcement {
        language: if fell_back { "en".to_string() } else { lang },
        text: text.trim().to_string(),
        fell_back,
    })
}

/// What to speak, and in which language it actually ended up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announcement {
    /// The language actually spoken — English when the requested one is missing.
    pub language: String,
    pub text: String,
    /// True when the requested language had no copy. A compliance fact: the recipient was
    /// addressed in a language they may not read. Belongs in the audit log.
    pub fell_back: bool,
}

/// Interpret a DTMF digit against the gate.
///
/// Only an explicit accepting digit grants. Anything else denies — a wrong key is not an
/// ambiguous signal to be resolved in our favour.
pub fn on_digit(digit: char, accept_digits: &str) -> ConsentStatus {
    if accept_digits.contains(digit) {
        ConsentStatus::Granted
    } else {
        ConsentStatus::Denied
    }
}

/// What to do once consent has resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterConsent {
    /// Start recording/transcription as configured.
    StartCapture,
    /// Keep the call, capture nothing.
    ContinueWithoutCapture,
    /// End the call politely.
    EndCall,
}

pub fn after(status: ConsentStatus, refused: RefusedAction) -> AfterConsent {
    match status {
        ConsentStatus::Granted | ConsentStatus::NotRequired => AfterConsent::StartCapture,
        // Still waiting is not permission. Treated as "not yet", never as "go ahead".
        ConsentStatus::Pending => AfterConsent::ContinueWithoutCapture,
        ConsentStatus::Denied | ConsentStatus::Timeout => match refused {
            RefusedAction::ContinueUnrecorded => AfterConsent::ContinueWithoutCapture,
            RefusedAction::End => AfterConsent::EndCall,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(recording: bool, transcription: bool) -> CaptureIntent {
        CaptureIntent {
            recording,
            transcription,
            ai_analysis: false,
        }
    }

    // ---- the announcement says what is true, and only that (R21) --------------

    #[test]
    fn the_announcement_names_exactly_what_is_enabled() {
        let cases = [
            (false, false, None),
            (false, true, Some(AnnouncementBody::Transcription)),
            (true, false, Some(AnnouncementBody::Recording)),
            (
                true,
                true,
                Some(AnnouncementBody::RecordingAndTranscription),
            ),
        ];
        for (rec, tra, expected) in cases {
            let d = plan(ConsentPolicy::NoticeOnly, intent(rec, tra), false, "zh");
            assert_eq!(d.body, expected, "recording={rec} transcription={tra}");
        }
    }

    #[test]
    fn nothing_captured_and_no_ai_disclosure_means_no_announcement_at_all() {
        // Playing a notice that claims nothing costs the first seconds of the call and
        // teaches people to ignore the one that matters.
        let d = plan(ConsentPolicy::NoticeOnly, intent(false, false), false, "en");
        assert_eq!(d.body, None);
        assert_eq!(d.gate, None);
        assert_eq!(d.initial_status, ConsentStatus::NotRequired);
    }

    #[test]
    fn ai_disclosure_alone_speaks_only_about_the_synthesized_voice() {
        // It must NOT claim a recording. That is the falsehood R21 exists to prevent.
        let d = plan(ConsentPolicy::NoticeOnly, intent(false, false), true, "de");
        assert_eq!(d.body, Some(AnnouncementBody::TranslationOnly));
        assert_eq!(d.gate, None);
    }

    #[test]
    fn the_announcement_is_in_the_recipients_language_normalised() {
        let d = plan(
            ConsentPolicy::NoticeOnly,
            intent(true, true),
            false,
            "  ZH  ",
        );
        assert_eq!(d.language, "zh");
    }

    // ---- the gate -------------------------------------------------------------

    #[test]
    fn press_key_waits_and_does_not_capture_while_waiting() {
        let d = plan(ConsentPolicy::PressKey, intent(true, true), false, "en");
        assert_eq!(d.gate, Some(AnnouncementGate::PressKey));
        assert_eq!(d.initial_status, ConsentStatus::Pending);
        assert!(!d.capture_after_announcement);
        assert!(
            !d.initial_status.permits_capture(),
            "the window between asking and being answered is exactly when an eager \
             implementation starts recording"
        );
    }

    #[test]
    fn notice_only_informs_and_proceeds() {
        let d = plan(ConsentPolicy::NoticeOnly, intent(true, false), false, "en");
        assert_eq!(d.gate, None);
        assert_eq!(d.initial_status, ConsentStatus::NotRequired);
        assert!(d.capture_after_announcement);
    }

    #[test]
    fn verbal_consent_waits_for_a_human_decision_not_for_a_keyword() {
        // We do not try to detect "yes". A false positive is a recording nobody agreed to.
        let d = plan(ConsentPolicy::Verbal, intent(true, true), false, "fr");
        assert_eq!(d.gate, Some(AnnouncementGate::Verbal));
        assert_eq!(d.initial_status, ConsentStatus::Pending);
        assert!(!d.capture_after_announcement);
    }

    #[test]
    fn no_gate_is_offered_when_there_is_nothing_to_consent_to() {
        // "Press 1 to allow the recording" when nothing is recorded is theatre.
        for p in [ConsentPolicy::PressKey, ConsentPolicy::Verbal] {
            let d = plan(p, intent(false, false), true, "en");
            assert_eq!(d.gate, None, "{p}");
            assert_eq!(d.initial_status, ConsentStatus::NotRequired);
        }
    }

    // ---- the misconfiguration that must never mean "capture silently" ---------

    #[test]
    fn disabling_consent_while_capturing_is_raised_to_notice_and_recorded() {
        let d = plan(ConsentPolicy::Disabled, intent(true, true), false, "en");
        assert_eq!(
            d.body,
            Some(AnnouncementBody::RecordingAndTranscription),
            "capture is never silent, whatever the setting says"
        );
        assert!(d.policy_escalated.is_some(), "and the override is surfaced");
        assert!(d.capture_after_announcement);
    }

    #[test]
    fn disabling_consent_with_nothing_captured_is_perfectly_valid() {
        let d = plan(ConsentPolicy::Disabled, intent(false, false), false, "en");
        assert_eq!(d.body, None);
        assert!(d.policy_escalated.is_none(), "nothing was overridden");
    }

    #[test]
    fn an_unreadable_policy_falls_back_to_the_most_protective_one() {
        // Not to the most convenient one. A row we cannot parse must not become
        // "capture silently".
        assert_eq!(ConsentPolicy::parse("press_key"), ConsentPolicy::PressKey);
        assert_eq!(
            ConsentPolicy::parse("notice_only"),
            ConsentPolicy::NoticeOnly
        );
        assert_eq!(ConsentPolicy::parse("disabled"), ConsentPolicy::Disabled);
        assert_eq!(ConsentPolicy::parse(""), ConsentPolicy::PressKey);
        assert_eq!(ConsentPolicy::parse("off"), ConsentPolicy::PressKey);
        assert_eq!(ConsentPolicy::parse("NOTICE_ONLY"), ConsentPolicy::PressKey);
    }

    // ---- AI analysis coherence ------------------------------------------------

    #[test]
    fn ai_analysis_without_a_transcript_is_dropped_not_promised() {
        // There would be nothing to analyse, and the announcement must not offer it.
        let i = CaptureIntent {
            recording: true,
            transcription: false,
            ai_analysis: true,
        }
        .normalised();
        assert!(!i.ai_analysis);

        let kept = CaptureIntent {
            recording: false,
            transcription: true,
            ai_analysis: true,
        }
        .normalised();
        assert!(kept.ai_analysis);
    }

    // ---- digits ---------------------------------------------------------------

    #[test]
    fn only_an_explicit_accepting_digit_grants_consent() {
        assert_eq!(on_digit('1', "1"), ConsentStatus::Granted);
        assert_eq!(on_digit('1', "12"), ConsentStatus::Granted);
        // A wrong key is a refusal, not an ambiguity to resolve in our favour.
        assert_eq!(on_digit('2', "1"), ConsentStatus::Denied);
        assert_eq!(on_digit('#', "1"), ConsentStatus::Denied);
        assert_eq!(on_digit('0', "1"), ConsentStatus::Denied);
    }

    // ---- what happens next ----------------------------------------------------

    #[test]
    fn a_refusal_respects_the_organizations_policy() {
        for status in [ConsentStatus::Denied, ConsentStatus::Timeout] {
            assert_eq!(
                after(status, RefusedAction::ContinueUnrecorded),
                AfterConsent::ContinueWithoutCapture,
                "{status:?}"
            );
            assert_eq!(
                after(status, RefusedAction::End),
                AfterConsent::EndCall,
                "{status:?}"
            );
        }
    }

    #[test]
    fn silence_is_not_consent() {
        // A timeout must never be read as agreement, under either policy.
        assert_ne!(
            after(ConsentStatus::Timeout, RefusedAction::ContinueUnrecorded),
            AfterConsent::StartCapture
        );
        assert_ne!(
            after(ConsentStatus::Timeout, RefusedAction::End),
            AfterConsent::StartCapture
        );
    }

    #[test]
    fn still_waiting_is_not_permission_either() {
        assert_eq!(
            after(ConsentStatus::Pending, RefusedAction::ContinueUnrecorded),
            AfterConsent::ContinueWithoutCapture
        );
        assert!(!ConsentStatus::Pending.permits_capture());
    }

    #[test]
    fn granted_and_not_required_both_allow_capture() {
        assert_eq!(
            after(ConsentStatus::Granted, RefusedAction::End),
            AfterConsent::StartCapture
        );
        assert_eq!(
            after(ConsentStatus::NotRequired, RefusedAction::End),
            AfterConsent::StartCapture
        );
        assert!(ConsentStatus::Granted.permits_capture());
        assert!(ConsentStatus::NotRequired.permits_capture());
    }

    // ---- persisted vocabulary -------------------------------------------------

    #[test]
    fn persisted_values_are_unique_and_match_the_schema_check_constraints() {
        // These strings are CHECK constraints in migration 056. A mismatch is a runtime
        // insert failure on a live call, not a compile error.
        let policies = [
            ConsentPolicy::NoticeOnly,
            ConsentPolicy::PressKey,
            ConsentPolicy::Verbal,
            ConsentPolicy::Disabled,
        ];
        let names: Vec<&str> = policies.iter().map(|p| p.as_str()).collect();
        assert_eq!(names, ["notice_only", "press_key", "verbal", "disabled"]);
        for p in policies {
            assert_eq!(ConsentPolicy::parse(p.as_str()), p, "{p} must round-trip");
        }

        let statuses = [
            ConsentStatus::NotRequired,
            ConsentStatus::Pending,
            ConsentStatus::Granted,
            ConsentStatus::Denied,
            ConsentStatus::Timeout,
        ];
        let names: Vec<&str> = statuses.iter().map(|s| s.as_str()).collect();
        assert_eq!(
            names,
            ["not_required", "pending", "granted", "denied", "timeout"]
        );

        assert_eq!(
            RefusedAction::ContinueUnrecorded.as_str(),
            "continue_unrecorded"
        );
        assert_eq!(RefusedAction::End.as_str(), "end");
        assert_eq!(RefusedAction::parse("end"), RefusedAction::End);
        assert_eq!(
            RefusedAction::parse("anything else"),
            RefusedAction::ContinueUnrecorded
        );
    }

    // ---- the spoken copy ------------------------------------------------------

    #[test]
    fn the_announcement_exists_in_every_language_the_product_speaks() {
        // An English notice to someone who does not read English discloses nothing, which
        // is why this table does NOT use the English fallback the rest of the server's
        // copy does. 84 languages, the same set the product ships.
        let langs = languages();
        assert_eq!(langs.len(), 84, "got {}", langs.len());
        for expected in ["en", "it", "zh", "yue", "ar", "hi", "sw", "cy", "ckb", "my"] {
            assert!(speaks(expected), "missing {expected}");
        }
    }

    #[test]
    fn every_language_carries_every_sentence() {
        // A half-translated table is worse than none: the announcement would be assembled
        // out of two languages mid-sentence.
        let required = [
            "translation_only",
            "transcription",
            "recording",
            "recording_and_transcription",
            "gate_press_key",
            "gate_verbal",
            "refuse_continue",
            "refuse_end",
        ];
        for lang in languages() {
            for key in required {
                let text = lookup(lang, key).unwrap_or("");
                assert!(!text.trim().is_empty(), "{lang} is missing {key}");
            }
        }
    }

    #[test]
    fn the_recipient_hears_their_own_language() {
        let d = plan(ConsentPolicy::NoticeOnly, intent(true, true), false, "zh");
        let a = announcement(&d, RefusedAction::ContinueUnrecorded).unwrap();
        assert_eq!(a.language, "zh");
        assert!(!a.fell_back);
        assert!(a.text.contains("录音"), "{}", a.text);
    }

    #[test]
    fn a_regional_tag_resolves_to_its_base_language() {
        // `pt-BR` must not fall back to English just because the tag carries a region.
        for tag in ["pt-BR", "pt_PT", "PT", " pt "] {
            let d = plan(ConsentPolicy::NoticeOnly, intent(true, false), false, tag);
            let a = announcement(&d, RefusedAction::ContinueUnrecorded).unwrap();
            assert_eq!(a.language, "pt", "tag {tag}");
            assert!(!a.fell_back, "tag {tag}");
        }
    }

    #[test]
    fn a_language_we_cannot_speak_falls_back_to_english_and_says_so() {
        // The call must not be silent — but "we addressed them in a language they may not
        // read" is a compliance fact, not a cosmetic one, and it has to reach the audit log.
        let d = plan(ConsentPolicy::NoticeOnly, intent(true, true), false, "xx");
        let a = announcement(&d, RefusedAction::ContinueUnrecorded).unwrap();
        assert_eq!(a.language, "en");
        assert!(a.fell_back, "the fallback must be reported, not silent");
        assert!(a.text.contains("recorded"));
    }

    #[test]
    fn the_announcement_names_exactly_what_will_happen() {
        let english = |rec: bool, tra: bool| {
            let d = plan(ConsentPolicy::NoticeOnly, intent(rec, tra), true, "en");
            announcement(&d, RefusedAction::ContinueUnrecorded).map(|a| a.text)
        };
        assert!(english(true, true)
            .unwrap()
            .contains("recorded and transcribed"));
        let rec = english(true, false).unwrap();
        assert!(rec.contains("recorded") && !rec.contains("transcribed"));
        let tra = english(false, true).unwrap();
        assert!(tra.contains("transcribed") && !tra.contains("recorded"));
        // Nothing kept: it talks about the synthesized voice and claims no recording.
        let none = english(false, false).unwrap();
        assert!(!none.contains("recorded") && !none.contains("transcribed"));
        assert!(none.contains("generated"));
    }

    #[test]
    fn nothing_is_spoken_when_there_is_nothing_to_disclose() {
        let d = plan(ConsentPolicy::NoticeOnly, intent(false, false), false, "en");
        assert_eq!(announcement(&d, RefusedAction::ContinueUnrecorded), None);
    }

    #[test]
    fn the_gate_tells_them_what_happens_if_they_decline() {
        // "Press 1 to agree" without saying what happens otherwise is a dark pattern.
        let d = plan(ConsentPolicy::PressKey, intent(true, false), false, "en");

        let cont = announcement(&d, RefusedAction::ContinueUnrecorded).unwrap();
        assert!(cont.text.contains("press 1"), "{}", cont.text);
        assert!(cont.text.contains("continues without it"), "{}", cont.text);

        let end = announcement(&d, RefusedAction::End).unwrap();
        assert!(end.text.contains("the call will end"), "{}", end.text);
        assert!(!end.text.contains("continues without it"));
    }

    #[test]
    fn a_notice_only_announcement_asks_for_nothing() {
        let d = plan(ConsentPolicy::NoticeOnly, intent(true, false), false, "en");
        let a = announcement(&d, RefusedAction::End).unwrap();
        assert!(!a.text.contains("press 1"), "{}", a.text);
        assert!(!a.text.contains("will end"), "{}", a.text);
    }

    #[test]
    fn the_verbal_gate_asks_out_loud_rather_than_for_a_keypress() {
        let d = plan(ConsentPolicy::Verbal, intent(false, true), false, "en");
        let a = announcement(&d, RefusedAction::ContinueUnrecorded).unwrap();
        assert!(a.text.contains("say yes"), "{}", a.text);
        assert!(!a.text.contains("press 1"), "{}", a.text);
    }

    #[test]
    fn no_announcement_is_absurdly_long_to_sit_through() {
        // It plays at the start of a call, before the conversation. Every extra second is
        // a second the recipient spends wondering what is happening.
        for lang in languages() {
            let d = plan(ConsentPolicy::PressKey, intent(true, true), false, lang);
            let a = announcement(&d, RefusedAction::End).unwrap();
            let words = a.text.split_whitespace().count();
            assert!(
                a.text.chars().count() < 400,
                "{lang} announcement is {} chars: {}",
                a.text.chars().count(),
                a.text
            );
            let _ = words;
        }
    }

    #[test]
    fn copy_keys_are_distinct_so_a_lookup_cannot_collide() {
        let keys = [
            AnnouncementBody::TranslationOnly.key(),
            AnnouncementBody::Transcription.key(),
            AnnouncementBody::Recording.key(),
            AnnouncementBody::RecordingAndTranscription.key(),
            AnnouncementGate::PressKey.key(),
            AnnouncementGate::Verbal.key(),
        ];
        let unique: std::collections::HashSet<&str> = keys.iter().copied().collect();
        assert_eq!(unique.len(), keys.len());
    }
}

#[cfg(test)]
mod voicemail_tests {
    use super::*;

    #[test]
    fn a_caller_hears_the_message_prompt_in_the_language_they_were_answered_in() {
        // The split this exists to close: the consent announcement in Mandarin, then
        // "leave a message" in English, to the same person on the same call.
        let (zh, fell_back) = voicemail_prompt("zh");
        assert!(!fell_back);
        assert!(zh.contains('留'), "not Chinese: {zh}");

        let (it, fell_back) = voicemail_prompt("it");
        assert!(!fell_back);
        assert!(it.to_lowercase().contains("messaggio"), "not Italian: {it}");
    }

    #[test]
    fn a_regional_tag_resolves_to_its_base_language() {
        let (a, _) = voicemail_prompt("pt-BR");
        let (b, _) = voicemail_prompt("pt");
        assert_eq!(a, b);
    }

    #[test]
    fn a_language_the_table_does_not_carry_falls_back_and_says_so() {
        // `fell_back` is a compliance fact, not a cosmetic one — the same reasoning
        // `announcement` states for itself.
        let (text, fell_back) = voicemail_prompt("kl");
        assert!(fell_back, "a fallback that does not admit to being one");
        assert_eq!(text, voicemail_prompt("en").0);
        assert!(!text.is_empty());
    }

    #[test]
    fn every_language_the_disclosure_speaks_can_also_take_a_message() {
        // The all-or-nothing rule, asserted rather than trusted: a language that can be
        // told it is being recorded must also be able to be asked for a message.
        for lang in table().keys() {
            let (text, fell_back) = voicemail_prompt(lang);
            assert!(!fell_back, "{lang} fell back");
            assert!(!text.trim().is_empty(), "{lang} is blank");
        }
    }
}
