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
/// Hard ceiling on one conversation.
const DEFAULT_MAX_SESSION_MIN: i64 = 120;
/// Open sessions one signed-in owner may have at once.
const DEFAULT_MAX_CONCURRENT_PER_OWNER: i64 = 3;
/// How long a dropped phone has to come back before its seat is given up.
const DEFAULT_RECONNECT_GRACE_SECS: i64 = 60;

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
    pub max_session_min: i64,
    pub max_concurrent_per_owner: i64,
    pub reconnect_grace_secs: i64,
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
            max_session_min: DEFAULT_MAX_SESSION_MIN,
            max_concurrent_per_owner: DEFAULT_MAX_CONCURRENT_PER_OWNER,
            reconnect_grace_secs: DEFAULT_RECONNECT_GRACE_SECS,
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
            max_session_min: number("LIVE_MAX_SESSION_MIN", DEFAULT_MAX_SESSION_MIN, 1, 24 * 60)?,
            max_concurrent_per_owner: number(
                "LIVE_MAX_CONCURRENT_PER_OWNER",
                DEFAULT_MAX_CONCURRENT_PER_OWNER,
                1,
                100,
            )?,
            reconnect_grace_secs: number(
                "LIVE_RECONNECT_GRACE_SECS",
                DEFAULT_RECONNECT_GRACE_SECS,
                5,
                600,
            )?,
        })
    }
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
    fn defaults_apply_and_overrides_are_range_checked() {
        let cfg = LiveConfig::load(env(&[
            ("LIVE_CONNECT_ENABLED", "on"),
            ("LIVE_TOKEN_KEY", KEY),
        ]))
        .unwrap();
        assert!(cfg.enabled && cfg.keys.is_some());
        assert_eq!(cfg.max_session_min, 120);
        assert_eq!(cfg.max_concurrent_per_owner, 3);
        assert_eq!(cfg.desk_wait_secs, 120);

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
}
