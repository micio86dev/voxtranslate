//! Live Connect configuration.
//!
//! Read through a lookup function rather than straight from the environment, so every rule
//! here is testable without mutating process-global state (the reason `tests/config_env.rs`
//! needs its own binary and a mutex).
//!
//! The feature is **off unless** `LIVE_CONNECT_ENABLED` is set *and* the token key is valid.
//! A key that is missing or too short is an error, not a silent downgrade to a weaker key:
//! the credentials it protects admit strangers into a conversation somebody else pays for.

use super::tokens::{KeyError, LiveKeys};

/// Seconds a B2C invitation stays redeemable before anyone has joined.
const DEFAULT_JOIN_WINDOW_SECS: i64 = 15 * 60;
/// Seconds a scanned desk session waits for staff before it expires.
const DEFAULT_DESK_WAIT_SECS: i64 = 120;
/// Sessions that may be waiting at one desk at once.
const DEFAULT_DESK_MAX_WAITING: i64 = 20;
/// Of those, how many one client address may hold at one desk. A printed QR is public: without
/// this, one phone could fill the whole queue and lock the real customers out for the wait window.
const DEFAULT_DESK_MAX_WAITING_PER_IP: i64 = 2;
/// Bytes of microphone audio per second one seat may feed the engine: 1.5x real time for PCM16
/// mono at 24 kHz (48 000 B/s). Billing is by the wall clock, so audio pushed faster than it
/// plays would be translated, and billed by the provider, for less than it costs the payer.
const DEFAULT_AUDIO_MAX_BYTES_PER_SEC: i64 = 72_000;
/// Hard ceiling on one conversation.
const DEFAULT_MAX_SESSION_MIN: i64 = 120;
/// Open sessions one signed-in owner may have at once.
const DEFAULT_MAX_CONCURRENT_PER_OWNER: i64 = 3;
/// Guests one invitation may admit, at most. A room holds four peers (`rooms::MAX_PEERS`): the
/// host plus three guests. The hard ceiling; `LIVE_MAX_GUESTS` can only lower it.
pub const MAX_GUESTS_CEILING: i64 = 3;
/// Default for `LIVE_MAX_GUESTS`: what a host may ask for at most.
const DEFAULT_MAX_GUESTS: i64 = MAX_GUESTS_CEILING;
/// How long a dropped phone has to come back before its seat is given up.
const DEFAULT_RECONNECT_GRACE_SECS: i64 = 60;
/// Days an ended business conversation's content is kept when its org sets no `retention_days`.
/// `0` switches that default off (such an org keeps its conversations until it deletes them).
const DEFAULT_RETENTION_DAYS: i64 = 90;
/// Days the transcript of an ended consumer conversation is kept (only ever stored on opt-in).
/// `0` keeps them until the owner deletes them.
const DEFAULT_B2C_RETENTION_DAYS: i64 = 30;
/// Seconds to wait after a conversation ends before its transcript is assembled, so the
/// engines' last finals have reached the recorder.
const DEFAULT_FINALIZE_GRACE_SECS: i64 = 5;
/// The echo filter (see `echo.rs`) is on unless `LIVE_ECHO_FILTER` says otherwise.
const DEFAULT_ECHO_FILTER: bool = true;
/// How alike the other phone's "original" words must be to this phone's own recent words before
/// they are treated as an echo of them.
const DEFAULT_ECHO_SIM: f64 = 0.6;
/// A threshold under this suppresses real replies that merely share a few words; over this it
/// stops catching an echo the ASR heard through a foreign-language ear.
const ECHO_SIM_RANGE: (f64, f64) = (0.3, 0.95);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// `LIVE_CONNECT_ENABLED` is on but `LIVE_TOKEN_KEY` is absent.
    MissingKey,
    /// The key exists but is not usable.
    BadKey(KeyError),
    /// A numeric setting is not a positive integer inside its allowed range.
    BadNumber(&'static str),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingKey => write!(
                f,
                "LIVE_CONNECT_ENABLED is set but LIVE_TOKEN_KEY is missing"
            ),
            Self::BadKey(e) => write!(f, "LIVE_TOKEN_KEY is unusable: {e}"),
            Self::BadNumber(name) => write!(f, "{name} must be a positive integer within range"),
        }
    }
}

impl std::error::Error for ConfigError {}

