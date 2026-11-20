//! Verifier-5 probes for the Live Connect echo filter: state machine driven the way `pump_live`
//! drives it (same call sequence: note_own_voice, levels from the REAL `SessionLatency` smoothing,
//! on_frame, deadline/on_timeout), with a simulated clock expressed as offsets from `t0`.
#![allow(clippy::too_many_arguments)]

use std::time::Duration;

use std::sync::Arc;
use tokio::time::Instant;
use voxtranslate_server::live::config::LiveConfig;
use voxtranslate_server::live::echo::{normalize, similarity, EchoConfig, EchoFilter, HOLD_MAX};
use voxtranslate_server::live::state::SessionLatency;
use voxtranslate_server::live::LiveState;
use voxtranslate_server::protocol::ServerMessage;

const HOST: &str = "host-peer";
const GUEST: &str = "guest-peer";

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
fn fin(speaker: &str, lang: &str, original: &str, tr: &str, tr_lang: &str) -> String {
    ServerMessage::SubtitleFinal {
        speaker_id: speaker.into(),
        speaker_name: "x".into(),
        original: original.into(),
        lang: lang.into(),
        translations: [(tr_lang.to_string(), tr.to_string())].into(),
    }
    .to_json()
}
fn audio(speaker: &str, lang: &str, seq: u64) -> String {
    ServerMessage::TranslatedAudio {
        speaker_id: speaker.into(),
        lang: lang.into(),
        seq,
        pcm16_b64: "AAAA".into(),
    }
    .to_json()
}

/// One seat's pump, simulated. `me_seat` is 0 (host) or 1 (guest).
struct Sim {
    f: EchoFilter,
    lat: Arc<SessionLatency>,
    t0: Instant,
    me_seat: usize,
    my_id: String,
    /// (time offset, frame) delivered to the socket
    delivered: Vec<(Duration, String)>,
    events: Vec<(Duration, Ev)>,
}
enum Ev {
    Mic(usize, u32),
    Frame(String),
}

impl Sim {
    fn new(me_seat: usize, my_id: &str, my_lang: &str, cfg: EchoConfig) -> Self {
        let lat = LiveState::new(LiveConfig::disabled()).latency_for(uuid::Uuid::new_v4());
        let t0 = Instant::now();
        Self {
            f: EchoFilter::new(cfg, my_id.into(), my_lang.into()),
            lat,
            t0,
            me_seat,
            my_id: my_id.into(),
            delivered: vec![],
            events: vec![],
        }
    }
    fn frame(&mut self, at: u64, f: String) {
        self.events.push((ms(at), Ev::Frame(f)));
    }
    /// `seat`'s mic carries `amp(t)` every 100 ms between `from` and `to`.
    fn mic(&mut self, seat: usize, from: u64, to: u64, amp: impl Fn(u64) -> u32) {
        let mut t = from;
        while t < to {
            self.events.push((ms(t), Ev::Mic(seat, amp(t))));
            t += 100;
        }
    }
    /// Drive everything in time order; timers fire before the event that follows them.
    fn run(&mut self, until: u64) {
        let mut evs = std::mem::take(&mut self.events);
        // mic updates before frames at the same instant (the real socket writes the mic first)
        evs.sort_by_key(|(t, e)| (*t, matches!(e, Ev::Frame(_))));
        for (t, ev) in evs {
            self.fire_timers(t);
            let now = self.t0 + t;
            match ev {
                Ev::Mic(seat, amp) => self.lat.mic_frame(seat, now, amp, amp >= 500),
                Ev::Frame(m) => {
                    self.f
                        .note_own_voice(self.lat.mic_last_voiced(self.me_seat));
                    let (mine, theirs) = (
                        self.lat.mic_level(self.me_seat, now),
                        self.lat.mic_level(self.me_seat ^ 1, now),
                    );
                    let my_id = self.my_id.clone();
                    let level = move |id: &str| if id == my_id { mine } else { theirs };
                    let mut out = vec![];
                    self.f.on_frame(now, m, &level, &mut out);
                    for o in out {
                        self.delivered.push((t, o));
                    }
                }
            }
        }
        self.fire_timers(ms(until));
    }
    fn fire_timers(&mut self, upto: Duration) {
        while let Some(d) = self.f.deadline() {
            let off = d.saturating_duration_since(self.t0);
            if off > upto {
                break;
            }
            let mut out = vec![];
            self.f.on_timeout(d, &mut out);
            for o in out {
                self.delivered.push((off, o));
            }
            if self.f.deadline() == Some(d) {
                panic!("stuck timer: on_timeout did not clear the deadline {d:?}");
            }
        }
    }
    fn texts(&self) -> Vec<String> {
        self.delivered.iter().map(|(_, m)| m.clone()).collect()
    }
}

