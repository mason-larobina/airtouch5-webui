//! Runtime configuration.
//!
//! `listen` and `discovery_timeout` come from CLI args (clap, defined in
//! `main.rs`), which themselves fall back to the `AIRTOUCH5_CONTROLLER_WEBUI_LISTEN` and
//! `AIRTOUCH5_CONTROLLER_WEBUI_DISCOVERY_TIMEOUT_MS` env vars (clap's `env` attribute). `log_level`
//! is sourced from the `AIRTOUCH5_CONTROLLER_WEBUI_LOG`/`RUST_LOG` env var so the tracing filter
//! stays environment-driven.

use std::net::SocketAddr;
use std::time::Duration;

/// Configuration for the airtouch5-controller-webui web server.
#[derive(Clone, Debug)]
pub struct Config {
    /// Address/port the HTTP server listens on.
    pub listen: SocketAddr,
    /// How long discovery waits for a console response.
    pub discovery_timeout: Duration,
    /// Tracing log level/filter.
    pub log_level: String,
}

impl Config {
    /// Build from CLI-derived values; `log_level` is read from the environment.
    pub fn new(listen: SocketAddr, discovery_timeout: Duration) -> Self {
        let log_level = std::env::var("AIRTOUCH5_CONTROLLER_WEBUI_LOG")
            .or_else(|_| std::env::var("RUST_LOG"))
            .unwrap_or_else(|_| "airtouch5_controller_webui=info,tower_http=info".to_string());
        Self {
            listen,
            discovery_timeout,
            log_level,
        }
    }
}