#[derive(Debug, Clone)]
pub struct LiveConfig {
    pub enabled: bool,
    pub keys: Option<LiveKeys>,
    pub join_window_secs: i64,
    pub desk_wait_secs: i64,
    pub desk_max_waiting: i64,
    pub desk_max_waiting_per_ip: i64,
    /// Per-seat microphone budget in bytes per second (also the burst it may save up).
    pub audio_max_bytes_per_sec: i64,
    pub max_session_min: i64,
    pub max_concurrent_per_owner: i64,
    /// Most guests a host may ask one invitation to admit (1..=[`MAX_GUESTS_CEILING`]). A session
    /// created without `max_guests` still takes one.
    pub max_guests: i64,
    pub reconnect_grace_secs: i64,
    /// Retention for business conversations whose org has no `retention_days` of its own.
    pub retention_default_days: i64,
    /// Retention for opted-in consumer transcripts.
    pub b2c_retention_days: i64,
    pub finalize_grace_secs: i64,
    /// Drop the translation of one phone's own voice that the OTHER phone's microphone picked up.
    pub echo_filter: bool,
    /// Similarity threshold of that filter, already clamped to its allowed range.
    pub echo_sim: f64,
}

impl LiveConfig {
    /// The feature fully off. What the server runs with when the flag is unset or the
    /// configuration is rejected.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            keys: None,
            join_window_secs: DEFAULT_JOIN_WINDOW_SECS,
            desk_wait_secs: DEFAULT_DESK_WAIT_SECS,
            desk_max_waiting: DEFAULT_DESK_MAX_WAITING,
            desk_max_waiting_per_ip: DEFAULT_DESK_MAX_WAITING_PER_IP,
            audio_max_bytes_per_sec: DEFAULT_AUDIO_MAX_BYTES_PER_SEC,
            max_session_min: DEFAULT_MAX_SESSION_MIN,
            max_concurrent_per_owner: DEFAULT_MAX_CONCURRENT_PER_OWNER,
            max_guests: DEFAULT_MAX_GUESTS,
            reconnect_grace_secs: DEFAULT_RECONNECT_GRACE_SECS,
            retention_default_days: DEFAULT_RETENTION_DAYS,
            b2c_retention_days: DEFAULT_B2C_RETENTION_DAYS,
            finalize_grace_secs: DEFAULT_FINALIZE_GRACE_SECS,
            echo_filter: DEFAULT_ECHO_FILTER,
            echo_sim: DEFAULT_ECHO_SIM,
        }
    }

    /// Load from any key/value source (`|k| std::env::var(k).ok()` in production).
    pub fn load(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let enabled = get("LIVE_CONNECT_ENABLED").is_some_and(|v| is_truthy(&v));
        if !enabled {
            return Ok(Self::disabled());
        }

        let secret = get("LIVE_TOKEN_KEY").ok_or(ConfigError::MissingKey)?;
        let keys = LiveKeys::from_secret(&secret).map_err(ConfigError::BadKey)?;

        let number =
            |name: &'static str, default: i64, min: i64, max: i64| -> Result<i64, ConfigError> {
                match get(name) {
                    None => Ok(default),
                    Some(raw) => raw
                        .trim()
                        .parse::<i64>()
                        .ok()
                        .filter(|n| (min..=max).contains(n))
                        .ok_or(ConfigError::BadNumber(name)),
                }
            };

        Ok(Self {
            enabled: true,
            keys: Some(keys),
            join_window_secs: number(
                "LIVE_JOIN_WINDOW_SECS",
                DEFAULT_JOIN_WINDOW_SECS,
                60,
                24 * 3600,
            )?,
            desk_wait_secs: number("LIVE_DESK_WAIT_SECS", DEFAULT_DESK_WAIT_SECS, 10, 3600)?,
            desk_max_waiting: number("LIVE_DESK_MAX_WAITING", DEFAULT_DESK_MAX_WAITING, 1, 200)?,
            desk_max_waiting_per_ip: number(
                "LIVE_DESK_MAX_WAITING_PER_IP",
                DEFAULT_DESK_MAX_WAITING_PER_IP,
                1,
                200,
            )?,
            audio_max_bytes_per_sec: number(
                "LIVE_AUDIO_MAX_BYTES_PER_SEC",
                DEFAULT_AUDIO_MAX_BYTES_PER_SEC,
                24_000,
                1_000_000,
            )?,
            max_session_min: number("LIVE_MAX_SESSION_MIN", DEFAULT_MAX_SESSION_MIN, 1, 24 * 60)?,
            max_concurrent_per_owner: number(
                "LIVE_MAX_CONCURRENT_PER_OWNER",
                DEFAULT_MAX_CONCURRENT_PER_OWNER,
                1,
                100,
            )?,
            // A capacity knob must never take the feature down either: junk falls back to the
            // default and a value outside 1..=3 is clamped (a room cannot hold more than four).
            max_guests: get("LIVE_MAX_GUESTS")
                .and_then(|v| v.trim().parse::<i64>().ok())
                .map_or(DEFAULT_MAX_GUESTS, |n| n.clamp(1, MAX_GUESTS_CEILING)),
            reconnect_grace_secs: number(
                "LIVE_RECONNECT_GRACE_SECS",
                DEFAULT_RECONNECT_GRACE_SECS,
                5,
                600,
            )?,
            // 0 is a valid value here (switches retention off), so the floor is 0.
            retention_default_days: number(
                "LIVE_RETENTION_DEFAULT_DAYS",
                DEFAULT_RETENTION_DAYS,
                0,
                3650,
            )?,
            b2c_retention_days: number(
                "LIVE_B2C_RETENTION_DAYS",
                DEFAULT_B2C_RETENTION_DAYS,
                0,
                3650,
            )?,
            finalize_grace_secs: number(
                "LIVE_FINALIZE_GRACE_SECS",
                DEFAULT_FINALIZE_GRACE_SECS,
                0,
                60,
            )?,
            echo_filter: get("LIVE_ECHO_FILTER").map_or(DEFAULT_ECHO_FILTER, |v| !is_falsy(&v)),
            // A tuning knob must never take the whole feature down: junk falls back to the
            // default and an out-of-range value is clamped, unlike the limits above.
            echo_sim: get("LIVE_ECHO_SIM")
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|n| n.is_finite())
                .map_or(DEFAULT_ECHO_SIM, |n| {
                    n.clamp(ECHO_SIM_RANGE.0, ECHO_SIM_RANGE.1)
                }),
        })
    }
}