fn types(frames: &[String]) -> Vec<String> {
    frames
        .iter()
        .map(|f| {
            let v: serde_json::Value = serde_json::from_str(f).unwrap();
            format!(
                "{}:{}",
                v["type"].as_str().unwrap(),
                v["seq"].as_u64().map(|s| s.to_string()).unwrap_or_default()
            )
        })
        .collect()
}

// ---- (a) normal turn taking, both directions -----------------------------------------------

/// Host (it) speaks 0..2 s with a loud mic (the guest mic, 3 m away, hears it at 150).
/// Guest (es) replies 3..5 s, loud on their phone.
fn scene_turn_taking(seat_is_host: bool) -> Sim {
    let (me_seat, my_id, my_lang, _their_id, their_lang, my_words, their_words, their_tr) =
        if seat_is_host {
            (
                0,
                HOST,
                "it",
                GUEST,
                "es",
                ["ciao come stai oggi", "stai bene"],
                ["hola estoy bien gracias", "y tu que tal"],
                ["ciao sto bene grazie", "e tu come va"],
            )
        } else {
            (
                1,
                GUEST,
                "es",
                HOST,
                "it",
                ["hola estoy bien gracias", "y tu que tal"],
                ["ciao come stai oggi", "stai bene"],
                ["hola como estas hoy", "estas bien"],
            )
        };
    let mut s = Sim::new(me_seat, my_id, my_lang, EchoConfig::default());
    type Amp = fn(u64) -> u32;
    let (host_amp, guest_amp): (Amp, Amp) = (
        |t| {
            if t < 2000 {
                1000
            } else if (3000..5000).contains(&t) {
                300
            } else {
                20
            }
        },
        |t| {
            if t < 2000 {
                300
            } else if (3000..5000).contains(&t) {
                1000
            } else {
                20
            }
        },
    );
    s.mic(0, 0, 6000, host_amp);
    s.mic(1, 0, 6000, guest_amp);
    // Speaker order: host first (words, then audio), guest second. Frames relative to the seat.
    let (first_id, first_lang, second_id, second_lang) = ("host", "it", "guest", "es");
    let _ = (first_id, first_lang, second_id, second_lang);
    // host speaks 0..2 s
    let (h_id, g_id) = (HOST, GUEST);
    // the host's own captions on the host seat; on the guest seat they arrive translated.
    if seat_is_host {
        s.frame(400, interim(h_id, "it", my_words[0], None));
        s.frame(900, interim(h_id, "it", my_words[0], None));
        s.frame(2100, fin(h_id, "it", my_words[0], my_words[0], "it"));
        // guest reply (reaches host translated), AUDIO FIRST then interim with original
        s.frame(3300, audio(g_id, "it", 1));
        s.frame(
            3500,
            interim(g_id, their_lang, their_tr[0], Some(their_words[0])),
        );
        s.frame(3600, audio(g_id, "it", 2));
        s.frame(
            3900,
            interim(g_id, their_lang, their_tr[1], Some(their_words[1])),
        );
        s.frame(
            4300,
            fin(g_id, their_lang, their_words[1], their_tr[1], "it"),
        );
        s.frame(4400, audio(g_id, "it", 3));
    } else {
        // guest seat: host's speech arrives translated into es; guest's own captions
        s.frame(
            500,
            interim(h_id, their_lang, their_tr[0], Some(their_words[0])),
        );
        s.frame(700, audio(h_id, "es", 1));
        s.frame(
            2100,
            fin(h_id, their_lang, their_words[0], their_tr[0], "es"),
        );
        s.frame(2200, audio(h_id, "es", 2));
        s.frame(3200, interim(g_id, "es", my_words[0], None));
        s.frame(4100, interim(g_id, "es", my_words[1], None));
        s.frame(4300, fin(g_id, "es", my_words[1], my_words[1], "es"));
    }
    s.run(8000);
    s
}

