//! Runtime configuration.
//!
//! All options come from CLI args (clap, defined in `main.rs`). Logging is the
//! one env-driven option: the tracing filter is read from `RUST_LOG` in the
//! binary's tracing init, not via clap.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// The application's directory name, used under the XDG config dir.
pub const APP_DIR: &str = "airtouch5-webui";

/// The default state directory: `$XDG_CONFIG_HOME/airtouch5-webui` (typically
/// `~/.config/airtouch5-webui`), where the automation and presets config files
/// live. Returns `None` only if no home directory can be determined. Overridden
/// by the `--state-dir` flag in both binaries.
pub fn default_state_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join(APP_DIR))
}

/// Configuration for the airtouch5-webui web server.
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
