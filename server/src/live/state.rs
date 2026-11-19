//! Shared Live Connect state, carried on `AppState`.

use super::config::LiveConfig;

/// Configuration today; the in-memory presence registry joins it with the WebSocket handler.
#[derive(Debug, Clone)]
pub struct LiveState {
    pub config: LiveConfig,
}

impl LiveState {
    pub fn new(config: LiveConfig) -> Self {
        Self { config }
    }

    /// Read the process environment. A bad configuration leaves the feature **off** and says
    /// why at ERROR level: failing the whole server's boot over an optional feature would be
    /// the worse trade, but silently running it with a weakened key would be worse still.
    pub fn from_env() -> Self {
        match LiveConfig::load(|k| std::env::var(k).ok()) {
            Ok(config) => {
                if config.enabled {
                    tracing::info!("live connect enabled");
                }
                Self::new(config)
            }
            Err(e) => {
                tracing::error!(error = %e, "live connect misconfigured — feature stays off");
                Self::new(LiveConfig::disabled())
            }
        }
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled && self.config.keys.is_some()
    }
}