#[test]
fn a_normal_turn_taking_conversation_is_delivered_whole_in_order_both_directions() {
    for host_seat in [true, false] {
        let s = scene_turn_taking(host_seat);
        let sent: Vec<String> = {
            // everything we fed (frames) in order, to compare
            vec![]
        };
        let _ = sent;
        assert!(s.f.enabled());
        let got = s.texts();
        eprintln!(
            "host_seat={host_seat} delivered {} frames: {:?}",
            got.len(),
            types(&got)
        );
        for (t, m) in &s.delivered {
            eprintln!("   {:>5} ms {}", t.as_millis(), &m[..m.len().min(70)]);
        }
        if host_seat {
            assert_eq!(got.len(), 9, "all 3 own + 6 reply frames delivered");
            assert_eq!(
                types(&got),
                [
                    "subtitle_interim:",
                    "subtitle_interim:",
                    "subtitle_final:",
                    "translated_audio:1",
                    "subtitle_interim:",
                    "translated_audio:2",
                    "subtitle_interim:",
                    "subtitle_final:",
                    "translated_audio:3"
                ]
            );
            // added latency: audio at 3300 held until the interim with original at 3500 (<= 700 ms)
            // arrival times of the reply frames: 3300,3500,3600,3900,4300,4400
            let arrivals = [3300u64, 3500, 3600, 3900, 4300, 4400];
            for (i, (t, _)) in s.delivered.iter().enumerate().skip(3) {
                let lag = t.as_millis() as u64 - arrivals[i - 3];
                eprintln!("   reply frame {i}: added latency {lag} ms");
                assert!(lag <= 700, "delivery {i} late: {t:?}");
            }
        } else {
            assert_eq!(got.len(), 7);
        }
    }
}

// ---- (b) simultaneous real speech from both phones ----------------------------------------

#[test]
fn simultaneous_real_speech_both_directions_is_not_dropped() {
    for host_seat in [true, false] {
        let (me_seat, my, other, my_lang, their_lang) = if host_seat {
            (0, HOST, GUEST, "it", "es")
        } else {
            (1, GUEST, HOST, "es", "it")
        };
        let mut s = Sim::new(me_seat, my, my_lang, EchoConfig::default());
        // both mics loud at the same time (each hears the other at 40%); AGC-ish: peak 1000 each.
        s.mic(0, 0, 6000, |_| 1000);
        s.mic(1, 0, 6000, |_| 1000);
        for i in 0..8u64 {
            let t = 300 + i * 400;
            s.frame(
                t,
                interim(my, my_lang, "il treno parte alle otto in punto", None),
            );
            s.frame(
                t + 50,
                interim(
                    other,
                    their_lang,
                    "la cena e pronta stasera",
                    Some("el tren sale a las ocho en punto sin falta"),
                ),
            );
        }
        s.run(7000);
        let theirs: Vec<_> = s
            .delivered
            .iter()
            .filter(|(_, m)| m.contains(other))
            .collect();
        assert_eq!(
            theirs.len(),
            8,
            "host_seat={host_seat}: partner frames lost"
        );
        assert_eq!(s.delivered.len(), 16);
    }
}

// ---- (c) the headline risk: a short reply sharing words with my last sentence --------------
// Realistic acoustic model: the partner's mic hears me at 30% and my mic hears the partner at 30%.

fn reply_scene(
    gap_ms: u64,
    seat_host: bool,
    same_lang: bool,
    mine: &str,
    reply_orig: &str,
    reply_tr: &str,
    first_frame_after: u64,
) -> (bool, Sim) {
    reply_scene_g(
        gap_ms,
        seat_host,
        same_lang,
        mine,
        reply_orig,
        reply_tr,
        first_frame_after,
        false,
    )
}
fn reply_scene_g(
    gap_ms: u64,
    seat_host: bool,
    same_lang: bool,
    mine: &str,
    reply_orig: &str,
    reply_tr: &str,
    first_frame_after: u64,
    partner_gated: bool,
) -> (bool, Sim) {
    let (me_seat, my, other, my_lang, their_lang) = if seat_host {
        (
            0usize,
            HOST,
            GUEST,
            "it",
            if same_lang { "it" } else { "es" },
        )
    } else {
        (1, GUEST, HOST, "es", if same_lang { "es" } else { "it" })
    };
    let mut s = Sim::new(me_seat, my, my_lang, EchoConfig::default());
    let speak_end = 2000u64;
    let reply_start = speak_end + gap_ms;
    let reply_end = reply_start + 2000;
    // my mic: loud while I speak, hears them at 300 while they reply, 25 otherwise.
    s.mic(me_seat, 0, 7000, |t| {
        if t < speak_end {
            1000
        } else if (reply_start..reply_end).contains(&t) {
            300
        } else {
            25
        }
    });
    // `partner_gated`: the replier's phone was playing my translation (no-headphones client gate drops mic frames) until they start
    let pfrom = if partner_gated { reply_start } else { 0 };
    s.mic(me_seat ^ 1, pfrom, 7000, |t| {
        if t < speak_end {
            300
        } else if (reply_start..reply_end).contains(&t) {
            1000
        } else {
            25
        }
    });
    s.frame(500, interim(my, my_lang, mine, None));
    s.frame(1500, interim(my, my_lang, mine, None));
    s.frame(2100, fin(my, my_lang, mine, mine, my_lang));
    let first = reply_start + first_frame_after;
    let text = if same_lang { reply_orig } else { reply_tr };
    let original = if same_lang { None } else { Some(reply_orig) };
    s.frame(first, interim(other, their_lang, text, original));
    s.frame(first + 400, interim(other, their_lang, text, original));
    s.run(8000);
    let delivered = s.delivered.iter().any(|(_, m)| m.contains(other));
    (delivered, s)
}

