//! `airtouch5-webui` binary: the real AirTouch 5 entrypoint.
//!
//! Parses CLI args (clap), spawns the connection manager (which discovers and
//! connects to a real console), and serves the web UI. Logging is the one
//! env-driven option: set `RUST_LOG` to control the tracing filter.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;

use airtouch5_webui::automation::{self, AutomationStore};
use airtouch5_webui::config::{self, Config};
use airtouch5_webui::scenes::{self, SceneStore};
use airtouch5_webui::{manager::spawn_manager, serve};

/// airtouch5-webui: AirTouch 5 web UI.
#[derive(Parser, Debug)]
#[command(name = "airtouch5-webui", version, about = "AirTouch 5 web UI")]
struct Cli {
    /// Address/port to bind the HTTP server (e.g. 127.0.0.1:3000).
    #[arg(long, default_value = "0.0.0.0:3000")]
    bind: std::net::SocketAddr,

    /// How long (ms) UDP discovery waits for a console response.
    #[arg(long, default_value = "3000")]
    discovery_timeout_ms: u64,

    /// Shut the server down after this many seconds (mainly for tests; off by default).
    #[arg(long)]
    timeout: Option<u64>,

    /// Automation engine evaluation tick, in seconds. Set to 0 to disable the
    /// engine entirely. Default 60 (once per minute).
    #[arg(long, default_value = "60")]
    automation_tick_secs: u64,

    /// Directory holding the persisted state files (automation + presets
    /// config). Files are found or created here on startup and updated on
    /// change. When unset, defaults to `$XDG_CONFIG_HOME/airtouch5-webui`
    /// (typically `~/.config/airtouch5-webui`).
    #[arg(long)]
    state_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let config = Arc::new(Config::new(
        cli.bind,
        Duration::from_millis(cli.discovery_timeout_ms),
    ));

    // Tracing init. Logging is the one env-driven option: the tracing filter
    // is read from RUST_LOG, falling back to a sensible default.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| {
            tracing_subscriber::EnvFilter::new("airtouch5_webui=info,tower_http=info")
        });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    tracing::info!("airtouch5-webui starting; listening on {}", config.listen);

    // Spawn the connection manager (discovers + connects in the background).
    let manager = spawn_manager((*config).clone()).await;

    // Resolve the state directory (--state-dir, else the XDG default, else the
    // current directory) that holds the persisted config files.
    let state_dir = cli
        .state_dir
        .or_else(config::default_state_dir)
        .unwrap_or_else(|| PathBuf::from("."));

    // Load the shared automation config from the state dir and spawn the
    // background engine that evaluates the enabled programs on a tick.
    let automation = AutomationStore::load(state_dir.join(automation::CONFIG_FILE_NAME));
    if cli.automation_tick_secs > 0 {
        automation::spawn_automation(
            manager.clone(),
            automation.clone(),
            Duration::from_secs(cli.automation_tick_secs),
        );
    }

    // Load the shared presets store from the state dir.
    let scenes = SceneStore::load(state_dir.join(scenes::CONFIG_FILE_NAME));

    serve(
        manager,
        automation,
        scenes,
        config.listen,
        cli.timeout.map(Duration::from_secs),
    )
    .await;
}
