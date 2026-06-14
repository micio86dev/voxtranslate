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
    }
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
        // Every metric is preceded by a TYPE line (Prometheus exposition hygiene).
        assert_eq!(out.matches("# TYPE ").count(), 4);
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