fn is_falsy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

fn is_truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const KEY: &str = "0123456789abcdef0123456789abcdef-live-test";

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn unset_flag_means_disabled_and_needs_no_key() {
        let cfg = LiveConfig::load(env(&[])).unwrap();
        assert!(!cfg.enabled && cfg.keys.is_none());
    }

    #[test]
    fn falsy_flag_means_disabled_even_with_a_key() {
        let cfg = LiveConfig::load(env(&[
            ("LIVE_CONNECT_ENABLED", "0"),
            ("LIVE_TOKEN_KEY", KEY),
        ]))
        .unwrap();
        assert!(!cfg.enabled && cfg.keys.is_none());
    }

    #[test]
    fn enabled_without_a_key_is_an_error_not_a_silent_default() {
        let err = LiveConfig::load(env(&[("LIVE_CONNECT_ENABLED", "1")])).unwrap_err();
        assert_eq!(err, ConfigError::MissingKey);
    }

    #[test]
    fn a_short_key_is_rejected() {
        let err = LiveConfig::load(env(&[
            ("LIVE_CONNECT_ENABLED", "true"),
            ("LIVE_TOKEN_KEY", "short"),
        ]))
        .unwrap_err();
        assert!(matches!(err, ConfigError::BadKey(_)));
    }

    #[test]
    fn the_guest_limit_is_clamped_to_what_a_room_holds_and_junk_falls_back() {
        let load = |v: &str| {
            LiveConfig::load(env(&[
                ("LIVE_CONNECT_ENABLED", "1"),
                ("LIVE_TOKEN_KEY", KEY),
                ("LIVE_MAX_GUESTS", v),
            ]))
            .unwrap()
            .max_guests
        };
        assert_eq!(load("2"), 2);
        assert_eq!(load(" 1 "), 1);
        // Out of range is clamped, never an error and never above what a room holds.
        assert_eq!((load("0"), load("-4"), load("4"), load("99")), (1, 1, 3, 3));
        // Junk keeps the default instead of switching the feature off.
        assert_eq!((load("many"), load("")), (3, 3));
    }

    #[test]
    fn defaults_apply_and_overrides_are_range_checked() {
        let cfg = LiveConfig::load(env(&[
            ("LIVE_CONNECT_ENABLED", "on"),
            ("LIVE_TOKEN_KEY", KEY),
        ]))
        .unwrap();
        assert!(cfg.enabled && cfg.keys.is_some());
        assert_eq!(cfg.max_session_min, 120);
        assert_eq!(cfg.max_concurrent_per_owner, 3);
        assert_eq!(cfg.max_guests, 3);
        assert_eq!(cfg.desk_wait_secs, 120);
        assert_eq!(cfg.desk_max_waiting_per_ip, 2);
        assert_eq!(cfg.audio_max_bytes_per_sec, 72_000);

        let cfg = LiveConfig::load(env(&[
            ("LIVE_CONNECT_ENABLED", "1"),
            ("LIVE_TOKEN_KEY", KEY),
            ("LIVE_MAX_SESSION_MIN", "45"),
        ]))
        .unwrap();
        assert_eq!(cfg.max_session_min, 45);

        for bad in ["0", "-3", "abc", "999999"] {
            let err = LiveConfig::load(env(&[
                ("LIVE_CONNECT_ENABLED", "1"),
                ("LIVE_TOKEN_KEY", KEY),
                ("LIVE_MAX_SESSION_MIN", bad),
            ]))
            .unwrap_err();
            assert_eq!(err, ConfigError::BadNumber("LIVE_MAX_SESSION_MIN"), "{bad}");
        }
    }

    #[test]
    fn retention_defaults_apply_zero_is_allowed_and_junk_is_rejected() {
        let base = [("LIVE_CONNECT_ENABLED", "1"), ("LIVE_TOKEN_KEY", KEY)];
        let cfg = LiveConfig::load(env(&base)).unwrap();
        assert_eq!(
            (
                cfg.retention_default_days,
                cfg.b2c_retention_days,
                cfg.finalize_grace_secs
            ),
            (90, 30, 5)
        );

        let mut zero = base.to_vec();
        zero.extend([
            ("LIVE_RETENTION_DEFAULT_DAYS", "0"),
            ("LIVE_B2C_RETENTION_DAYS", "0"),
            ("LIVE_FINALIZE_GRACE_SECS", "0"),
        ]);
        let cfg = LiveConfig::load(env(&zero)).unwrap();
        assert_eq!(
            (
                cfg.retention_default_days,
                cfg.b2c_retention_days,
                cfg.finalize_grace_secs
            ),
            (0, 0, 0),
            "0 switches retention off and the grace off"
        );

        for (name, bad) in [
            ("LIVE_RETENTION_DEFAULT_DAYS", "-1"),
            ("LIVE_RETENTION_DEFAULT_DAYS", "99999"),
            ("LIVE_B2C_RETENTION_DAYS", "abc"),
            ("LIVE_FINALIZE_GRACE_SECS", "61"),
        ] {
            let mut pairs = base.to_vec();
            pairs.push((name, bad));
            let err = LiveConfig::load(env(&pairs)).unwrap_err();
            assert_eq!(err, ConfigError::BadNumber(name), "{name}={bad}");
        }
    }

    #[test]
    fn the_echo_filter_is_on_by_default_and_both_knobs_are_forgiving() {
        let base = [("LIVE_CONNECT_ENABLED", "1"), ("LIVE_TOKEN_KEY", KEY)];
        let cfg = LiveConfig::load(env(&base)).unwrap();
        assert!(cfg.echo_filter);
        assert_eq!(cfg.echo_sim, 0.6);
        assert!(LiveConfig::disabled().echo_filter);

        let with = |pairs: &[(&'static str, &'static str)]| {
            let mut all = base.to_vec();
            all.extend_from_slice(pairs);
            LiveConfig::load(env(&all)).unwrap()
        };
        for off in ["0", "false", "No", "OFF", " off "] {
            assert!(!with(&[("LIVE_ECHO_FILTER", off)]).echo_filter, "{off}");
        }
        for on in ["1", "true", "on", "yes", "garbage"] {
            assert!(with(&[("LIVE_ECHO_FILTER", on)]).echo_filter, "{on}");
        }
        assert_eq!(with(&[("LIVE_ECHO_SIM", "0.75")]).echo_sim, 0.75);
        assert_eq!(
            with(&[("LIVE_ECHO_SIM", "0.01")]).echo_sim,
            0.3,
            "clamped up"
        );
        assert_eq!(
            with(&[("LIVE_ECHO_SIM", "7")]).echo_sim,
            0.95,
            "clamped down"
        );
        for junk in ["abc", "", "NaN", "inf"] {
            assert_eq!(with(&[("LIVE_ECHO_SIM", junk)]).echo_sim, 0.6, "{junk:?}");
        }
    }
}
