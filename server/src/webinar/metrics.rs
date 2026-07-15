//! Pure, batch-at-finalization billing for a finished webinar (Webinar Analytics
//! Phase A). No DB, no clock, no config — it takes the raw event timeline and the
//! resolved rates, sweeps the timeline, and returns the metrics + host cost.
//!
//! Cost model (mirrors the meet per-tick meter in `usage.rs` EXACTLY): the host
//! pays their org, per broadcast-minute, for
//!
//! ```text
//! cost = Σ over intervals [ interval_minutes × ratePerMin × K ]
//! ```
//!
//! where `K` is the number of DISTINCT participant languages that DIFFER from the
//! webinar's `source_language` during that interval — i.e. the billable TRANSLATION
//! streams, exactly like the meet meter's `billable_streams` (distinct targets minus
//! the speaker's language). When `K == 0` (nobody to translate for) the interval is
//! SKIPPED — no charge — mirroring the meet meter, which `continue`s a tick with no
//! translation targets. There is NO "+1 for the host STT" and NO separate TTS factor:
//! the per-tier `rate_per_minute` already bundles STT + translation + TTS per stream.
//! Enhanced simply costs more because it resolves to the Cartesia per-minute rate.
//!
//! K is DYNAMIC: it changes every time a viewer joins, leaves, or (via a re-`join`
//! with a new `language_code`) changes language. We reconstruct the active set from
//! the `webinar_events` log, which today records only `join` (with the participant's
//! `language_code`) and `leave` events — that is enough to know, at every instant,
//! which participants are present and in which language. Participants already present
//! at go-live (they joined the waiting room BEFORE `actual_start`) are seeded into the
//! active set from their pre-start state so they are billed from the first interval.
//!
//! Credits: the returned `cost_credits` is in the org INTEGER pool currency
//! (100 credits = $1), rounded UP so we never under-charge a fractional cent.

use chrono::{DateTime, Utc};
use std::collections::BTreeMap;

/// The per-run rates the pure function integrates over the timeline. Resolved from
/// config/engine metadata by the caller so this function stays config-free.
#[derive(Debug, Clone, Copy)]
pub struct WebinarRates {
    /// User-facing rate per minute, USD, for ONE translation stream. This is the meet
    /// meter's `user_rate_per_minute` (`cost_per_minute × (1 + markup)`) for the
    /// webinar's tier, which already bundles STT + translation + TTS per stream.
    pub rate_per_minute_usd: f64,
    /// Credits per USD (the org pool is `100 credits = $1`).
    pub credits_per_usd: f64,
}

impl WebinarRates {
    /// Standard tier: consumer per-minute rate.
    pub fn standard(rate_per_minute_usd: f64) -> Self {
        Self {
            rate_per_minute_usd,
            credits_per_usd: 100.0,
        }
    }

    /// Enhanced tier: the Cartesia per-minute rate already bundles its STT + TTS, so
    /// there is no separate factor — Enhanced pays more purely via a higher rate.
    pub fn enhanced(rate_per_minute_usd: f64) -> Self {
        Self {
            rate_per_minute_usd,
            credits_per_usd: 100.0,
        }
    }
}

/// A single row from `webinar_events` relevant to the cost sweep. Only `join` and
/// `leave` are emitted today; a re-`join` with a new `language_code` is how a
/// participant changes language. Ordered by `at`, then joins before leaves at the
/// same instant so a same-timestamp re-join is applied after its prior leave.
#[derive(Debug, Clone)]
pub struct WebinarEvent {
    pub participant_id: uuid::Uuid,
    /// `"join"` or `"leave"`. Unknown types are ignored by the sweep.
    pub kind: String,
    /// The participant's language at this event (set on `join`; `None` on `leave`).
    pub language_code: Option<String>,
    pub at: DateTime<Utc>,
}

/// The finalized rollup written to `webinar_sessions` and used to bill the host.
#[derive(Debug, Clone, PartialEq)]
pub struct WebinarMetrics {
    pub duration_seconds: i64,
    pub host_online_seconds: i64,
    pub peak_viewers: i32,
    pub translated_language_count: i32,
    pub cost_credits: i32,
}

