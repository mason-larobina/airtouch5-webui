//! `aircon-mock` binary: the mock AirTouch 5 entrypoint.
//!
//! Same CLI surface as `aircon` (minus the discovery timeout), but it serves the
//! UI against an in-memory mock controller ([`aircon::mock`]) instead of
//! discovering a real console. Handy for manual UI development in a browser
//! without hardware, and for integration tests.
//!
//! `--bind` falls back to the AIRCON_LISTEN env var; `--timeout` arms an
//! auto-shutdown deadline.

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

use aircon::automation::{self, AutomationStore};
use aircon::{mock, serve};

/// aircon-mock: AirTouch 5 web UI against an in-memory mock controller.
#[derive(Parser, Debug)]
#[command(
    name = "aircon-mock",
    version,
    about = "AirTouch 5 web UI (mock controller)"
)]
struct Cli {
    /// Address/port to bind the HTTP server (e.g. 127.0.0.1:3000).
    #[arg(long, env = "AIRCON_LISTEN", default_value = "0.0.0.0:3000")]
    bind: std::net::SocketAddr,

    /// Shut the server down after this many seconds (mainly for tests; off by default).
    #[arg(long)]
    timeout: Option<u64>,

    /// Automation engine evaluation tick, in seconds. Set to 0 to disable the
    /// engine entirely. Default 60 (once per minute).
    #[arg(long, env = "AIRCON_AUTOMATION_TICK_SECS", default_value = "60")]
    automation_tick_secs: u64,

    /// Path to the automation config file (enable/disable + parameters).
    /// Created/updated on change; loaded on startup.
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

    // Tracing init (env-driven: RUST_LOG, then AIRCON_LOG, then the default).
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("aircon=info,tower_http=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    tracing::info!("aircon-mock starting; listening on {}", cli.bind);

    // Spawn the mock controller with the sample (mockup-like) state.
    let (manager, _mock) = mock::spawn_mock_controller(mock::sample_snapshot());

    // Load automation config + spawn the engine. The mock defaults to a
    // writeable file in the cwd (so the mock UI persists toggles just like the
    // real binary).
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
        cli.bind,
        cli.timeout.map(Duration::from_secs),
    )
    .await;
}