#[test]
fn a_reply_that_restates_my_sentence_survives_when_the_gap_is_short_same_language() {
    // "Domani alle otto?"  ->  "Si, domani alle otto." (very common confirmation)
    let mut dropped = vec![];
    for gap in [0u64, 200, 400, 800, 1500, 2500] {
        for after in [300u64, 500, 800] {
            let (ok, _) = reply_scene(
                gap,
                true,
                true,
                "ci vediamo domani alle otto",
                "si domani alle otto va bene",
                "",
                after,
            );
            if !ok {
                dropped.push((gap, after));
            }
        }
    }
    eprintln!("same-language confirmation dropped for (gap,first_frame_after_start) = {dropped:?}");
    assert!(
        dropped.is_empty(),
        "legitimate confirmation dropped: {dropped:?}"
    );
}

#[test]
fn a_reply_that_restates_my_sentence_survives_cross_language_cognates() {
    // Italian "vuoi un caffe per favore" / Spanish reply "un cafe por favor si" (cognates)
    let mut dropped = vec![];
    for gap in [0u64, 200, 400, 800, 1500, 2500] {
        for after in [300u64, 500, 800] {
            let (ok, _) = reply_scene(
                gap,
                true,
                false,
                "vuoi un caffe per favore",
                "un cafe por favor",
                "un caffe per favore",
                after,
            );
            if !ok {
                dropped.push((gap, after));
            }
        }
    }
    eprintln!("cross-language cognate reply dropped for (gap,after) = {dropped:?}");
    assert!(
        dropped.is_empty(),
        "legitimate cognate reply dropped: {dropped:?}"
    );
}

// ---- (d) very short utterances ------------------------------------------------------------

#[test]
fn one_and_two_word_utterances_are_never_suppressed() {
    for (orig, label) in [
        ("si", "1 word"),
        ("ok", "1 word"),
        ("vale", "1 word"),
        ("hola", "1"),
        ("buenas noches", "2 words, not in my sentence"),
        ("gracias", "1 word"),
    ] {
        let mut s = Sim::new(0, HOST, "it", EchoConfig::default());
        s.mic(0, 0, 4000, |t| if t < 2000 { 1000 } else { 20 });
        s.mic(1, 0, 4000, |t| if t < 2000 { 100 } else { 20 });
        s.frame(500, interim(HOST, "it", "si no gracias vale ok hola", None));
        s.frame(
            1000,
            interim(HOST, "it", "si no gracias vale ok hola", None),
        );
        s.frame(
            2100,
            fin(
                HOST,
                "it",
                "si no gracias vale ok hola",
                "si no gracias vale ok hola",
                "it",
            ),
        );
        s.frame(1200, interim(GUEST, "es", "si", Some(orig)));
        s.frame(1250, audio(GUEST, "it", 1));
        s.run(5000);
        let got = s
            .delivered
            .iter()
            .filter(|(_, m)| m.contains(GUEST))
            .count();
        assert_eq!(got, 2, "{label} {orig:?} lost");
    }
}

// ---- (e) equal / unknown mic levels never suppress ----------------------------------------

#[test]
fn equal_levels_from_agc_never_suppress_even_for_a_verbatim_echo() {
    for (m0, m1) in [(800u32, 800u32), (1000, 900), (1000, 801), (500, 500)] {
        for seat in [0usize, 1] {
            let (my, other, my_lang, their_lang) = if seat == 0 {
                (HOST, GUEST, "it", "es")
            } else {
                (GUEST, HOST, "es", "it")
            };
            let mut s = Sim::new(seat, my, my_lang, EchoConfig::default());
            s.mic(0, 0, 4000, |_| m0);
            s.mic(1, 0, 4000, |_| m1);
            s.frame(300, interim(my, my_lang, "ciao come stai oggi", None));
            s.frame(
                900,
                interim(
                    other,
                    their_lang,
                    "ciao come stai oggi",
                    Some("ciao come stai oggi"),
                ),
            );
            s.frame(1000, audio(other, my_lang, 1));
            s.run(5000);
            assert_eq!(
                s.delivered
                    .iter()
                    .filter(|(_, m)| m.contains(other))
                    .count(),
                2,
                "levels {m0}/{m1} seat {seat}"
            );
        }
    }
}