/// Ceiling for non-negative f64 → i32 (matches `credits.rs` intent: never
/// under-charge a fractional cent), with a tiny epsilon so f64 accumulation error
/// (e.g. a summed `30.0000000000004`) doesn't spuriously round a whole-number credit
/// total up. The epsilon (1e-6 credits = one ten-thousandth of a cent) is far below
/// any real charge granularity.
fn ceil_i32(v: f64) -> i32 {
    if v <= 0.0 {
        0
    } else {
        (v - 1e-6).ceil().max(0.0) as i32
    }
}

/// Sweep the event timeline and compute the finalized metrics + host cost.
///
/// * `events` — the webinar's `webinar_events` rows (any order; sorted internally).
/// * `source_lang` — the webinar's `source_language`; participants on it need no
///   translation and are not counted toward K.
/// * `_tier` — kept for signature stability / future per-tier logic; the tier's
///   pricing is already resolved into `rates` by the caller.
/// * `actual_start` / `actual_end` — the broadcast span; cost integrates only within
///   `[start, end]`. If `end <= start` the run is zero-length (cost 0).
/// * `rates` — resolved per-tier rates.
pub fn finalize_webinar_metrics(
    events: &[WebinarEvent],
    source_lang: &str,
    _tier: &str,
    actual_start: DateTime<Utc>,
    actual_end: DateTime<Utc>,
    rates: WebinarRates,
) -> WebinarMetrics {
    let duration_seconds = (actual_end - actual_start).num_seconds().max(0);

    // Zero/negative-length broadcast: nothing to integrate → cost 0. Return early.
    if duration_seconds == 0 {
        return WebinarMetrics {
            duration_seconds: 0,
            host_online_seconds: 0,
            peak_viewers: 0,
            translated_language_count: 0,
            cost_credits: 0,
        };
    }

    // Active participant → current language. A `join` inserts/updates; a `leave`
    // removes. We compare the stored code verbatim, matching how the client sends and
    // how `source_language` is stored.
    let mut active: BTreeMap<uuid::Uuid, String> = BTreeMap::new();

    // Every distinct non-source language that was EVER active (for the analytics
    // `translated_language_count`, independent of the per-interval K integration).
    let mut ever_foreign: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    // SEED the active set with viewers already present at go-live: for each
    // participant, take their most recent join/leave BEFORE `actual_start`; if that
    // latest pre-start event is a `join`, they were still in the room when the host
    // went live → they are billable from the first interval. This does NOT bill the
    // pre-start waiting-room time (cost integration still starts at `actual_start`);
    // it only reconstructs the initial presence the in-window sweep would otherwise
    // miss (they joined the waiting room, so their `join` has `at < actual_start`).
    {
        // Latest pre-start event per participant.
        let mut latest_pre: BTreeMap<uuid::Uuid, &WebinarEvent> = BTreeMap::new();
        for e in events.iter().filter(|e| e.at < actual_start) {
            match latest_pre.get(&e.participant_id) {
                // On a tie (same `at`), a `join` supersedes a `leave` so a
                // same-instant re-join wins — consistent with the in-window ordering.
                Some(prev) if prev.at > e.at => {}
                Some(prev) if prev.at == e.at && !(prev.kind == "leave" && e.kind == "join") => {}
                _ => {
                    latest_pre.insert(e.participant_id, e);
                }
            }
        }
        for (pid, e) in latest_pre {
            if e.kind == "join" {
                let lang = e
                    .language_code
                    .clone()
                    .unwrap_or_else(|| source_lang.to_string());
                if lang.as_str() != source_lang {
                    ever_foreign.insert(lang.clone());
                }
                active.insert(pid, lang);
            }
        }
    }

    // peak_viewers must account for the seeded (already-present) viewers too.
    let mut peak_viewers: i32 = active.len() as i32;

    // Build the sorted in-window event stream, clamped to [start, end]. Sort by time,
    // then apply leaves before joins at the same instant so a same-timestamp re-join
    // supersedes a prior presence rather than colliding with it.
    let mut sorted: Vec<&WebinarEvent> = events
        .iter()
        .filter(|e| e.at >= actual_start && e.at <= actual_end)
        .collect();
    sorted.sort_by(|a, b| {
        a.at.cmp(&b.at).then_with(|| {
            let rank = |k: &str| if k == "leave" { 0 } else { 1 };
            rank(&a.kind).cmp(&rank(&b.kind))
        })
    });

    // Integrate cost over the intervals between consecutive event times. Within an
    // interval the active set (and thus K) is constant.
    let mut cost_usd = 0.0f64;
    let mut cursor = actual_start;

    // Helper: distinct foreign languages currently active = K = billable streams.
    let k_now = |active: &BTreeMap<uuid::Uuid, String>| -> usize {
        active
            .values()
            .filter(|l| l.as_str() != source_lang)
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    };

    // Per-minute billed cost for a given K: `rate × K` (K=0 ⇒ 0, meter skips the tick).
    let interval_cost_usd =
        |k: usize, minutes: f64| -> f64 { minutes * rates.rate_per_minute_usd * k as f64 };

    for ev in &sorted {
        // Bill the span [cursor, ev.at) at the CURRENT active set before applying
        // this event.
        if ev.at > cursor {
            let minutes = (ev.at - cursor).num_seconds() as f64 / 60.0;
            cost_usd += interval_cost_usd(k_now(&active), minutes);
            cursor = ev.at;
        }
        match ev.kind.as_str() {
            "join" => {
                if let Some(lang) = &ev.language_code {
                    active.insert(ev.participant_id, lang.clone());
                    if lang.as_str() != source_lang {
                        ever_foreign.insert(lang.clone());
                    }
                } else {
                    // A join with no language: treat as present on the source
                    // language (no translation needed).
                    active.insert(ev.participant_id, source_lang.to_string());
                }
                peak_viewers = peak_viewers.max(active.len() as i32);
            }
            "leave" => {
                active.remove(&ev.participant_id);
            }
            _ => {}
        }
    }

    // Final span [cursor, actual_end) at the last-known active set.
    if actual_end > cursor {
        let minutes = (actual_end - cursor).num_seconds() as f64 / 60.0;
        cost_usd += interval_cost_usd(k_now(&active), minutes);
    }

    let cost_credits = ceil_i32(cost_usd * rates.credits_per_usd);

    WebinarMetrics {
        duration_seconds,
        // Phase A: the host runs STT for the whole broadcast; host presence signal
        // is future work, so online == broadcast span.
        host_online_seconds: duration_seconds,
        peak_viewers,
        translated_language_count: ever_foreign.len() as i32,
        cost_credits,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use uuid::Uuid;

    fn t(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + secs, 0).unwrap()
    }

    fn ev(pid: Uuid, kind: &str, lang: Option<&str>, at_secs: i64) -> WebinarEvent {
        WebinarEvent {
            participant_id: pid,
            kind: kind.into(),
            language_code: lang.map(str::to_string),
            at: t(at_secs),
        }
    }

    // rate_per_minute = 0.01 USD (Standard default: 0.008 × 1.25). credits = ×100.
    // ⇒ 1 stream-minute = 0.01 USD = 1.0 credit. Easy to reason about by hand.
    fn std_rates() -> WebinarRates {
        WebinarRates::standard(0.01)
    }

    #[test]
    fn no_participants_costs_nothing() {
        // 10-minute broadcast, nobody watching. K=0 the whole time → meter skips every
        // interval → cost 0 (no host-STT surcharge in the corrected model).
        let m = finalize_webinar_metrics(&[], "en", "standard", t(0), t(600), std_rates());
        assert_eq!(m.duration_seconds, 600);
        assert_eq!(m.host_online_seconds, 600);
        assert_eq!(m.peak_viewers, 0);
        assert_eq!(m.translated_language_count, 0);
        assert_eq!(m.cost_credits, 0);
    }

    #[test]
    fn one_same_language_participant_k_is_zero() {
        // Source en, one participant also en for the whole 10 min → K stays 0.
        // The "minus 1 for same-language" rule: this viewer is NOT a billable stream,
        // and with no foreign viewer K=0 → cost 0.
        let p = Uuid::new_v4();
        let events = vec![ev(p, "join", Some("en"), 0)];
        let m = finalize_webinar_metrics(&events, "en", "standard", t(0), t(600), std_rates());
        assert_eq!(m.translated_language_count, 0);
        assert_eq!(m.peak_viewers, 1);
        assert_eq!(m.cost_credits, 0);
    }

    #[test]
    fn distinct_foreign_langs_bill_rate_times_k() {
        // Source en; three viewers in fr, it, de for the full 10 min → K=3.
        // cost = 10 min × 0.01 × 3 = 0.30 USD = 30 credits.
        let events = vec![
            ev(Uuid::new_v4(), "join", Some("fr"), 0),
            ev(Uuid::new_v4(), "join", Some("it"), 0),
            ev(Uuid::new_v4(), "join", Some("de"), 0),
        ];
        let m = finalize_webinar_metrics(&events, "en", "standard", t(0), t(600), std_rates());
        assert_eq!(m.translated_language_count, 3);
        assert_eq!(m.peak_viewers, 3);
        assert_eq!(m.cost_credits, 30);
    }

    #[test]
    fn duplicate_foreign_lang_counts_once() {
        // Two viewers BOTH in fr → one translation stream, not two. K=1.
        // cost = 10 × 0.01 × 1 = 0.10 USD = 10 credits.
        let events = vec![
            ev(Uuid::new_v4(), "join", Some("fr"), 0),
            ev(Uuid::new_v4(), "join", Some("fr"), 0),
        ];
        let m = finalize_webinar_metrics(&events, "en", "standard", t(0), t(600), std_rates());
        assert_eq!(m.translated_language_count, 1);
        assert_eq!(m.peak_viewers, 2);
        assert_eq!(m.cost_credits, 10);
    }

    #[test]
    fn participant_changes_language_midway() {
        // 10-min run. One fr viewer for the first 5 min (K=1), then re-joins as de
        // at 5 min (a language change → K still 1 but a different language).
        // cost = 5 min × 0.01 × 1  +  5 min × 0.01 × 1 = 0.05 + 0.05 = 0.10 = 10 credits.
        // translated_language_count = 2 (fr AND de were each active).
        let p = Uuid::new_v4();
        let events = vec![
            ev(p, "join", Some("fr"), 0),
            ev(p, "leave", None, 300),
            ev(p, "join", Some("de"), 300),
        ];
        let m = finalize_webinar_metrics(&events, "en", "standard", t(0), t(600), std_rates());
        assert_eq!(m.translated_language_count, 2);
        assert_eq!(m.peak_viewers, 1);
        assert_eq!(m.cost_credits, 10);
    }

    #[test]
    fn join_leave_overlaps_change_k_over_time() {
        // 6-min run, source en.
        //  [0,2): fr present        → K=1
        //  [2,4): fr + it present   → K=2  (it joins at 2)
        //  [4,6): it present        → K=1  (fr leaves at 4)
        let fr = Uuid::new_v4();
        let it = Uuid::new_v4();
        let events = vec![
            ev(fr, "join", Some("fr"), 0),
            ev(it, "join", Some("it"), 120),
            ev(fr, "leave", None, 240),
        ];
        // cost = 2min×0.01×1 + 2min×0.01×2 + 2min×0.01×1
        //      = 0.02 + 0.04 + 0.02 = 0.08 USD = 8 credits.
        let m = finalize_webinar_metrics(&events, "en", "standard", t(0), t(360), std_rates());
        assert_eq!(m.translated_language_count, 2);
        assert_eq!(m.peak_viewers, 2);
        assert_eq!(m.cost_credits, 8);
    }

    #[test]
    fn enhanced_tier_bills_enhanced_rate_times_k() {
        // Enhanced resolves to a higher per-minute rate (no separate TTS adder).
        // rate 0.03/min, source en, one fr viewer, 10 min → K=1.
        // cost = 10 × 0.03 × 1 = 0.30 USD = 30 credits.
        let rates = WebinarRates::enhanced(0.03);
        let events = vec![ev(Uuid::new_v4(), "join", Some("fr"), 0)];
        let m = finalize_webinar_metrics(&events, "en", "enhanced", t(0), t(600), rates);
        assert_eq!(m.cost_credits, 30);
    }

    #[test]
    fn enhanced_no_viewers_costs_nothing() {
        // Enhanced, nobody watching. K=0 → meter skips every interval → cost 0.
        let rates = WebinarRates::enhanced(0.03);
        let m = finalize_webinar_metrics(&[], "en", "enhanced", t(0), t(600), rates);
        assert_eq!(m.cost_credits, 0);
    }

    #[test]
    fn enhanced_two_foreign_langs_scale_by_k() {
        // Enhanced rate 0.03/min, source en, viewers fr + de for 10 min → K=2.
        // cost = 10 × 0.03 × 2 = 0.60 USD = 60 credits (rate × K, no TTS adder).
        let rates = WebinarRates::enhanced(0.03);
        let events = vec![
            ev(Uuid::new_v4(), "join", Some("fr"), 0),
            ev(Uuid::new_v4(), "join", Some("de"), 0),
        ];
        let m = finalize_webinar_metrics(&events, "en", "enhanced", t(0), t(600), rates);
        assert_eq!(m.cost_credits, 60);
    }

    #[test]
    fn zero_length_broadcast_costs_nothing() {
        // actual_end == actual_start (or before): nothing to bill.
        let m = finalize_webinar_metrics(&[], "en", "standard", t(100), t(100), std_rates());
        assert_eq!(m.duration_seconds, 0);
        assert_eq!(m.cost_credits, 0);
        // Defensive: end before start clamps to 0, no negative cost.
        let m2 = finalize_webinar_metrics(&[], "en", "standard", t(100), t(50), std_rates());
        assert_eq!(m2.duration_seconds, 0);
        assert_eq!(m2.cost_credits, 0);
    }

    #[test]
    fn events_outside_the_span_are_ignored() {
        // A stray join well before actual_start whose participant LEFT before go-live,
        // and a leave after actual_end, must not affect the [start,end] integration.
        // Here the participant joins at 10 then leaves at 40 — both before start(100) —
        // so at go-live they are NOT present → seeded set empty → K=0 → cost 0.
        let p = Uuid::new_v4();
        let events = vec![ev(p, "join", Some("fr"), 10), ev(p, "leave", None, 40)];
        let m = finalize_webinar_metrics(&events, "en", "standard", t(100), t(700), std_rates());
        assert_eq!(m.cost_credits, 0);
        assert_eq!(m.peak_viewers, 0);
    }

    #[test]
    fn present_at_go_live_is_billed_from_first_interval() {
        // FIX 2: a foreign-language viewer joined the WAITING ROOM before go-live
        // (join at 50, before actual_start=100) and leaves inside the window (at 400).
        // They must be counted in K for the initial interval [100,400): K=1.
        //  [100,400): fr present (seeded) → K=1  → 5 min × 0.01 × 1 = 0.05
        //  [400,700): empty               → K=0  → 0
        // cost = 0.05 USD = 5 credits.
        let p = Uuid::new_v4();
        let events = vec![
            ev(p, "join", Some("fr"), 50), // waiting room, before go-live
            ev(p, "leave", None, 400),     // leaves inside the window
        ];
        let m = finalize_webinar_metrics(&events, "en", "standard", t(100), t(700), std_rates());
        assert_eq!(m.translated_language_count, 1);
        assert_eq!(m.peak_viewers, 1);
        assert_eq!(m.cost_credits, 5);
    }

    #[test]
    fn ceil_rounds_fractional_credits_up() {
        // 90-second (1.5 min) Standard broadcast, one fr viewer → K=1.
        // cost = 1.5 × 0.01 × 1 = 0.015 USD = 1.5 credits → ceil → 2.
        let events = vec![ev(Uuid::new_v4(), "join", Some("fr"), 0)];
        let m = finalize_webinar_metrics(&events, "en", "standard", t(0), t(90), std_rates());
        assert_eq!(m.duration_seconds, 90);
        assert_eq!(m.cost_credits, 2);
    }

    #[test]
    fn same_language_viewer_among_foreign_ones_is_not_counted() {
        // Source en. Viewers: en (same), fr, it. Only fr+it are billable → K=2.
        // Explicitly covers the "minus 1 for same-language" rule alongside foreign.
        let events = vec![
            ev(Uuid::new_v4(), "join", Some("en"), 0),
            ev(Uuid::new_v4(), "join", Some("fr"), 0),
            ev(Uuid::new_v4(), "join", Some("it"), 0),
        ];
        let m = finalize_webinar_metrics(&events, "en", "standard", t(0), t(600), std_rates());
        assert_eq!(m.translated_language_count, 2);
        assert_eq!(m.peak_viewers, 3);
        // cost = 10 × 0.01 × 2 = 0.20 = 20 credits.
        assert_eq!(m.cost_credits, 20);
    }
}
