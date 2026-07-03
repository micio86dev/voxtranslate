//! A tiny fixed-window rate limiter backed by a `DashMap`.
//!
//! Used to throttle the auth and checkout endpoints. Fixed-window (not a
//! sliding window) is intentionally simple: each key gets `max` calls per
//! `window`; the counter resets when the window elapses.
//!
//! SCALING CAVEAT (M6): counters are per-process/in-memory. This is correct for a
//! single instance (the current Railway topology). If the server is ever run with
//! more than one replica behind a load balancer, each replica keeps its own counters,
//! so a caller's effective limit becomes `max × replica_count`. Before scaling out,
//! back this with the shared DragonflyDB (see `crate::cache`) using atomic
//! `INCR`+`EXPIRE` so limits are global across instances.

use std::time::{Duration, Instant};

use dashmap::DashMap;

/// Per-key call counters with their window start instants.
#[derive(Default)]
pub struct RateLimiter {
    hits: DashMap<String, (u32, Instant)>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a hit for `key`; return `true` if it's within the limit, `false`
    /// if the caller should be throttled.
    pub fn allow(&self, key: &str, max: u32, window: Duration) -> bool {
        let now = Instant::now();
        // Bound memory (spec 0029): one-off keys (per-uuid, per-IP) would otherwise
        // accumulate forever. When the map grows large, drop fully-elapsed entries.
        if self.hits.len() > 10_000 {
            self.hits.retain(|_, v| now.duration_since(v.1) <= window);
        }
        let mut entry = self.hits.entry(key.to_string()).or_insert((0, now));
        let (count, start) = *entry;
        if now.duration_since(start) > window {
            *entry = (1, now); // window elapsed — reset
            true
        } else if count < max {
            entry.0 = count + 1;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_max_then_blocks() {
        let rl = RateLimiter::new();
        let window = Duration::from_secs(60);
        assert!(rl.allow("k", 3, window));
        assert!(rl.allow("k", 3, window));
        assert!(rl.allow("k", 3, window));
        assert!(!rl.allow("k", 3, window)); // 4th in-window is blocked
                                            // A different key is independent.
        assert!(rl.allow("other", 3, window));
    }

    #[test]
    fn window_reset_allows_again() {
        let rl = RateLimiter::new();
        let window = Duration::from_millis(50);
        assert!(rl.allow("k", 1, window));
        assert!(!rl.allow("k", 1, window));
        std::thread::sleep(Duration::from_millis(70));
        assert!(rl.allow("k", 1, window)); // window elapsed
    }
}