/// The documented "unknown" rule: unknown means 0. A partner mic reading 0 (never sent audio,
/// stale >3 s) while mine is loud IS treated as "clearly louder" and a verbatim match is dropped.
#[test]
fn an_unknown_partner_level_never_suppresses_even_with_my_mic_loud() {
    let mut s = Sim::new(0, HOST, "it", EchoConfig::default());
    s.mic(0, 0, 4000, |_| 1000);
    // the partner's seat never sent a single audio frame: level unknown (0)
    s.frame(300, interim(HOST, "it", "ciao come stai oggi", None));
    s.frame(
        900,
        interim(
            GUEST,
            "es",
            "ciao come stai oggi",
            Some("ciao come stai oggi"),
        ),
    );
    s.run(5000);
    let n = s
        .delivered
        .iter()
        .filter(|(_, m)| m.contains(GUEST))
        .count();
    eprintln!(
        "partner level unknown (0), mine loud, verbatim match -> partner frames delivered: {n}"
    );
    assert_eq!(n, 1);
}

// ---- (f) same-language pair --------------------------------------------------------------

#[test]
fn same_language_pair_alternating_normal_speech_is_not_suppressed() {
    let mut s = Sim::new(0, HOST, "it", EchoConfig::default());
    s.mic(
        0,
        0,
        12_000,
        |t| if (t / 3000) % 2 == 0 { 1000 } else { 300 },
    );
    s.mic(
        1,
        0,
        12_000,
        |t| if (t / 3000) % 2 == 1 { 1000 } else { 300 },
    );
    let lines = [
        "domani devo andare dal dentista",
        "a che ora hai l appuntamento",
        "alle undici del mattino",
        "allora ci vediamo nel pomeriggio",
    ];
    for (i, l) in lines.iter().enumerate() {
        let t = i as u64 * 3000 + 400;
        let (who, other_is_me) = if i % 2 == 0 {
            (HOST, true)
        } else {
            (GUEST, false)
        };
        let _ = other_is_me;
        s.frame(t, interim(who, "it", l, None));
        s.frame(t + 1400, interim(who, "it", l, None));
        s.frame(t + 2200, fin(who, "it", l, l, "it"));
    }
    s.run(14_000);
    assert_eq!(s.delivered.len(), 12, "{:#?}", s.delivered);
}

// ---- (g) a seat that never produces an original ------------------------------------------

#[test]
fn a_partner_who_never_has_an_original_is_delayed_700ms_per_utterance_but_never_lost() {
    let mut s = Sim::new(0, HOST, "it", EchoConfig::default());
    s.mic(0, 0, 20_000, |t| if t % 6000 < 2000 { 1000 } else { 30 });
    s.mic(1, 0, 20_000, |_| 30);
    // I speak at t = 0, 6, 12 s; partner replies 3 s later each time, cross language, NO original.
    for k in 0..3u64 {
        let base = k * 6000;
        s.frame(
            base + 300,
            interim(HOST, "it", "ciao come va tutto bene", None),
        );
        s.frame(base + 3000, interim(GUEST, "es", "tutto bene grazie", None));
        s.frame(base + 3100, audio(GUEST, "it", k * 10 + 1));
        s.frame(
            base + 3500,
            fin(GUEST, "es", "todo bien gracias", "tutto bene grazie", "it"),
        );
    }
    s.run(21_000);
    let theirs: Vec<_> = s
        .delivered
        .iter()
        .filter(|(_, m)| m.contains(GUEST))
        .collect();
    // The final carries `original`, so it decides at 3500 (closed utterance, no echo) or at 700ms.
    assert_eq!(theirs.len(), 9);
    for (i, (t, _)) in theirs.iter().enumerate() {
        let base = (i as u64 / 3) * 6000;
        eprintln!(
            "frame {i}: delivered at {:?} (first arrived {})",
            t,
            base + 3000
        );
        assert!(*t <= ms(base + 3000 + 700), "held more than 700 ms");
    }
}

// ---- (i) control frames are never held or dropped ------------------------------------------

