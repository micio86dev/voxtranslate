//! Prometheus metrics (spec 0058). Process-global atomic counters — HTTP request
//! totals by status class and a request-latency histogram — recorded once per
//! request by the canonical-log middleware ([`crate::observability::canonical_log`]),
//! plus live room/peer gauges read at scrape time. Everything here is a
//! **non-sensitive aggregate** (no user data, no per-request detail), so the
//! `/metrics` endpoint is safe to expose unauthenticated for a scraper.
//!
//! Rooms are in-memory per instance, so the gauges describe THIS instance — which is
//! the right granularity given the app scales vertically (see spec 0050 / issue #69).

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

/// Histogram bucket upper bounds (ms) for the `le` labels. `+Inf` is the total count.
const BUCKETS_MS: [u64; 11] = [5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000];

static REQ_2XX: AtomicU64 = AtomicU64::new(0);
static REQ_3XX: AtomicU64 = AtomicU64::new(0);
static REQ_4XX: AtomicU64 = AtomicU64::new(0);
static REQ_5XX: AtomicU64 = AtomicU64::new(0);
static LAT_SUM_MS: AtomicU64 = AtomicU64::new(0);
static LAT_COUNT: AtomicU64 = AtomicU64::new(0);
// Cumulative buckets: index i counts requests with latency <= BUCKETS_MS[i].
static LAT_BUCKETS: [AtomicU64; 11] = [const { AtomicU64::new(0) }; 11];

// Premium (Gemini Live) audio-translation latency (spec 0100). `ttfa` is the
// per-segment time from a speaker's first audio chunk reaching Gemini to the first
// translated-audio chunk back (the model's ear-voice span); `connect` is the
// per-session WS connect+setup cost. Aggregated here instead of logged per segment
// so they stay cheap at scale. Both reuse `BUCKETS_MS`.
static GEM_TTFA_SUM: AtomicU64 = AtomicU64::new(0);
static GEM_TTFA_COUNT: AtomicU64 = AtomicU64::new(0);
static GEM_TTFA_BUCKETS: [AtomicU64; 11] = [const { AtomicU64::new(0) }; 11];
static GEM_CONNECT_SUM: AtomicU64 = AtomicU64::new(0);
static GEM_CONNECT_COUNT: AtomicU64 = AtomicU64::new(0);
static GEM_CONNECT_BUCKETS: [AtomicU64; 11] = [const { AtomicU64::new(0) }; 11];
static QWEN_TTFA_SUM: AtomicU64 = AtomicU64::new(0);
static QWEN_TTFA_COUNT: AtomicU64 = AtomicU64::new(0);
static QWEN_TTFA_BUCKETS: [AtomicU64; 11] = [const { AtomicU64::new(0) }; 11];
static QWEN_CONNECT_SUM: AtomicU64 = AtomicU64::new(0);
static QWEN_CONNECT_COUNT: AtomicU64 = AtomicU64::new(0);
static QWEN_CONNECT_BUCKETS: [AtomicU64; 11] = [const { AtomicU64::new(0) }; 11];

