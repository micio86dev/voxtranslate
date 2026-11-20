//! Echo filter for Live Connect's outbound path (spec 0121).
//!
//! # The problem
//!
//! Two phones side by side. The owner speaks Italian, the guest Spanish. The owner's voice also
//! reaches the *guest's* microphone, so the guest's seat opens an engine session (es -> it) that
//! "translates" Italian speech it takes for Spanish and delivers the result, captions and
//! `translated_audio`, to the *owner*, who hears their own sentence read back to them.
//!
//! # What this module is
//!
//! A pure state machine, one per outbound seat, fed every frame destined to that phone, which
//! returns the frames to actually send. No I/O, no clock of its own (the caller passes `now`),
//! no allocation on the hot path while nobody spoke recently. It is written for N seats (one
//! host and up to three guests), although a conversation has two today.
//!
//! * It keeps a ledger of the recent ORIGINAL words of every speaker this seat has seen, mine
//!   included, each entry tagged with its speaker (my own captions come back to my phone:
//!   `subtitle_interim` with `speaker_id == me`; another speaker's words are recorded once their
//!   utterance has been judged real).
//! * A frame from another speaker X starts an utterance, tracked per speaker (at most
//!   [`MAX_SPEAKERS`]). When somebody else spoke lately, or my own microphone did, the utterance is
//!   held (at most [`HOLD_MAX`]) until its first `original` text is known, then it is either
//!   released in order (`Real`) or dropped entirely (`Echo`). Timeout fails OPEN.
//! * It is an echo when its `original` is similar to a recent entry whose speaker is NOT X: my
//!   words, or a third speaker's (B's microphone heard A, and B's session "translates" A for the
//!   host). An utterance is never compared with its own speaker's entries.
//! * When nobody else has spoken for [`OWN_RECENT`], nothing is ever held: zero added latency.
//!
//! # Why the comparison is on `original`, never on `text`
//!
//! For a listener of another language, `text` is the TRANSLATION and `original` is what the
//! speaker's ASR heard. A genuine Spanish reply "hola, estoy bien, gracias" reaches the Italian
//! phone as `text = "ciao, sto bene, grazie"`. That translation can share words with what the
//! owner just said ("ciao come stai"); the Spanish original shares none. An echo is the opposite:
//! the owner's Italian run through a Spanish ear ("chao como estai") still *sounds like* what
//! the owner's own captions say. So only `original` is compared, and when it is absent on a
//! cross-language frame the utterance stays undecided (and fails open) rather than guessing from
//! `text`.
//!
//! # The symmetry trap, and why there is a loudness rule
//!
//! Two phones hear the same sentence, so each seat sees a "similar pair": the owner's seat sees
//! `own caption ~ the guest's original` (the guest's session is the echo), while the guest's seat
//! sees the same thing with the roles swapped, because the guest's own "caption" is the engine's
//! reading of the owner's voice. Judged on text alone BOTH filters would drop the other's frames
//! and the conversation would lose the real translation too. The tie is broken by the one
//! physical fact available: the real speaker's microphone is clearly louder than the other's. An
//! utterance of X is only declared `Echo` of Y's words when Y's smoothed mic level beats X's by
//! [`DOMINANCE_NUM`] / [`DOMINANCE_DEN`]; equal or unknown levels never suppress. For any pair at
//! most one can be the louder, so two filters cannot both fire on the same sentence.
//!
//! # What is NOT verified
//!
//! Everything here is verified as logic (unit tests), as a state machine and through the real
//! socket and mock engine. What the real ASR returns for cross-language echo, how loud a real
//! second phone's microphone is, and whether phone gain control flattens the two levels are NOT
//! verified: only a real device test can. See `docs/live-connect.md` for how to tune and what to
//! check first.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use serde::Deserialize;
use tokio::time::Instant;

/// Longest a new, undecided partner utterance is held while its first `original` is awaited.
pub const HOLD_MAX: Duration = Duration::from_millis(700);
/// The seat counts as "having spoken recently" this long after its last words / voiced audio.
pub const OWN_RECENT: Duration = Duration::from_secs(12);
/// Own ledger entries older than this are forgotten.
const LEDGER_KEEP: Duration = Duration::from_secs(15);
/// At most this many entries are remembered per speaker, and this many in all.
const LEDGER_MAX: usize = 8;
const LEDGER_TOTAL_MAX: usize = 64;
/// Speakers whose utterance state is tracked at once (a host and three guests, with room).
pub const MAX_SPEAKERS: usize = 8;
/// Silence from the speaker after which their next frame starts a new utterance.
const UTTERANCE_GAP: Duration = Duration::from_millis(1500);
/// Translated audio may trail its `subtitle_final`; for this long after an utterance ended, its
/// audio frames still belong to it (so an echo's audio tail is dropped, not held and leaked).
const AUDIO_TAIL: Duration = Duration::from_secs(2);
/// Frames held for one undecided utterance; beyond this the utterance fails open.
pub const MAX_HELD: usize = 64;
/// The source's mic level must exceed the echoing speaker's by this ratio (5/4) for a match to
/// count as an echo.
const DOMINANCE_NUM: u64 = 5;
const DOMINANCE_DEN: u64 = 4;

/// Default similarity threshold.
pub const DEFAULT_SIM: f64 = 0.6;
/// The most characters of a caption that are ever compared (its end is kept).
const NORMALIZE_MAX: usize = 400;
/// A single token this long is a spaceless-script sentence, not a lone word.
const ENOUGH_ONE_TOKEN: usize = 12;

const INTERIM_PREFIX: &str = r#"{"type":"subtitle_interim""#;
const FINAL_PREFIX: &str = r#"{"type":"subtitle_final""#;
const AUDIO_PREFIX: &str = r#"{"type":"translated_audio""#;

#[derive(Debug, Clone, Copy)]
pub struct EchoConfig {
    pub enabled: bool,
    pub sim: f64,
}

impl Default for EchoConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sim: DEFAULT_SIM,
        }
    }
}

// ---- text similarity ---------------------------------------------------------------------