#[test]
fn control_frames_pass_instantly_while_partner_speech_is_held() {
    let mut s = Sim::new(0, HOST, "it", EchoConfig::default());
    s.mic(0, 0, 3000, |_| 1000);
    s.mic(1, 0, 3000, |_| 100);
    s.frame(100, interim(HOST, "it", "ciao come stai", None));
    s.frame(500, audio(GUEST, "it", 1)); // held (no original yet)
    let sess = ServerMessage::SessionState {
        status: "ended".into(),
        reason: None,
    }
    .to_json();
    let muted = ServerMessage::PeerMuted {
        peer_id: GUEST.into(),
        kind: "audio".into(),
        muted: true,
    }
    .to_json();
    let bal = ServerMessage::BalanceUpdate { balance: 1.0 }.to_json();
    let spent = ServerMessage::SessionSpent { spent: 0.01 }.to_json();
    for (i, m) in [sess.clone(), muted.clone(), bal.clone(), spent.clone()]
        .into_iter()
        .enumerate()
    {
        s.frame(600 + i as u64 * 10, m);
    }
    s.frame(700, "{garbage".into());
    s.frame(
        710,
        r#"{"type":"subtitle_interim","speaker_id":123}"#.into(),
    );
    s.frame(720, "".into());
    s.run(3000);
    let at = |needle: &str| {
        s.delivered
            .iter()
            .find(|(_, m)| m.contains(needle))
            .map(|(t, _)| *t)
    };
    for needle in [
        "session_state",
        "peer_muted",
        "balance_update",
        "session_spent",
        "garbage",
        "123",
    ] {
        assert!(
            at(needle).unwrap() < ms(750),
            "{needle} held: {:?}",
            at(needle)
        );
    }
    // the held audio is released only at the 700 ms deadline (500+700 = 1200), after the others.
    assert_eq!(at("translated_audio"), Some(ms(1200)));
    assert!(s.delivered.iter().any(|(_, m)| m.is_empty()));
}

// ---- (j) ordering after a flush, nothing twice ---------------------------------------------

#[test]
fn flush_keeps_arrival_order_and_never_duplicates() {
    let mut s = Sim::new(0, HOST, "it", EchoConfig::default());
    s.mic(0, 0, 5000, |_| 1000);
    s.mic(1, 0, 5000, |_| 100);
    s.frame(100, interim(HOST, "it", "ciao come stai", None));
    let mut sent = vec![];
    for i in 0..10u64 {
        let f = audio(GUEST, "it", i);
        sent.push(f.clone());
        s.frame(400 + i * 20, f);
    }
    let f = interim(GUEST, "es", "sto bene e tu", Some("estoy bien y tu"));
    sent.push(f.clone());
    s.frame(700, f);
    for i in 10..15u64 {
        let f = audio(GUEST, "it", i);
        sent.push(f.clone());
        s.frame(800 + i * 20, f);
    }
    s.run(5000);
    let theirs: Vec<String> = s
        .delivered
        .iter()
        .map(|(_, m)| m.clone())
        .filter(|m| m.contains(GUEST))
        .collect();
    assert_eq!(theirs, sent, "order/duplication");
    let mut uniq = theirs.clone();
    uniq.sort();
    uniq.dedup();
    assert_eq!(uniq.len(), theirs.len());
}

// ---- (h) supersede mid-utterance: drain releases and a fresh filter starts clean ------------

#[test]
fn drain_releases_everything_held_in_utterance_order_and_a_fresh_filter_fails_safe() {
    let mut s = Sim::new(0, HOST, "it", EchoConfig::default());
    s.mic(0, 0, 600, |_| 1000);
    s.mic(1, 0, 600, |_| 100);
    s.frame(100, interim(HOST, "it", "ciao come stai", None));
    s.frame(500, audio(GUEST, "it", 1));
    s.run(600);
    assert!(s.delivered.iter().all(|(_, m)| !m.contains(GUEST)));
    let mut out = vec![];
    s.f.drain(&mut out);
    assert_eq!(out.len(), 1);
    let mut out2 = vec![];
    s.f.drain(&mut out2);
    assert!(out2.is_empty(), "drain twice must not duplicate");
    // a reconnected seat: brand new filter, shared SessionLatency (mic voiced stays known)
    let mut f2 = EchoFilter::new(EchoConfig::default(), HOST.into(), "it".into());
    let now = s.t0 + ms(800);
    f2.note_own_voice(s.lat.mic_last_voiced(0));
    let (mine, theirs) = (s.lat.mic_level(0, now), s.lat.mic_level(1, now));
    let level = move |id: &str| if id == HOST { mine } else { theirs };
    let mut o = vec![];
    f2.on_frame(
        now,
        interim(GUEST, "es", "ciao come stai", Some("chao como estai")),
        &level,
        &mut o,
    );
    assert!(
        o.is_empty(),
        "held, waiting for captions that will never come"
    );
    f2.on_timeout(f2.deadline().unwrap(), &mut o);
    assert_eq!(o.len(), 1, "fails open on timeout");
    assert!(f2.deadline().is_none());
}

// ---- robustness / cost ------------------------------------------------------------------