/// Add one observation (ms) to a cumulative-bucket histogram.
fn observe(sum: &AtomicU64, count: &AtomicU64, buckets: &[AtomicU64; 11], ms: u64) {
    sum.fetch_add(ms, Ordering::Relaxed);
    count.fetch_add(1, Ordering::Relaxed);
    for (i, &le) in BUCKETS_MS.iter().enumerate() {
        if ms <= le {
            buckets[i].fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Record one Premium segment's time-to-first-translated-audio (ms).
pub fn record_gemini_ttfa(ms: u64) {
    observe(&GEM_TTFA_SUM, &GEM_TTFA_COUNT, &GEM_TTFA_BUCKETS, ms);
}

/// Record one Gemini Live session connect+setup duration (ms).
pub fn record_gemini_connect(ms: u64) {
    observe(
        &GEM_CONNECT_SUM,
        &GEM_CONNECT_COUNT,
        &GEM_CONNECT_BUCKETS,
        ms,
    );
}

/// Record one Standard segment's time-to-first-translated-audio (ms). Standard is the
/// default tier, so this histogram — not the Gemini one — is the app's headline
/// ear-voice-span SLI.
pub fn record_qwen_ttfa(ms: u64) {
    observe(&QWEN_TTFA_SUM, &QWEN_TTFA_COUNT, &QWEN_TTFA_BUCKETS, ms);
}

/// Record one Qwen realtime session connect+`session.update` duration (ms).
pub fn record_qwen_connect(ms: u64) {
    observe(
        &QWEN_CONNECT_SUM,
        &QWEN_CONNECT_COUNT,
        &QWEN_CONNECT_BUCKETS,
        ms,
    );
}

/// Record one completed request. Called from the canonical-log middleware, which
/// already has the final status + wall latency, so there's no extra timing cost.
pub fn record_request(status: u16, latency_ms: u64) {
    match status / 100 {
        2 => {
            REQ_2XX.fetch_add(1, Ordering::Relaxed);
        }
        3 => {
            REQ_3XX.fetch_add(1, Ordering::Relaxed);
        }
        4 => {
            REQ_4XX.fetch_add(1, Ordering::Relaxed);
        }
        5 => {
            REQ_5XX.fetch_add(1, Ordering::Relaxed);
        }
        _ => {} // 1xx / unknown: count toward latency only, not a status class
    }
    LAT_SUM_MS.fetch_add(latency_ms, Ordering::Relaxed);
    LAT_COUNT.fetch_add(1, Ordering::Relaxed);
    for (i, &le) in BUCKETS_MS.iter().enumerate() {
        if latency_ms <= le {
            LAT_BUCKETS[i].fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Point-in-time copy of the global counters — lets [`render`] stay pure + testable.
struct Snapshot {
    req: [(&'static str, u64); 4],
    lat_sum_ms: u64,
    lat_count: u64,
    lat_buckets: [u64; 11],
    gem_ttfa_sum: u64,
    gem_ttfa_count: u64,
    gem_ttfa_buckets: [u64; 11],
    gem_connect_sum: u64,
    gem_connect_count: u64,
    gem_connect_buckets: [u64; 11],
    qwen_ttfa_sum: u64,
    qwen_ttfa_count: u64,
    qwen_ttfa_buckets: [u64; 11],
    qwen_connect_sum: u64,
    qwen_connect_count: u64,
    qwen_connect_buckets: [u64; 11],
}

fn snapshot() -> Snapshot {
    Snapshot {
        req: [
            ("2xx", REQ_2XX.load(Ordering::Relaxed)),
            ("3xx", REQ_3XX.load(Ordering::Relaxed)),
            ("4xx", REQ_4XX.load(Ordering::Relaxed)),
            ("5xx", REQ_5XX.load(Ordering::Relaxed)),
        ],
        lat_sum_ms: LAT_SUM_MS.load(Ordering::Relaxed),
        lat_count: LAT_COUNT.load(Ordering::Relaxed),
        lat_buckets: std::array::from_fn(|i| LAT_BUCKETS[i].load(Ordering::Relaxed)),
        gem_ttfa_sum: GEM_TTFA_SUM.load(Ordering::Relaxed),
        gem_ttfa_count: GEM_TTFA_COUNT.load(Ordering::Relaxed),
        gem_ttfa_buckets: std::array::from_fn(|i| GEM_TTFA_BUCKETS[i].load(Ordering::Relaxed)),
        gem_connect_sum: GEM_CONNECT_SUM.load(Ordering::Relaxed),
        gem_connect_count: GEM_CONNECT_COUNT.load(Ordering::Relaxed),
        gem_connect_buckets: std::array::from_fn(|i| {
            GEM_CONNECT_BUCKETS[i].load(Ordering::Relaxed)
        }),
        qwen_ttfa_sum: QWEN_TTFA_SUM.load(Ordering::Relaxed),
        qwen_ttfa_count: QWEN_TTFA_COUNT.load(Ordering::Relaxed),
        qwen_ttfa_buckets: std::array::from_fn(|i| QWEN_TTFA_BUCKETS[i].load(Ordering::Relaxed)),
        qwen_connect_sum: QWEN_CONNECT_SUM.load(Ordering::Relaxed),
        qwen_connect_count: QWEN_CONNECT_COUNT.load(Ordering::Relaxed),
        qwen_connect_buckets: std::array::from_fn(|i| {
            QWEN_CONNECT_BUCKETS[i].load(Ordering::Relaxed)
        }),
    }
}

/// Write one cumulative-bucket histogram in Prometheus text format.
fn write_histogram(
    o: &mut String,
    name: &str,
    help: &str,
    sum: u64,
    count: u64,
    buckets: &[u64; 11],
) {
    let _ = writeln!(o, "# HELP {name} {help}");
    let _ = writeln!(o, "# TYPE {name} histogram");
    for (i, le) in BUCKETS_MS.iter().enumerate() {
        let _ = writeln!(o, "{name}_bucket{{le=\"{le}\"}} {}", buckets[i]);
    }
    let _ = writeln!(o, "{name}_bucket{{le=\"+Inf\"}} {count}");
    let _ = writeln!(o, "{name}_sum {sum}");
    let _ = writeln!(o, "{name}_count {count}");
}

/// Render the Prometheus text-format exposition for the current snapshot plus the
/// live `active_rooms` / `active_peers` gauges (read from the room registry).
pub fn render(active_rooms: u64, active_peers: u64) -> String {
    render_from(&snapshot(), active_rooms, active_peers)
}

fn render_from(s: &Snapshot, active_rooms: u64, active_peers: u64) -> String {
    let mut o = String::with_capacity(1024);

    let _ = writeln!(
        o,
        "# HELP voxtranslate_http_requests_total Total HTTP requests by status class."
    );
    let _ = writeln!(o, "# TYPE voxtranslate_http_requests_total counter");
    for (class, n) in s.req {
        let _ = writeln!(
            o,
            "voxtranslate_http_requests_total{{status_class=\"{class}\"}} {n}"
        );
    }

    let _ = writeln!(
        o,
        "# HELP voxtranslate_http_request_duration_ms Request latency in milliseconds."
    );
    let _ = writeln!(o, "# TYPE voxtranslate_http_request_duration_ms histogram");
    for (i, le) in BUCKETS_MS.iter().enumerate() {
        let _ = writeln!(
            o,
            "voxtranslate_http_request_duration_ms_bucket{{le=\"{le}\"}} {}",
            s.lat_buckets[i]
        );
    }
    let _ = writeln!(
        o,
        "voxtranslate_http_request_duration_ms_bucket{{le=\"+Inf\"}} {}",
        s.lat_count
    );
    let _ = writeln!(
        o,
        "voxtranslate_http_request_duration_ms_sum {}",
        s.lat_sum_ms
    );
    let _ = writeln!(
        o,
        "voxtranslate_http_request_duration_ms_count {}",
        s.lat_count
    );

    let _ = writeln!(
        o,
        "# HELP voxtranslate_active_rooms Rooms with at least one peer on this instance."
    );
    let _ = writeln!(o, "# TYPE voxtranslate_active_rooms gauge");
    let _ = writeln!(o, "voxtranslate_active_rooms {active_rooms}");

    let _ = writeln!(
        o,
        "# HELP voxtranslate_active_peers Connected peers across all rooms on this instance."
    );
    let _ = writeln!(o, "# TYPE voxtranslate_active_peers gauge");
    let _ = writeln!(o, "voxtranslate_active_peers {active_peers}");

    write_histogram(
        &mut o,
        "voxtranslate_gemini_ttfa_ms",
        "Premium per-segment time-to-first-translated-audio (Gemini ear-voice span), ms.",
        s.gem_ttfa_sum,
        s.gem_ttfa_count,
        &s.gem_ttfa_buckets,
    );
    write_histogram(
        &mut o,
        "voxtranslate_gemini_connect_ms",
        "Gemini Live session connect+setup duration, ms.",
        s.gem_connect_sum,
        s.gem_connect_count,
        &s.gem_connect_buckets,
    );
    write_histogram(
        &mut o,
        "voxtranslate_qwen_ttfa_ms",
        "Standard per-segment time-to-first-translated-audio (Qwen ear-voice span), ms.",
        s.qwen_ttfa_sum,
        s.qwen_ttfa_count,
        &s.qwen_ttfa_buckets,
    );
    write_histogram(
        &mut o,
        "voxtranslate_qwen_connect_ms",
        "Qwen realtime session connect+session.update duration, ms.",
        s.qwen_connect_sum,
        s.qwen_connect_count,
        &s.qwen_connect_buckets,
    );

    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_emits_valid_prometheus_exposition() {
        let snap = Snapshot {
            req: [("2xx", 10), ("3xx", 0), ("4xx", 3), ("5xx", 1)],
            lat_sum_ms: 420,
            lat_count: 14,
            // cumulative: <=5ms:2, <=10ms:5, … <=10000ms:14
            lat_buckets: [2, 5, 9, 11, 12, 13, 14, 14, 14, 14, 14],
            // ttfa: 4 segments, one cold (>1000ms) + three warm (<=100ms).
            gem_ttfa_sum: 2200,
            gem_ttfa_count: 4,
            gem_ttfa_buckets: [1, 2, 3, 3, 3, 3, 3, 3, 4, 4, 4],
            gem_connect_sum: 180,
            gem_connect_count: 2,
            gem_connect_buckets: [0, 0, 0, 0, 1, 2, 2, 2, 2, 2, 2],
            // Standard/Qwen: 3 segments, all warm.
            qwen_ttfa_sum: 900,
            qwen_ttfa_count: 3,
            qwen_ttfa_buckets: [0, 0, 1, 2, 3, 3, 3, 3, 3, 3, 3],
            qwen_connect_sum: 240,
            qwen_connect_count: 2,
            qwen_connect_buckets: [0, 0, 0, 0, 0, 2, 2, 2, 2, 2, 2],
        };
        let out = render_from(&snap, 2, 5);

        // Status-class counters.
        assert!(out.contains("voxtranslate_http_requests_total{status_class=\"2xx\"} 10"));
        assert!(out.contains("voxtranslate_http_requests_total{status_class=\"4xx\"} 3"));
        // Histogram: a finite bucket, the +Inf bucket == count, and sum/count.
        assert!(out.contains("voxtranslate_http_request_duration_ms_bucket{le=\"50\"} 11"));
        assert!(out.contains("voxtranslate_http_request_duration_ms_bucket{le=\"+Inf\"} 14"));
        assert!(out.contains("voxtranslate_http_request_duration_ms_sum 420"));
        assert!(out.contains("voxtranslate_http_request_duration_ms_count 14"));
        // Gauges.
        assert!(out.contains("voxtranslate_active_rooms 2"));
        assert!(out.contains("voxtranslate_active_peers 5"));
        // Gemini latency histograms.
        assert!(out.contains("voxtranslate_gemini_ttfa_ms_bucket{le=\"+Inf\"} 4"));
        assert!(out.contains("voxtranslate_gemini_ttfa_ms_sum 2200"));
        assert!(out.contains("voxtranslate_gemini_ttfa_ms_count 4"));
        assert!(out.contains("voxtranslate_gemini_connect_ms_bucket{le=\"100\"} 1"));
        assert!(out.contains("voxtranslate_gemini_connect_ms_count 2"));
        // Qwen (Standard tier) latency histograms — the default-tier SLI.
        assert!(out.contains("voxtranslate_qwen_ttfa_ms_bucket{le=\"+Inf\"} 3"));
        assert!(out.contains("voxtranslate_qwen_ttfa_ms_sum 900"));
        assert!(out.contains("voxtranslate_qwen_ttfa_ms_count 3"));
        assert!(out.contains("voxtranslate_qwen_connect_ms_count 2"));
        // Every metric is preceded by a TYPE line (Prometheus exposition hygiene).
        assert_eq!(out.matches("# TYPE ").count(), 8);
    }

    #[test]
    fn record_gemini_helpers_bucket_observations() {
        let before = snapshot();
        record_gemini_ttfa(2000); // cold segment
        record_gemini_ttfa(40); // warm segment
        record_gemini_connect(60);
        let after = snapshot();

        assert_eq!(after.gem_ttfa_count - before.gem_ttfa_count, 2);
        assert_eq!(after.gem_ttfa_sum - before.gem_ttfa_sum, 2040);
        // The 40ms ttfa lands in le=50 (index 3) but not le=25 (index 2).
        assert_eq!(after.gem_ttfa_buckets[3] - before.gem_ttfa_buckets[3], 1);
        assert_eq!(after.gem_connect_count - before.gem_connect_count, 1);
        assert_eq!(after.gem_connect_sum - before.gem_connect_sum, 60);
    }

    #[test]
    fn record_request_classifies_and_buckets() {
        // Uses process-global counters; assert on deltas so it's order-independent.
        let before = snapshot();
        record_request(204, 7);
        record_request(404, 30);
        record_request(500, 9000);
        let after = snapshot();

        assert_eq!(after.req[0].1 - before.req[0].1, 1); // one 2xx
        assert_eq!(after.req[2].1 - before.req[2].1, 1); // one 4xx
        assert_eq!(after.req[3].1 - before.req[3].1, 1); // one 5xx
        assert_eq!(after.lat_count - before.lat_count, 3);
        assert_eq!(after.lat_sum_ms - before.lat_sum_ms, 7 + 30 + 9000);
        // The 7ms request falls in every bucket from le=10 up (not le=5).
        assert_eq!(after.lat_buckets[1] - before.lat_buckets[1], 1); // le=10
    }
}
