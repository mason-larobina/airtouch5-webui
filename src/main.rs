//! `aircon` binary: the real AirTouch 5 entrypoint.
//!
//! Parses CLI args (clap), spawns the connection manager (which discovers and
//! connects to a real console), and serves the web UI. Tracing is env-driven
//! (RUST_LOG / AIRCON_LOG); `--bind` and `--discovery-timeout-ms` fall back to
//! the AIRCON_LISTEN / AIRCON_DISCOVERY_TIMEOUT_MS env vars.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;

use aircon::automation::{self, AutomationStore};
use aircon::{config::Config, manager::spawn_manager, serve};

/// aircon: AirTouch 5 web UI.
#[derive(Parser, Debug)]
#[command(name = "aircon", version, about = "AirTouch 5 web UI")]
struct Cli {
    /// Address/port to bind the HTTP server (e.g. 127.0.0.1:3000).
    #[arg(long, env = "AIRCON_LISTEN", default_value = "0.0.0.0:3000")]
    bind: std::net::SocketAddr,

    /// How long (ms) UDP discovery waits for a console response.
    #[arg(long, env = "AIRCON_DISCOVERY_TIMEOUT_MS", default_value = "3000")]
    discovery_timeout_ms: u64,

    /// Shut the server down after this many seconds (mainly for tests; off by default).
    #[arg(long)]
    timeout: Option<u64>,

    /// Automation engine evaluation tick, in seconds. Set to 0 to disable the
    /// engine entirely. Default 60 (once per minute).
    #[arg(long, env = "AIRCON_AUTOMATION_TICK_SECS", default_value = "60")]
    automation_tick_secs: u64,

    /// Path to the automation config file (enable/disable + parameters).
    /// Created/updated on change; loaded on startup. Defaults to
    /// `automation.json` in the current directory.
    #[arg(
        long,
        env = "AIRCON_AUTOMATION_CONFIG",
        default_value = "automation.json"
    )]
    automation_config: PathBuf,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let config = Arc::new(Config::new(
        cli.bind,
        Duration::from_millis(cli.discovery_timeout_ms),
    ));

    // Tracing init (env-driven: RUST_LOG, then AIRCON_LOG, then the default).
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&config.log_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    tracing::info!("aircon starting; listening on {}", config.listen);

    // Spawn the connection manager (discovers + connects in the background).
    let manager = spawn_manager((*config).clone()).await;

    // Load the shared automation config (persisted to disk) and spawn the
    // background engine that evaluates the enabled programs on a tick.
    let automation = AutomationStore::load(cli.automation_config.clone());
    if cli.automation_tick_secs > 0 {
        automation::spawn_automation(
            manager.clone(),
            automation.clone(),
            Duration::from_secs(cli.automation_tick_secs),
        );
    }

    serve(
        manager,
        automation,
        config.listen,
        cli.timeout.map(Duration::from_secs),
    )
    .await;
}