/// Lowercase, strip diacritics and punctuation, collapse whitespace.
pub fn normalize(s: &str) -> String {
    // Only the tail matters (what was said last is what is being echoed now) and similarity is
    // quadratic on an unbroken token, so bound the work: keep at most the last NORMALIZE_MAX chars.
    let skip = s.chars().count().saturating_sub(NORMALIZE_MAX);
    let s: String = s.chars().skip(skip).collect();
    let s = s.as_str();
    let mut out = String::with_capacity(s.len());
    let mut gap = true;
    for c in s.chars().flat_map(char::to_lowercase) {
        let c = fold(c);
        if c.is_alphanumeric() {
            out.push(c);
            gap = false;
        } else if !gap {
            out.push(' ');
            gap = true;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

fn fold(c: char) -> char {
    match c {
        'à' | 'á' | 'â' | 'ä' | 'ã' | 'å' | 'ā' => 'a',
        'è' | 'é' | 'ê' | 'ë' | 'ē' => 'e',
        'ì' | 'í' | 'î' | 'ï' | 'ī' => 'i',
        'ò' | 'ó' | 'ô' | 'ö' | 'õ' | 'ō' => 'o',
        'ù' | 'ú' | 'û' | 'ü' | 'ū' => 'u',
        'ñ' => 'n',
        'ç' => 'c',
        other => other,
    }
}

/// Enough text to judge: two words, or one very long token (scripts written without spaces give
/// a whole sentence as one token). A lone word, however long in Latin script ("gracias"), is never
/// judged: a real one-word reply must not be taken for an echo.
fn enough(norm: &str) -> bool {
    norm.split_whitespace().nth(1).is_some() || norm.chars().count() >= ENOUGH_ONE_TOKEN
}

fn levenshtein(a: &[char], b: &[char]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Two words are "the same word heard differently": identical, or (from 3 letters up) within a
/// quarter of the longer one in edits. "ciao"/"chao", "come"/"como", "stai"/"estai" match;
/// short function words must match exactly.
fn same_word(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let longest = a.len().max(b.len());
    if a.len().min(b.len()) < 3 || a.len().abs_diff(b.len()) > longest / 4 {
        return false;
    }
    levenshtein(&a, &b) <= longest / 4
}

/// Share of `cand`'s words found (each of `other`'s used once) in `other`. Containment rather
/// than Jaccard on purpose: the echo is often a PART of a longer sentence of mine, and the other
/// direction (my short phrase inside their long reply) must not count.
fn containment(cand: &str, other: &str) -> f64 {
    let words: Vec<&str> = other.split_whitespace().collect();
    let mut used = vec![false; words.len()];
    let (mut hit, mut total) = (0usize, 0usize);
    for w in cand.split_whitespace() {
        total += 1;
        if let Some(i) = (0..words.len()).find(|&i| !used[i] && same_word(w, words[i])) {
            used[i] = true;
            hit += 1;
        }
    }
    if total == 0 {
        0.0
    } else {
        hit as f64 / total as f64
    }
}

fn trigrams(s: &str) -> Vec<[char; 3]> {
    let chars: Vec<char> = s.chars().collect();
    let mut v: Vec<[char; 3]> = chars.windows(3).map(|w| [w[0], w[1], w[2]]).collect();
    v.sort_unstable();
    v
}

/// Sorensen-Dice over character trigrams (multiset), 0 when either side is too short.
fn dice(a: &str, b: &str) -> f64 {
    let (ta, tb) = (trigrams(a), trigrams(b));
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let (mut i, mut j, mut common) = (0, 0, 0usize);
    while i < ta.len() && j < tb.len() {
        match ta[i].cmp(&tb[j]) {
            std::cmp::Ordering::Equal => {
                common += 1;
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    2.0 * common as f64 / (ta.len() + tb.len()) as f64
}

/// Similarity of two NORMALISED strings: the better of word containment and trigram Dice.
pub fn similarity(cand: &str, other: &str) -> f64 {
    containment(cand, other).max(dice(cand, other))
}

// ---- frames ------------------------------------------------------------------------------

#[derive(Deserialize)]
struct Subtitle {
    speaker_id: String,
    #[serde(default)]
    lang: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    original: Option<String>,
}

/// The `speaker_id` of a `translated_audio` frame without parsing it. The tag and the id come
/// first in the serialised struct and the base64 payload is last, so only the head is scanned.
fn audio_speaker(frame: &str) -> Option<&str> {
    const KEY: &[u8] = br#""speaker_id":""#;
    let bytes = frame.as_bytes();
    let head = &bytes[..bytes.len().min(160)];
    let start = head.windows(KEY.len()).position(|w| w == KEY)? + KEY.len();
    let end = start + bytes.get(start..)?.iter().position(|&b| b == b'"')?;
    // Both ends sit next to ASCII quotes, so they are char boundaries.
    frame.get(start..end)
}

enum Frame {
    Other,
    Audio {
        speaker: String,
    },
    Subtitle {
        speaker: String,
        lang: String,
        is_final: bool,
        text: Option<String>,
        original: Option<String>,
    },
}

fn parse(frame: &str) -> Frame {
    if frame.starts_with(AUDIO_PREFIX) {
        return audio_speaker(frame).map_or(Frame::Other, |s| Frame::Audio {
            speaker: s.to_string(),
        });
    }
    let is_final = frame.starts_with(FINAL_PREFIX);
    if !is_final && !frame.starts_with(INTERIM_PREFIX) {
        return Frame::Other;
    }
    let Ok(sub) = serde_json::from_str::<Subtitle>(frame) else {
        return Frame::Other;
    };
    Frame::Subtitle {
        speaker: sub.speaker_id,
        lang: sub.lang,
        is_final,
        text: sub.text,
        original: sub.original,
    }
}

/// Primary language subtag, case-insensitive: `it-IT` == `it`.
fn same_lang(a: &str, b: &str) -> bool {
    let primary = |s: &str| {
        s.split(['-', '_'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
    };
    primary(a) == primary(b)
}

// ---- the state machine -------------------------------------------------------------------

/// Smoothed microphone level of a speaker's own phone, by speaker id (`0` = unknown or quiet).
/// The pump owns the real numbers; the filter only asks. Today a conversation has two seats, so
/// the pump answers for "me" and "everyone else"; with more seats this is the one place that
/// becomes a per-peer lookup.
pub type Levels<'a> = &'a dyn Fn(&str) -> u32;

#[derive(Debug)]
struct Entry {
    speaker: String,
    norm: String,
    last: Instant,
    closed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Undecided,
    Real,
    Echo,
}

#[derive(Debug)]
struct Utterance {
    verdict: Verdict,
    started: Instant,
    last: Instant,
    /// A `subtitle_final` was seen: the next subtitle starts a new utterance.
    closed: bool,
    held: Vec<String>,
    /// The speaker's latest normalised `original` for this utterance.
    candidate: Option<String>,
}

/// One seat's outbound filter. See the module docs.
#[derive(Debug)]
pub struct EchoFilter {
    cfg: EchoConfig,
    my_id: String,
    my_lang: String,
    /// Recent original words of EVERY speaker this seat has seen, mine included, tagged.
    ledger: VecDeque<Entry>,
    /// One utterance state per other speaker, bounded to [`MAX_SPEAKERS`].
    utts: HashMap<String, Utterance>,
    mic_voiced: Option<Instant>,
    /// The ledger changed since undecided utterances were last judged.
    dirty: bool,
    suppressed: u64,
    failed_open: u64,
}

impl EchoFilter {
    pub fn new(cfg: EchoConfig, my_id: String, my_lang: String) -> Self {
        Self {
            cfg,
            my_id,
            my_lang,
            ledger: VecDeque::new(),
            utts: HashMap::new(),
            mic_voiced: None,
            dirty: false,
            suppressed: 0,
            failed_open: 0,
        }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    pub fn my_id(&self) -> &str {
        &self.my_id
    }

    /// When my own microphone last carried speech (it counts as "I spoke recently" before my
    /// caption of it has arrived).
    pub fn note_own_voice(&mut self, at: Option<Instant>) {
        if let Some(t) = at {
            if self.mic_voiced.is_none_or(|old| t > old) {
                self.mic_voiced = Some(t);
            }
        }
    }

    /// When the pump must call [`Self::on_timeout`]: the earliest end of a hold, if any.
    pub fn deadline(&self) -> Option<Instant> {
        self.utts
            .values()
            .filter(|u| u.verdict == Verdict::Undecided)
            .map(|u| u.started + HOLD_MAX)
            .min()
    }

    /// A hold budget ran out: fail open. Frames to send are appended to `out`.
    pub fn on_timeout(&mut self, now: Instant, out: &mut Vec<String>) {
        self.expire(now, out);
    }

    /// Everything still held, released (the socket is closing). Fail open.
    pub fn drain(&mut self, out: &mut Vec<String>) {
        let mut held: Vec<(Instant, Vec<String>)> = Vec::new();
        for u in self.utts.values_mut() {
            u.verdict = Verdict::Real;
            held.push((u.started, std::mem::take(&mut u.held)));
        }
        held.sort_by_key(|(t, _)| *t);
        for (_, mut frames) in held {
            out.append(&mut frames);
        }
    }

    pub fn has_report(&self) -> bool {
        self.suppressed > 0 || self.failed_open > 0
    }

    /// `(suppressed, failed_open)` since the last call. Counts only, never text.
    pub fn take_report(&mut self) -> (u64, u64) {
        (
            std::mem::take(&mut self.suppressed),
            std::mem::take(&mut self.failed_open),
        )
    }

    /// Process one outbound frame; what must be sent now (possibly earlier held frames first,
    /// possibly nothing) is appended to `out`, in order. `levels` answers how loud a speaker's
    /// own microphone currently is.
    pub fn on_frame(&mut self, now: Instant, frame: String, levels: Levels, out: &mut Vec<String>) {
        if !self.cfg.enabled {
            out.push(frame);
            return;
        }
        self.expire(now, out);
        match parse(&frame) {
            Frame::Other => out.push(frame),
            Frame::Audio { speaker } => {
                if speaker == self.my_id {
                    out.push(frame);
                } else {
                    self.other_frame(now, speaker, frame, true, false, None, levels, out);
                }
            }
            Frame::Subtitle {
                speaker,
                lang,
                is_final,
                text,
                original,
            } => {
                if speaker == self.my_id {
                    // My own caption: `text` IS my original words (a final has no `text`).
                    let words = if is_final {
                        original.or(text)
                    } else {
                        text.or(original)
                    };
                    self.record(
                        &speaker,
                        now,
                        &normalize(words.as_deref().unwrap_or("")),
                        is_final,
                    );
                    out.push(frame);
                } else {
                    // The words the speaker's ASR heard. Absent on a cross-language interim, and
                    // then NOT guessed from `text` (a translation). When the speaker's language
                    // is mine `text` IS the original.
                    let cand = original.or_else(|| {
                        (!is_final && same_lang(&lang, &self.my_lang))
                            .then_some(text)
                            .flatten()
                    });
                    self.other_frame(now, speaker, frame, false, is_final, cand, levels, out);
                }
            }
        }
        // A new entry may settle an utterance that was waiting for it (and that one, once
        // released, may settle another): a few rounds at most, bounded by the speaker table.
        for _ in 0..MAX_SPEAKERS {
            if !std::mem::take(&mut self.dirty) {
                break;
            }
            self.rejudge(now, levels, out);
        }
    }

    /// Remember `norm` as `speaker`'s latest words: an interim updates their open entry, a final
    /// closes it, anything else starts a new one.
    fn record(&mut self, speaker: &str, now: Instant, norm: &str, is_final: bool) {
        if norm.is_empty() {
            return;
        }
        self.dirty = true;
        self.ledger
            .retain(|e| now.saturating_duration_since(e.last) <= LEDGER_KEEP);
        let open = self.ledger.iter_mut().rev().find(|e| e.speaker == speaker);
        match open {
            Some(e) if !e.closed && now.saturating_duration_since(e.last) <= UTTERANCE_GAP => {
                e.norm = norm.to_string();
                e.last = now;
                e.closed = is_final;
            }
            _ => {
                self.ledger.push_back(Entry {
                    speaker: speaker.to_string(),
                    norm: norm.to_string(),
                    last: now,
                    closed: is_final,
                });
                let mine = self.ledger.iter().filter(|e| e.speaker == speaker).count();
                if mine > LEDGER_MAX {
                    if let Some(i) = self.ledger.iter().position(|e| e.speaker == speaker) {
                        self.ledger.remove(i);
                    }
                }
                if self.ledger.len() > LEDGER_TOTAL_MAX {
                    self.ledger.pop_front();
                }
            }
        }
    }

    /// Something was said by somebody other than `speaker` lately, or my own microphone carried
    /// speech: the only cases where a new utterance of `speaker` is worth holding.
    fn others_spoke_recently(&self, speaker: &str, now: Instant) -> bool {
        let within = |t: Instant| now.saturating_duration_since(t) <= OWN_RECENT;
        self.ledger
            .iter()
            .any(|e| e.speaker != speaker && within(e.last))
            || self.mic_voiced.is_some_and(within)
    }

    #[allow(clippy::too_many_arguments)]
    fn other_frame(
        &mut self,
        now: Instant,
        speaker: String,
        frame: String,
        is_audio: bool,
        is_final: bool,
        original: Option<String>,
        levels: Levels,
        out: &mut Vec<String>,
    ) {
        let joins = self.utts.get(&speaker).is_some_and(|u| {
            let idle = now.saturating_duration_since(u.last);
            if u.closed {
                is_audio && idle <= AUDIO_TAIL
            } else {
                idle <= UTTERANCE_GAP
            }
        });
        if !joins {
            self.make_room(&speaker, out);
            let verdict = if self.others_spoke_recently(&speaker, now) {
                Verdict::Undecided
            } else {
                Verdict::Real
            };
            self.utts.insert(
                speaker.clone(),
                Utterance {
                    verdict,
                    started: now,
                    last: now,
                    closed: false,
                    held: Vec::new(),
                    candidate: None,
                },
            );
        }
        let norm = original.as_deref().map(normalize).filter(|o| !o.is_empty());
        let Some(u) = self.utts.get_mut(&speaker) else {
            return;
        };
        u.last = now;
        if is_final {
            u.closed = true;
        }
        match u.verdict {
            Verdict::Real => {
                out.push(frame);
                if let Some(n) = norm {
                    self.record(&speaker, now, &n, is_final);
                }
            }
            Verdict::Echo => {}
            Verdict::Undecided => {
                if let Some(n) = norm {
                    u.candidate = Some(n);
                }
                u.held.push(frame);
                if u.held.len() > MAX_HELD {
                    self.fail_open(&speaker, out);
                } else {
                    self.try_decide(&speaker, now, levels, out);
                }
            }
        }
    }

    /// Keep the speaker table bounded: a new speaker past the limit displaces the one idle the
    /// longest, whose held frames (if any) are released.
    fn make_room(&mut self, newcomer: &str, out: &mut Vec<String>) {
        if self.utts.len() < MAX_SPEAKERS || self.utts.contains_key(newcomer) {
            return;
        }
        let oldest = self
            .utts
            .iter()
            .min_by_key(|(_, u)| u.last)
            .map(|(k, _)| k.clone());
        if let Some(k) = oldest {
            if let Some(mut u) = self.utts.remove(&k) {
                if !u.held.is_empty() {
                    self.failed_open += 1;
                    crate::metrics::record_live_echo_failopen();
                    out.append(&mut u.held);
                }
            }
        }
    }

    fn rejudge(&mut self, now: Instant, levels: Levels, out: &mut Vec<String>) {
        let mut waiting: Vec<(Instant, String)> = self
            .utts
            .iter()
            .filter(|(_, u)| u.verdict == Verdict::Undecided)
            .map(|(k, u)| (u.started, k.clone()))
            .collect();
        waiting.sort();
        for (_, k) in waiting {
            self.try_decide(&k, now, levels, out);
        }
    }

    /// Decide `speaker`'s undecided utterance if its candidate allows it.
    fn try_decide(&mut self, speaker: &str, now: Instant, levels: Levels, out: &mut Vec<String>) {
        let Some(u) = self.utts.get(speaker) else {
            return;
        };
        if u.verdict != Verdict::Undecided {
            return;
        }
        let verdict = match u.candidate.as_deref() {
            Some(c) if enough(c) => {
                if self.echo_of_someone_else(speaker, c, now, levels) {
                    Verdict::Echo
                } else if self.awaiting_my_captions(speaker, levels) {
                    // I am talking and my caption of it has not arrived: what they say may
                    // still turn out to be an echo of it. Wait, within the hold budget.
                    return;
                } else {
                    Verdict::Real
                }
            }
            // Too short to judge, or nothing to judge: wait for more unless the utterance ended.
            _ if u.closed => Verdict::Real,
            _ => return,
        };
        let Some(u) = self.utts.get_mut(speaker) else {
            return;
        };
        u.verdict = verdict;
        match verdict {
            Verdict::Real => {
                out.append(&mut u.held);
                let (cand, closed, at) = (u.candidate.clone(), u.closed, u.last);
                if let Some(c) = cand {
                    self.record(speaker, at, &c, closed);
                }
            }
            Verdict::Echo => {
                u.held.clear();
                self.suppressed += 1;
                crate::metrics::record_live_echo_suppressed();
            }
            Verdict::Undecided => {}
        }
    }

    /// `a`'s microphone is clearly louder than `b`'s: only then can `a` be the real source of
    /// words both of them heard.
    ///
    /// A level of 0 means "no recent frames" (never started, muted, or stale): UNKNOWN on either
    /// side is never evidence that one microphone is louder, so nothing is suppressed on it.
    fn clearly_louder(levels: Levels, a: &str, b: &str) -> bool {
        let (la, lb) = (u64::from(levels(a)), u64::from(levels(b)));
        la > 0 && lb > 0 && la * DOMINANCE_DEN > lb * DOMINANCE_NUM
    }

    /// My microphone carried speech after the newest caption of mine, so words of mine are still
    /// on their way to the ledger. Only matters when I could be the one an echo is an echo of.
    fn awaiting_my_captions(&self, speaker: &str, levels: Levels) -> bool {
        Self::clearly_louder(levels, &self.my_id, speaker)
            && self.mic_voiced.is_some_and(|t| {
                self.ledger
                    .iter()
                    .rev()
                    .find(|e| e.speaker == self.my_id)
                    .is_none_or(|e| t > e.last)
            })
    }

    /// `cand`, said by `speaker`, is similar to something SOMEBODY ELSE said lately (me or a third
    /// speaker) and that somebody's microphone is the clearly louder one. An utterance is never
    /// compared with its own speaker's entries.
    fn echo_of_someone_else(
        &self,
        speaker: &str,
        cand: &str,
        now: Instant,
        levels: Levels,
    ) -> bool {
        let mut sources: Vec<&str> = self
            .ledger
            .iter()
            .filter(|e| {
                e.speaker != speaker && now.saturating_duration_since(e.last) <= LEDGER_KEEP
            })
            .map(|e| e.speaker.as_str())
            .collect();
        sources.sort_unstable();
        sources.dedup();
        sources
            .into_iter()
            .filter(|y| Self::clearly_louder(levels, y, speaker))
            .any(|y| {
                let words: Vec<&str> = self
                    .ledger
                    .iter()
                    .filter(|e| {
                        e.speaker == y && now.saturating_duration_since(e.last) <= LEDGER_KEEP
                    })
                    .map(|e| e.norm.as_str())
                    .collect();
                let mut best = words
                    .iter()
                    .map(|w| similarity(cand, w))
                    .fold(0.0f64, f64::max);
                if let [.., a, b] = words.as_slice() {
                    best = best.max(similarity(cand, &format!("{a} {b}")));
                }
                best >= self.cfg.sim
            })
    }

    fn expire(&mut self, now: Instant, out: &mut Vec<String>) {
        let mut due: Vec<(Instant, String)> = self
            .utts
            .iter()
            .filter(|(_, u)| u.verdict == Verdict::Undecided && now >= u.started + HOLD_MAX)
            .map(|(k, u)| (u.started, k.clone()))
            .collect();
        due.sort();
        for (_, k) in due {
            self.fail_open(&k, out);
        }
    }

    fn fail_open(&mut self, speaker: &str, out: &mut Vec<String>) {
        if let Some(u) = self.utts.get_mut(speaker) {
            u.verdict = Verdict::Real;
            out.append(&mut u.held);
            self.failed_open += 1;
            crate::metrics::record_live_echo_failopen();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ServerMessage;

    const ME: &str = "peer-me";
    const THEM: &str = "peer-them";
    const A: &str = "peer-a";
    const B: &str = "peer-b";

    /// Mic levels: mine is the clearly louder one, everyone else's quiet.
    const MINE_LOUDER: &[(&str, u32)] = &[(ME, 1000), (THEM, 100), (A, 100), (B, 100)];

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn interim(speaker: &str, lang: &str, text: &str, original: Option<&str>) -> String {
        ServerMessage::SubtitleInterim {
            speaker_id: speaker.into(),
            speaker_name: "x".into(),
            text: text.into(),
            lang: lang.into(),
            original: original.map(Into::into),
        }
        .to_json()
    }

    fn fin(speaker: &str, lang: &str, translated: &str, original: &str) -> String {
        ServerMessage::SubtitleFinal {
            speaker_id: speaker.into(),
            speaker_name: "x".into(),
            original: original.into(),
            lang: lang.into(),
            translations: [("it".to_string(), translated.to_string())].into(),
        }
        .to_json()
    }

    fn audio(speaker: &str, seq: u64) -> String {
        ServerMessage::TranslatedAudio {
            speaker_id: speaker.into(),
            lang: "it".into(),
            seq,
            pcm16_b64: "AAAA".into(),
        }
        .to_json()
    }

    fn own_interim(text: &str) -> String {
        interim(ME, "it", text, None)
    }

    /// The other phone's speaker as it reaches me: `text` is the translation into MY language.
    fn their_interim(text: &str, original: Option<&str>) -> String {
        interim(THEM, "es", text, original)
    }

    fn their_final(translated: &str, original: &str) -> String {
        fin(THEM, "es", translated, original)
    }

    fn their_audio(seq: u64) -> String {
        audio(THEM, seq)
    }

    fn filter() -> EchoFilter {
        EchoFilter::new(EchoConfig::default(), ME.into(), "it".into())
    }

    fn push_with(
        f: &mut EchoFilter,
        now: Instant,
        frame: String,
        table: &[(&str, u32)],
    ) -> Vec<String> {
        let levels = |id: &str| table.iter().find(|(k, _)| *k == id).map_or(0, |(_, v)| *v);
        let mut out = Vec::new();
        f.on_frame(now, frame, &levels, &mut out);
        out
    }

    fn push(f: &mut EchoFilter, now: Instant, frame: String) -> Vec<String> {
        push_with(f, now, frame, MINE_LOUDER)
    }

    // ---- similarity ----

    fn score(a: &str, b: &str) -> f64 {
        similarity(&normalize(a), &normalize(b))
    }

    #[test]
    fn normalisation_drops_case_punctuation_and_diacritics() {
        assert_eq!(normalize("  Ciao,  COME stai?! "), "ciao come stai");
        assert_eq!(normalize("¿Cómo estás? ¡Niño!"), "como estas nino");
        assert_eq!(normalize("...,"), "");
    }

    #[test]
    fn echoes_score_high_and_real_replies_score_low() {
        let mine = "ciao come stai oggi";
        // Exact, and punctuation/case only.
        assert!(score("ciao come stai oggi", mine) >= 0.99);
        assert!(score("Ciao, come stai oggi?", mine) >= 0.99);
        // The same sentence heard through a Spanish ear.
        assert!(score("chao como estai hoy", mine) >= DEFAULT_SIM);
        // A part of a longer sentence of mine.
        assert!(
            score(
                "ciao come stai",
                "ciao come stai oggi mi sento bene davvero"
            ) >= 0.99
        );
        assert!(score("chao como", "ciao come stai oggi mi sento bene davvero") >= DEFAULT_SIM);
        // A real reply, and something unrelated.
        assert!(score("hola estoy bien gracias", "ciao come stai") < 0.3);
        assert!(score("me llamo Lucia y vivo en Madrid", mine) < 0.3);
        // My short phrase inside THEIR long reply is not their echo of me.
        assert!(score("bien gracias y tu como estas hoy", "come stai") < DEFAULT_SIM);
    }

    #[test]
    fn a_one_word_candidate_is_never_enough_to_judge() {
        assert!(!enough("si"));
        assert!(!enough("ok"));
        assert!(!enough("chao"));
        assert!(!enough("gracias"), "a lone Latin word is never judged");
        assert!(enough("hola amigo"));
        assert!(enough(&"字".repeat(12)), "a spaceless-script sentence is");
    }

    // ---- frames ----

    #[test]
    fn the_prefixes_are_what_the_protocol_really_emits() {
        assert!(own_interim("x").starts_with(INTERIM_PREFIX));
        assert!(their_final("a", "b").starts_with(FINAL_PREFIX));
        assert!(their_audio(1).starts_with(AUDIO_PREFIX));
        assert_eq!(audio_speaker(&their_audio(9)), Some(THEM));
        assert_eq!(audio_speaker(r#"{"type":"translated_audio""#), None);
    }

    // ---- two seats ----

    #[test]
    fn with_no_recent_speech_of_mine_everything_passes_at_once() {
        let mut f = filter();
        let t0 = Instant::now();
        for (i, frame) in [
            their_audio(1),
            their_interim("ciao come stai", Some("chao como estai")),
            their_final("ciao come stai", "chao como estai"),
        ]
        .into_iter()
        .enumerate()
        {
            let out = push(&mut f, t0 + ms(i as u64 * 50), frame.clone());
            assert_eq!(out, vec![frame], "nothing held");
        }
        assert_eq!(f.deadline(), None);
    }

    #[test]
    fn my_words_from_long_ago_do_not_trigger_a_hold() {
        let mut f = filter();
        let t0 = Instant::now();
        push(&mut f, t0, own_interim("ciao come stai oggi"));
        let late = t0 + OWN_RECENT + ms(1);
        let frame = their_audio(1);
        assert_eq!(push(&mut f, late, frame.clone()), vec![frame]);
    }

    #[test]
    fn an_echo_is_held_then_dropped_with_its_audio_and_its_final() {
        let mut f = filter();
        let t0 = Instant::now();
        push(&mut f, t0, own_interim("ciao come stai oggi"));

        // Audio arrives first: held, with a deadline.
        assert!(push(&mut f, t0 + ms(100), their_audio(1)).is_empty());
        assert_eq!(f.deadline(), Some(t0 + ms(100) + HOLD_MAX));
        // The first original decides: Echo, everything held is dropped.
        let out = push(
            &mut f,
            t0 + ms(200),
            their_interim("ciao come stai oggi", Some("chao como estai hoy")),
        );
        assert!(out.is_empty());
        assert_eq!(f.deadline(), None, "decided: nothing left to time out");
        // The rest of the utterance, final included, is dropped as it arrives.
        assert!(push(&mut f, t0 + ms(300), their_audio(2)).is_empty());
        assert!(push(
            &mut f,
            t0 + ms(400),
            their_final("ciao come stai oggi", "chao como estai hoy")
        )
        .is_empty());
        // Audio trailing the final still belongs to the echo...
        assert!(push(&mut f, t0 + ms(900), their_audio(3)).is_empty());
        // ...for good: it is not held and let through when the hold budget runs out.
        assert_eq!(f.deadline(), None);
        let mut late = Vec::new();
        f.on_timeout(t0 + ms(900) + HOLD_MAX, &mut late);
        assert!(late.is_empty());
        assert_eq!(f.take_report(), (1, 0), "one utterance, counted once");
    }

    #[test]
    fn a_real_reply_is_held_then_flushed_in_order() {
        let mut f = filter();
        let t0 = Instant::now();
        push(&mut f, t0, own_interim("ciao come stai oggi"));
        let (a1, a2) = (their_audio(1), their_audio(2));
        assert!(push(&mut f, t0 + ms(100), a1.clone()).is_empty());
        assert!(push(&mut f, t0 + ms(150), a2.clone()).is_empty());
        // The partner's real reply: the TRANSLATION shares "ciao" with me, the original does not.
        let i = their_interim("ciao sto bene grazie", Some("hola estoy bien gracias"));
        let out = push(&mut f, t0 + ms(250), i.clone());
        assert_eq!(out, vec![a1, a2, i], "held frames first, in arrival order");
        // From now on it flows straight through.
        let a3 = their_audio(3);
        assert_eq!(push(&mut f, t0 + ms(300), a3.clone()), vec![a3]);
        assert_eq!(f.take_report(), (0, 0));
    }

    #[test]
    fn the_decision_ignores_text_even_when_the_translation_matches_me() {
        let mut f = filter();
        let t0 = Instant::now();
        push(&mut f, t0, own_interim("ciao come stai"));
        // `text` is identical to my words; `original` is unrelated Spanish: Real.
        let i = their_interim("ciao come stai", Some("buenos dias a todos los presentes"));
        assert_eq!(push(&mut f, t0 + ms(50), i.clone()), vec![i]);
    }

    #[test]
    fn without_an_original_it_waits_and_then_fails_open() {
        let mut f = filter();
        let t0 = Instant::now();
        push(&mut f, t0, own_interim("ciao come stai oggi"));
        let (a, i) = (
            their_audio(1),
            // Identical text but NO original: not guessed from `text`.
            their_interim("ciao come stai oggi", None),
        );
        assert!(push(&mut f, t0 + ms(10), a.clone()).is_empty());
        assert!(push(&mut f, t0 + ms(300), i.clone()).is_empty());
        // The pump's timer fires at the deadline.
        let mut out = Vec::new();
        f.on_timeout(t0 + ms(10) + HOLD_MAX, &mut out);
        assert_eq!(out, vec![a, i], "fail open, in order");
        assert_eq!(f.take_report(), (0, 1));
        // And an utterance that was let through stays through.
        let more = their_audio(2);
        assert_eq!(push(&mut f, t0 + ms(800), more.clone()), vec![more]);
    }

    #[test]
    fn a_frame_after_the_budget_flushes_without_waiting_for_the_timer() {
        let mut f = filter();
        let t0 = Instant::now();
        push(&mut f, t0, own_interim("ciao come stai oggi"));
        let a = their_audio(1);
        assert!(push(&mut f, t0 + ms(10), a.clone()).is_empty());
        let b = their_audio(2);
        let out = push(&mut f, t0 + ms(10) + HOLD_MAX, b.clone());
        assert_eq!(out, vec![a, b]);
    }

    #[test]
    fn a_short_first_candidate_waits_for_more_words() {
        let mut f = filter();
        let t0 = Instant::now();
        push(&mut f, t0, own_interim("ciao come stai oggi"));
        // One short word cannot be judged...
        assert!(push(&mut f, t0 + ms(50), their_interim("ciao", Some("chao"))).is_empty());
        // ...the growing interim can.
        let out = push(
            &mut f,
            t0 + ms(150),
            their_interim("ciao come stai", Some("chao como estai")),
        );
        assert!(out.is_empty());
        assert_eq!(f.take_report(), (1, 0));
    }

    #[test]
    fn a_one_word_utterance_that_ends_is_let_through() {
        let mut f = filter();
        let t0 = Instant::now();
        push(&mut f, t0, own_interim("si si certo"));
        let fin = their_final("si", "si");
        assert_eq!(push(&mut f, t0 + ms(50), fin.clone()), vec![fin]);
    }

    #[test]
    fn echo_is_monotonic_within_an_utterance_and_a_new_one_starts_afresh() {
        let mut f = filter();
        let t0 = Instant::now();
        push(&mut f, t0, own_interim("ciao come stai oggi"));
        assert!(push(
            &mut f,
            t0 + ms(100),
            their_interim("ciao come stai", Some("chao como estai"))
        )
        .is_empty());
        // The ASR text drifts into something that would pass on its own: still the same echo.
        assert!(push(
            &mut f,
            t0 + ms(200),
            their_interim("hola amigos", Some("hola amigos que tal"))
        )
        .is_empty());
        assert!(push(
            &mut f,
            t0 + ms(250),
            their_final("ciao come stai", "chao como estai")
        )
        .is_empty());
        // A new utterance after the final is judged on its own: a real reply.
        let reply = their_interim("sto bene grazie", Some("estoy bien gracias"));
        assert_eq!(push(&mut f, t0 + ms(2_500), reply.clone()), vec![reply]);
    }

    #[test]
    fn a_pause_over_the_gap_starts_a_new_utterance_too() {
        let mut f = filter();
        let t0 = Instant::now();
        push(&mut f, t0, own_interim("ciao come stai oggi"));
        assert!(push(
            &mut f,
            t0 + ms(100),
            their_interim("ciao come stai", Some("chao como estai"))
        )
        .is_empty());
        let reply = their_interim("sto bene grazie", Some("estoy bien gracias"));
        assert_eq!(
            push(&mut f, t0 + ms(100) + UTTERANCE_GAP + ms(1), reply.clone()),
            vec![reply]
        );
    }

    #[test]
    fn an_echo_heard_before_my_own_caption_is_held_until_that_caption_arrives() {
        let mut f = filter();
        let t0 = Instant::now();
        // My microphone just carried speech but my caption of it is not here yet: their echo,
        // however clear, is held rather than judged against an empty ledger.
        f.note_own_voice(Some(t0));
        assert!(push(&mut f, t0 + ms(100), their_audio(1)).is_empty());
        let echo = their_interim("ciao come stai oggi", Some("chao como estai hoy"));
        assert!(push(&mut f, t0 + ms(150), echo).is_empty());
        assert!(f.deadline().is_some(), "still waiting for my words");
        // My caption lands: now it is known to be an echo, and what was held is dropped.
        let mine = own_interim("ciao come stai oggi");
        assert_eq!(push(&mut f, t0 + ms(300), mine.clone()), vec![mine]);
        assert_eq!(f.take_report(), (1, 0));
        assert_eq!(f.deadline(), None);
    }

    #[test]
    fn a_real_reply_held_while_i_talk_is_released_once_my_caption_shows_it_is_not_an_echo() {
        let mut f = filter();
        let t0 = Instant::now();
        f.note_own_voice(Some(t0));
        let reply = their_interim("sto bene grazie", Some("estoy bien gracias"));
        assert!(push(&mut f, t0 + ms(100), reply.clone()).is_empty());
        let mine = own_interim("ciao come stai oggi");
        assert_eq!(
            push(&mut f, t0 + ms(300), mine.clone()),
            vec![mine, reply],
            "my frame first, then the released one"
        );
        assert_eq!(f.take_report(), (0, 0));
    }

    #[test]
    fn a_quiet_microphone_never_waits_for_captions_it_could_not_use() {
        let mut f = filter();
        let t0 = Instant::now();
        f.note_own_voice(Some(t0));
        let reply = their_interim("sto bene grazie", Some("estoy bien gracias"));
        let quiet: &[(&str, u32)] = &[(ME, 100), (THEM, 1000)];
        assert_eq!(
            push_with(&mut f, t0 + ms(50), reply.clone(), quiet),
            vec![reply],
            "no added latency"
        );
    }

    #[test]
    fn the_ledger_keeps_the_last_eight_and_forgets_after_fifteen_seconds() {
        let mut f = filter();
        let t0 = Instant::now();
        for i in 0..12u64 {
            // Each phrase well apart (new entry every time), all inside the keep window.
            let t = t0 + Duration::from_millis(i * 1_600);
            push(
                &mut f,
                t,
                own_interim(&format!("frase numero{i} della prova")),
            );
        }
        assert_eq!(f.ledger.len(), LEDGER_MAX);
        // Far later, an own frame prunes what is older than the keep window.
        push(
            &mut f,
            t0 + Duration::from_secs(60),
            own_interim("una cosa nuova adesso"),
        );
        assert_eq!(f.ledger.len(), 1);
    }

    #[test]
    fn an_interim_updates_the_open_entry_and_a_final_closes_it() {
        let mut f = filter();
        let t0 = Instant::now();
        push(&mut f, t0, own_interim("ciao"));
        push(&mut f, t0 + ms(200), own_interim("ciao come"));
        push(&mut f, t0 + ms(400), own_interim("ciao come stai"));
        assert_eq!(f.ledger.len(), 1);
        assert_eq!(f.ledger[0].norm, "ciao come stai");
        assert_eq!(
            f.ledger[0].speaker, ME,
            "entries are tagged with their speaker"
        );
        push(&mut f, t0 + ms(500), fin(ME, "it", "", "Ciao, come stai?"));
        assert!(f.ledger[0].closed);
        push(&mut f, t0 + ms(600), own_interim("sto bene"));
        assert_eq!(
            f.ledger.len(),
            2,
            "after a final the next words are a new entry"
        );
    }

    #[test]
    fn an_echo_of_two_of_my_sentences_matches_their_concatenation() {
        let mut f = filter();
        let t0 = Instant::now();
        push(&mut f, t0, own_interim("ciao come stai oggi"));
        push(&mut f, t0 + ms(3_000), own_interim("io sto molto bene"));
        // Their ASR ran both sentences together.
        let out = push(
            &mut f,
            t0 + ms(4_000),
            their_interim("x", Some("chao como estai oggi io sto molto bene")),
        );
        assert!(out.is_empty(), "one echo spanning two of my entries");
        assert_eq!(f.take_report(), (1, 0));
    }

    #[test]
    fn my_own_frames_always_pass_untouched() {
        let mut f = filter();
        let t0 = Instant::now();
        let mine = own_interim("ciao come stai oggi");
        assert_eq!(push(&mut f, t0, mine.clone()), vec![mine]);
    }

    #[test]
    fn other_frames_are_never_held_or_judged() {
        let mut f = filter();
        let t0 = Instant::now();
        push(&mut f, t0, own_interim("ciao come stai oggi"));
        for frame in [
            r#"{"type":"peer_muted","peer_id":"x","kind":"audio","muted":true}"#.to_string(),
            r#"{"type":"error","message":"m"}"#.to_string(),
            "not json at all".to_string(),
            r#"{"type":"subtitle_interim","broken"#.to_string(),
        ] {
            assert_eq!(push(&mut f, t0 + ms(10), frame.clone()), vec![frame]);
        }
    }

    #[test]
    fn the_hold_buffer_is_bounded() {
        let mut f = filter();
        let t0 = Instant::now();
        push(&mut f, t0, own_interim("ciao come stai oggi"));
        let mut released = 0;
        for i in 0..(MAX_HELD as u64 + 5) {
            // Audio only, well inside the hold budget: nothing ever decides it.
            released += push(&mut f, t0 + ms(i), their_audio(i)).len();
            assert!(f.utts[THEM].held.len() <= MAX_HELD);
        }
        assert!(released >= MAX_HELD, "the overflow released what was held");
        assert_eq!(f.take_report(), (0, 1));
    }

    #[test]
    fn the_symmetric_seat_is_not_filtered_when_its_microphone_is_the_quieter_one() {
        // The same pair of captions as seen from the OTHER phone: there the "similar" own
        // words are the engine's reading of the first speaker's voice, and its mic is quieter.
        let t0 = Instant::now();
        let (mut loud, mut quiet) = (filter(), filter());
        let quiet_levels: &[(&str, u32)] = &[(ME, 100), (THEM, 1000)];
        push(&mut loud, t0, own_interim("ciao come stai oggi"));
        push_with(
            &mut quiet,
            t0,
            own_interim("ciao come stai oggi"),
            quiet_levels,
        );
        let echo = their_interim("ciao come stai oggi", Some("chao como estai hoy"));
        assert!(push(&mut loud, t0 + ms(100), echo.clone()).is_empty());
        assert_eq!(
            push_with(&mut quiet, t0 + ms(100), echo.clone(), quiet_levels),
            vec![echo],
            "never both seats at once"
        );
    }

    #[test]
    fn equal_or_unknown_loudness_never_suppresses() {
        let t0 = Instant::now();
        let echo = their_interim("ciao come stai oggi", Some("chao como estai hoy"));
        for (mine, theirs) in [(0, 0), (500, 500), (1000, 900), (0, 1000), (1000, 801)] {
            let mut f = filter();
            let levels = [(ME, mine), (THEM, theirs)];
            push_with(&mut f, t0, own_interim("ciao come stai oggi"), &levels);
            assert_eq!(
                push_with(&mut f, t0 + ms(50), echo.clone(), &levels),
                vec![echo.clone()],
                "levels {mine} vs {theirs}"
            );
        }
        // Just over the 5/4 ratio does suppress.
        let mut f = filter();
        let levels = [(ME, 1000), (THEM, 799)];
        push_with(&mut f, t0, own_interim("ciao come stai oggi"), &levels);
        assert!(push_with(&mut f, t0 + ms(50), echo, &levels).is_empty());
    }

    #[test]
    fn a_speaker_of_my_language_is_judged_on_the_text_that_is_their_original() {
        let t0 = Instant::now();
        // Two Italian phones: nothing is translated, `text` IS the original and `original` is
        // absent on interims. A different sentence passes...
        let mut f = filter();
        push(&mut f, t0, own_interim("ciao come stai oggi"));
        let other = interim(THEM, "it", "sto benissimo grazie mille", None);
        assert_eq!(push(&mut f, t0 + ms(50), other.clone()), vec![other]);
        // ...and so do the case and region spellings of the language...
        for lang in ["IT", "it-IT"] {
            let mut f = filter();
            push(&mut f, t0, own_interim("ciao come stai oggi"));
            let other = interim(THEM, lang, "sto benissimo grazie mille", None);
            assert_eq!(
                push(&mut f, t0 + ms(50), other.clone()),
                vec![other],
                "{lang}"
            );
        }
        // ...while the other phone's microphone captioning MY words is the same echo.
        let mut f = filter();
        push(&mut f, t0, own_interim("ciao come stai oggi"));
        let echo = interim(THEM, "it", "ciao come stai oggi", None);
        assert!(push(&mut f, t0 + ms(50), echo).is_empty());
    }

    #[test]
    fn a_disabled_filter_passes_everything() {
        let mut f = EchoFilter::new(
            EchoConfig {
                enabled: false,
                sim: DEFAULT_SIM,
            },
            ME.into(),
            "it".into(),
        );
        let t0 = Instant::now();
        push(&mut f, t0, own_interim("ciao come stai oggi"));
        let echo = their_interim("ciao come stai oggi", Some("chao como estai hoy"));
        assert_eq!(push(&mut f, t0 + ms(50), echo.clone()), vec![echo]);
        let a = their_audio(1);
        assert_eq!(push(&mut f, t0 + ms(60), a.clone()), vec![a]);
        assert_eq!(f.deadline(), None);
        assert!(!f.has_report());
    }

    #[test]
    fn a_stricter_threshold_lets_a_marginal_match_through() {
        let t0 = Instant::now();
        // Two of three words are shared: 0.67 passes 0.6 but not 0.8.
        let marginal = their_interim("x", Some("ciao come estas"));
        let mut lenient = filter();
        push(&mut lenient, t0, own_interim("ciao come stai oggi"));
        assert!(push(&mut lenient, t0 + ms(50), marginal.clone()).is_empty());
        let mut strict = EchoFilter::new(
            EchoConfig {
                enabled: true,
                sim: 0.8,
            },
            ME.into(),
            "it".into(),
        );
        push(&mut strict, t0, own_interim("ciao come stai oggi"));
        assert_eq!(
            push(&mut strict, t0 + ms(50), marginal.clone()),
            vec![marginal]
        );
    }

    #[test]
    fn closing_releases_whatever_is_held() {
        let mut f = filter();
        let t0 = Instant::now();
        push(&mut f, t0, own_interim("ciao come stai oggi"));
        let a = their_audio(1);
        assert!(push(&mut f, t0 + ms(10), a.clone()).is_empty());
        let mut out = Vec::new();
        f.drain(&mut out);
        assert_eq!(out, vec![a]);
        assert_eq!(f.deadline(), None);
    }

    #[test]
    fn a_report_is_taken_once() {
        let mut f = filter();
        assert!(!f.has_report());
        let t0 = Instant::now();
        push(&mut f, t0, own_interim("ciao come stai oggi"));
        push(
            &mut f,
            t0 + ms(10),
            their_interim("x", Some("chao como estai hoy")),
        );
        assert!(f.has_report());
        assert_eq!(f.take_report(), (1, 0));
        assert!(!f.has_report());
        assert_eq!(f.take_report(), (0, 0));
    }

    // ---- three seats: me (it), guest A (es), guest B (en) ----

    /// A is the one really talking: the loudest microphone. B sits further away.
    const A_LOUDEST: &[(&str, u32)] = &[(ME, 50), (A, 1000), (B, 100)];

    fn a_says(text: &str, original: &str) -> String {
        interim(A, "es", text, Some(original))
    }

    fn b_says(text: &str, original: &str) -> String {
        interim(B, "en", text, Some(original))
    }

    #[test]
    fn guest_bs_microphone_picking_up_guest_a_is_an_echo_at_the_host() {
        let mut f = filter();
        let t0 = Instant::now();
        // A really says something: it reaches me, is judged real, and is remembered.
        let real = a_says("buongiorno a tutti voi", "buenos dias a todos ustedes");
        assert_eq!(
            push_with(&mut f, t0, real.clone(), A_LOUDEST),
            vec![real],
            "nothing was said before it: no hold"
        );
        // B's microphone heard A, and B's session delivers A's words to me as B's own utterance.
        let a1 = audio(B, 1);
        assert!(
            push_with(&mut f, t0 + ms(300), a1, A_LOUDEST).is_empty(),
            "held"
        );
        let echo = b_says("buongiorno a tutti", "bwenos dias a todos ustedes");
        assert!(push_with(&mut f, t0 + ms(400), echo, A_LOUDEST).is_empty());
        assert!(push_with(&mut f, t0 + ms(500), audio(B, 2), A_LOUDEST).is_empty());
        assert_eq!(f.take_report(), (1, 0));
    }

    #[test]
    fn the_same_pair_seen_the_other_way_round_is_not_an_echo_when_b_is_the_louder() {
        // A's entry is the (polluted) one now: B is really talking, A's mic only hears B.
        let b_loudest: &[(&str, u32)] = &[(ME, 50), (A, 100), (B, 1000)];
        let mut f = filter();
        let t0 = Instant::now();
        let first = a_says("buongiorno a tutti voi", "buenos dias a todos ustedes");
        push_with(&mut f, t0, first, b_loudest);
        let real = b_says("buongiorno a tutti", "bwenos dias a todos ustedes");
        assert_eq!(
            push_with(&mut f, t0 + ms(400), real.clone(), b_loudest),
            vec![real]
        );
        assert_eq!(f.take_report(), (0, 0));
    }

    #[test]
    fn two_simultaneous_real_speakers_both_pass() {
        let mut f = filter();
        let t0 = Instant::now();
        let levels: &[(&str, u32)] = &[(ME, 50), (A, 1000), (B, 900)];
        let a = a_says(
            "ci vediamo domani mattina",
            "nos vemos manana por la manana",
        );
        let b = b_says(
            "il treno parte alle otto",
            "the train leaves at eight sharp",
        );
        // A is first (nothing held: nobody spoke before), B starts over A.
        assert_eq!(push_with(&mut f, t0, a.clone(), levels), vec![a]);
        assert!(push_with(&mut f, t0 + ms(50), audio(B, 1), levels).is_empty());
        let out = push_with(&mut f, t0 + ms(100), b.clone(), levels);
        assert_eq!(out, vec![audio(B, 1), b], "released in order once judged");
        // Both keep talking, interleaved, and neither is held or dropped again.
        let a2 = a_says(
            "ci vediamo domani mattina presto",
            "nos vemos manana por la manana temprano",
        );
        let b2 = b_says(
            "il treno parte alle otto in punto",
            "the train leaves at eight sharp today",
        );
        assert_eq!(
            push_with(&mut f, t0 + ms(200), a2.clone(), levels),
            vec![a2]
        );
        assert_eq!(
            push_with(&mut f, t0 + ms(250), b2.clone(), levels),
            vec![b2]
        );
        assert_eq!(f.take_report(), (0, 0));
    }

    #[test]
    fn a_real_reply_sharing_a_few_words_with_someone_else_passes() {
        let mut f = filter();
        let t0 = Instant::now();
        let first = a_says(
            "andiamo a mangiare la pizza stasera",
            "vamos a comer pizza esta noche con todos",
        );
        push_with(&mut f, t0, first, A_LOUDEST);
        // B is quieter, so the loudness rule would allow an echo: only the text keeps it real.
        let reply = b_says(
            "stasera devo lavorare fino a tardi",
            "esta noche tengo que trabajar hasta tarde",
        );
        assert_eq!(
            push_with(&mut f, t0 + ms(400), reply.clone(), A_LOUDEST),
            vec![reply]
        );
        assert_eq!(f.take_report(), (0, 0));
    }

    #[test]
    fn an_utterance_is_never_compared_with_its_own_speakers_entries() {
        let mut f = filter();
        let t0 = Instant::now();
        push(&mut f, t0, own_interim("buongiorno a tutti"));
        // A says a sentence, finishes it, and says the very same sentence again (a repeat on
        // purpose). The second is not an echo of the first: same speaker.
        let levels: &[(&str, u32)] = &[(ME, 10), (A, 1000), (B, 100)];
        let s = "buenos dias a todos ustedes";
        let first = a_says("x", s);
        assert_eq!(
            push_with(&mut f, t0 + ms(100), first.clone(), levels),
            vec![first]
        );
        push_with(&mut f, t0 + ms(200), fin(A, "es", "x", s), levels);
        let again = a_says("x", s);
        assert_eq!(
            push_with(&mut f, t0 + ms(2_000), again.clone(), levels),
            vec![again]
        );
        assert_eq!(f.take_report(), (0, 0));
    }

    #[test]
    fn my_own_words_are_echoed_by_a_third_speaker_too() {
        let mut f = filter();
        let t0 = Instant::now();
        push(&mut f, t0, own_interim("ciao come stai oggi"));
        let levels: &[(&str, u32)] = &[(ME, 1000), (A, 100), (B, 100)];
        // Both guests' microphones heard me.
        for (i, who) in [A, B].into_iter().enumerate() {
            let echo = interim(who, "es", "x", Some("chao como estai hoy"));
            assert!(push_with(&mut f, t0 + ms(100 + i as u64 * 10), echo, levels).is_empty());
        }
        assert_eq!(f.take_report(), (2, 0));
    }

    #[test]
    fn each_speaker_has_its_own_hold_and_timer() {
        let mut f = filter();
        let t0 = Instant::now();
        f.note_own_voice(Some(t0));
        assert!(push(&mut f, t0, audio(A, 1)).is_empty());
        assert!(push(&mut f, t0 + ms(400), audio(B, 1)).is_empty());
        assert_eq!(f.deadline(), Some(t0 + HOLD_MAX), "the earliest hold");
        let mut out = Vec::new();
        f.on_timeout(t0 + HOLD_MAX, &mut out);
        assert_eq!(out, vec![audio(A, 1)], "only A's budget ran out");
        assert_eq!(f.deadline(), Some(t0 + ms(400) + HOLD_MAX));
        out.clear();
        f.on_timeout(t0 + ms(400) + HOLD_MAX, &mut out);
        assert_eq!(out, vec![audio(B, 1)]);
        assert_eq!(f.take_report(), (0, 2));
    }

    #[test]
    fn the_speaker_table_is_bounded() {
        let mut f = filter();
        let t0 = Instant::now();
        f.note_own_voice(Some(t0));
        let mut released = Vec::new();
        for i in 0..(MAX_SPEAKERS as u64 + 4) {
            released.extend(push(&mut f, t0 + ms(i), audio(&format!("guest-{i}"), i)));
            assert!(f.utts.len() <= MAX_SPEAKERS);
        }
        assert_eq!(
            released.len(),
            4,
            "each displaced speaker's held frame was let go"
        );
    }

    #[test]
    fn an_unknown_level_is_never_clearly_louder_or_quieter() {
        // 0 means "no recent frames" (never started, muted, or stale): unknown on EITHER side
        // must not be read as "the other one is quieter".
        let levels = |id: &str| match id {
            "loud" => 1000,
            _ => 0,
        };
        assert!(!EchoFilter::clearly_louder(&levels, "loud", "unknown"));
        assert!(!EchoFilter::clearly_louder(&levels, "unknown", "loud"));
        assert!(!EchoFilter::clearly_louder(
            &levels,
            "unknown",
            "also-unknown"
        ));
        let both = |id: &str| if id == "a" { 1000 } else { 700 };
        assert!(EchoFilter::clearly_louder(&both, "a", "b"));
    }

    #[test]
    fn a_very_long_text_is_capped_so_similarity_stays_cheap() {
        let huge = "字".repeat(20_000);
        assert!(normalize(&huge).chars().count() <= 400);
        let started = std::time::Instant::now();
        let s = similarity(&normalize(&huge), &normalize(&huge));
        assert!(s > 0.9, "{s}");
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "{:?}",
            started.elapsed()
        );
        // The END is kept: what was said last is what is being echoed now.
        let long = format!("{} fine della frase", "x ".repeat(500));
        assert!(normalize(&long).ends_with("fine della frase"));
    }
}