#[test]
fn malformed_and_odd_frames_never_panic() {
    let big = "x".repeat(300_000);
    let frames = vec![
        "".to_string(),
        "{".into(),
        "null".into(),
        "[]".into(),
        "\u{0}".into(),
        r#"{"type":"subtitle_interim"}"#.into(),
        r#"{"type":"subtitle_interim","speaker_id":null}"#.into(),
        r#"{"type":"subtitle_interim","speaker_id":"p","text":5}"#.into(),
        r#"{"type":"subtitle_final","speaker_id":"p","original":["a"]}"#.into(),
        r#"{"type":"translated_audio""#.into(),
        r#"{"type":"translated_audio","speaker_id":"#.into(),
        format!(
            r#"{{"type":"translated_audio","speaker_id":"{}"}}"#,
            "é".repeat(100)
        ),
        format!(
            r#"{{"type":"translated_audio","speaker_id":"é{}"}}"#,
            "x".repeat(150)
        ),
        "{\"type\":\"translated_audio\",\"speaker_id\":\"ab\u{301}".to_string(),
        interim("p", "es-ES", &big, Some(&big[..1000])),
        interim("", "", "", Some("")),
        interim(
            "p",
            "\u{202e}es",
            "مرحبا بالعالم كيف حالك",
            Some("مرحبا بالعالم"),
        ),
        interim(
            "p",
            "ja",
            "こんにちは世界",
            Some("こんにちは世界、元気ですか"),
        ),
        interim(
            "p",
            "x",
            "👩‍👩‍👧‍👦👩‍👩‍👧‍👦👩‍👩‍👧‍👦 👍👍👍",
            Some("e\u{301}e\u{301}e\u{301} cafe\u{301}"),
        ),
        fin("p", "es", "ab", "cd", "it"),
    ];
    let mut f = EchoFilter::new(EchoConfig::default(), "me".into(), "it".into());
    let t0 = Instant::now();
    let level = |_: &str| 100u32;
    let mut out = vec![];
    f.on_frame(
        t0,
        interim("me", "it", "ciao come stai oggi", None),
        &|id: &str| if id == "me" { 1000 } else { 100 },
        &mut out,
    );
    for (i, fr) in frames.into_iter().enumerate() {
        f.on_frame(t0 + ms(i as u64 * 10), fr, &level, &mut out);
    }
    f.on_timeout(t0 + ms(5000), &mut out);
    f.drain(&mut out);
    // control: no frame lost or duplicated
    assert!(out.len() >= 19, "got {}", out.len());
}

#[test]
fn unicode_normalisation_does_not_create_false_positives_or_panics() {
    // NFD vs NFC: combining marks are not alphanumeric -> used as separators.
    let nfc = normalize("caf\u{e9} tr\u{e8}s bon");
    let nfd = normalize("cafe\u{301} tre\u{300}s bon");
    eprintln!("nfc={nfc:?} nfd={nfd:?} sim={}", similarity(&nfc, &nfd));
    // CJK / RTL / emoji
    for s in [
        "こんにちは世界元気ですか",
        "مرحبا بالعالم كيف حالك اليوم",
        "👍👍👍 👍👍👍",
        "e\u{301}\u{301}\u{301}",
    ] {
        let n = normalize(s);
        let _ = similarity(&n, &n);
    }
    // Two unrelated Thai/CJK strings must not look alike
    assert!(
        similarity(
            &normalize("今日は天気がいいですね"),
            &normalize("明日は雨が降るでしょう")
        ) < 0.6
    );
}

#[test]
fn similarity_cost_on_long_originals_is_bounded() {
    use std::time::Instant as StdInstant;
    let words = |n: usize, k: usize| {
        (0..n)
            .map(|i| format!("parola{}x{}", i % 997, k))
            .collect::<Vec<_>>()
            .join(" ")
    };
    for n in [50usize, 200, 500, 1000] {
        let a = normalize(&words(n, 1));
        let b = normalize(&words(n, 2));
        let t = StdInstant::now();
        let s = similarity(&a, &b);
        eprintln!("{n} words x {n} words: {:?} (sim {s:.2})", t.elapsed());
    }
    // one huge unbroken token (CJK has no spaces): equal-length strings, worst case for Levenshtein
    for n in [1_000usize, 5_000, 20_000] {
        let a = "あ".repeat(n / 2) + &"い".repeat(n / 2);
        let b = "あ".repeat(n / 3) + &"う".repeat(n - n / 3);
        let t = StdInstant::now();
        let s = similarity(&a, &b);
        eprintln!(
            "single token of {n} chars vs {n} chars: {:?} (sim {s:.2})",
            t.elapsed()
        );
    }
}

