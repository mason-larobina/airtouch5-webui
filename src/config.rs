//! Runtime configuration.
//!
//! All options come from CLI args (clap, defined in `main.rs`). Logging is the
//! one env-driven option: the tracing filter is read from `RUST_LOG` in the
//! binary's tracing init, not via clap.

use std::net::SocketAddr;
use std::time::Duration;

/// Configuration for the airtouch5-controller-webui web server.
#[derive(Clone, Debug)]
pub struct Config {
    /// Address/port the HTTP server listens on.
    pub listen: SocketAddr,
    /// How long discovery waits for a console response.
    pub discovery_timeout: Duration,
}

impl Config {
    /// Build from CLI-derived values.
    pub fn new(listen: SocketAddr, discovery_timeout: Duration) -> Self {
        Self {
            listen,
            discovery_timeout,
        }
    }
}