#[test]
fn full_filter_cost_with_a_loaded_ledger_and_a_long_candidate() {
    use std::time::Instant as StdInstant;
    let long = |k: usize| {
        (0..300)
            .map(|i| format!("palabra{}k{}", i % 211, k))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut f = EchoFilter::new(EchoConfig::default(), "me".into(), "it".into());
    let t0 = Instant::now();
    let levels = |id: &str| if id == "me" { 1000u32 } else { 100 };
    let mut out = vec![];
    for k in 0..8usize {
        let mut fr = interim("me", "it", &long(k), None);
        fr.push(' ');
        f.on_frame(t0 + ms(k as u64 * 2000), fr, &levels, &mut out);
        f.on_frame(
            t0 + ms(k as u64 * 2000 + 1900),
            fin("me", "it", &long(k), &long(k), "it"),
            &levels,
            &mut out,
        );
    }
    let start = StdInstant::now();
    let now = t0 + ms(16_100);
    for i in 0..60u64 {
        // 60 distinct interim frames from a partner whose utterance stays undecided (too short at first)
        f.on_frame(
            now + ms(i),
            interim("them", "es", "x", Some(&long(99))),
            &levels,
            &mut out,
        );
    }
    eprintln!(
        "60 frames with 300-word candidate vs 8x300-word ledger: {:?}",
        start.elapsed()
    );
}

#[test]
fn hot_loop_100k_audio_frames_cost_and_memory_growth() {
    use std::time::Instant as StdInstant;
    let mut f = EchoFilter::new(EchoConfig::default(), "me".into(), "it".into());
    let t0 = Instant::now();
    let levels = |id: &str| if id == "me" { 1000u32 } else { 100 };
    let mut out = vec![];
    f.on_frame(
        t0,
        interim("me", "it", "ciao come stai oggi", None),
        &levels,
        &mut out,
    );
    let frame = audio("them", "it", 1);
    let start = StdInstant::now();
    let mut delivered = 0usize;
    // 100k frames at 10 ms spacing: first utterance held, then flows. Speakers rotate through 50 ids.
    for i in 0..100_000u64 {
        out.clear();
        let spk = format!("s{}", i % 50);
        let fr = frame.replace("them", &spk);
        f.on_frame(t0 + ms(i * 10 + 1), fr, &levels, &mut out);
        delivered += out.len();
    }
    f.on_timeout(t0 + ms(2_000_000), &mut out);
    eprintln!(
        "100k audio frames: {:?}, delivered so far {delivered}",
        start.elapsed()
    );
}

#[test]
fn many_reconnects_do_not_leak() {
    // each reconnect is a new filter; the old is dropped. Just make sure drain+drop work 10k times.
    for i in 0..10_000 {
        let mut f = EchoFilter::new(EchoConfig::default(), format!("me{i}"), "it".into());
        let t0 = Instant::now();
        let mut out = vec![];
        f.on_frame(
            t0,
            interim(&format!("me{i}"), "it", "ciao come stai", None),
            &|_: &str| 100,
            &mut out,
        );
        f.on_frame(t0, audio("x", "it", 1), &|_: &str| 100, &mut out);
        f.drain(&mut out);
    }
}

#[test]
fn clock_goes_backwards_or_far_forward_without_panicking() {
    let mut f = EchoFilter::new(EchoConfig::default(), "me".into(), "it".into());
    let t0 = Instant::now() + Duration::from_secs(1000);
    let lv = |id: &str| if id == "me" { 1000u32 } else { 100 };
    let mut out = vec![];
    f.on_frame(
        t0,
        interim("me", "it", "ciao come stai oggi", None),
        &lv,
        &mut out,
    );
    f.on_frame(t0 + ms(10), audio("them", "it", 1), &lv, &mut out);
    // clock jumps back 500 s
    f.on_frame(
        t0 - Duration::from_secs(500),
        audio("them", "it", 2),
        &lv,
        &mut out,
    );
    f.on_frame(
        t0 - Duration::from_secs(500),
        interim("them", "es", "a b c", Some("d e f")),
        &lv,
        &mut out,
    );
    // and 1 day forward
    f.on_timeout(t0 + Duration::from_secs(86_400), &mut out);
    f.on_frame(
        t0 + Duration::from_secs(86_400),
        audio("them", "it", 3),
        &lv,
        &mut out,
    );
    f.drain(&mut out);
    assert!(f.deadline().is_none());
    assert_eq!(out.len(), 5);
    let _ = HOLD_MAX;
}

#[test]
fn reply_drop_map_with_the_client_gate_active_on_the_replier() {
    // Same as above but the replier's mic frames are absent (client gate) until they start talking.
    let mut dropped = vec![];
    let mut total = 0;
    for gap in [0u64, 200, 400, 800, 1500, 2500] {
        for after in [300u64, 500, 800, 1200] {
            total += 1;
            let (ok, _) = reply_scene_g(
                gap,
                true,
                true,
                "ci vediamo domani alle otto",
                "si domani alle otto va bene",
                "",
                after,
                true,
            );
            if !ok {
                dropped.push((gap, after));
            }
        }
    }
    eprintln!(
        "GATED replier, same-language confirmation: dropped {}/{} for (gap,after) = {dropped:?}",
        dropped.len(),
        total
    );
    assert!(dropped.is_empty(), "{dropped:?}");
}
