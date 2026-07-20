//! `airtouch5-controller-webui-mock` binary: the mock AirTouch 5 entrypoint.
//!
//! Same CLI surface as `airtouch5-controller-webui` (minus the discovery timeout), but it serves the
//! UI against an in-memory mock controller ([`airtouch5_controller_webui::mock`]) instead of
//! discovering a real console. Handy for manual UI development in a browser
//! without hardware, and for integration tests.
//!
//! `--timeout` arms an auto-shutdown deadline; logging is env-driven (`RUST_LOG`).

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

use airtouch5_controller_webui::automation::{self, AutomationStore};
use airtouch5_controller_webui::{mock, serve};

/// airtouch5-controller-webui-mock: AirTouch 5 web UI against an in-memory mock controller.
#[derive(Parser, Debug)]
#[command(
    name = "airtouch5-controller-webui-mock",
    version,
    about = "AirTouch 5 web UI (mock controller)"
)]
struct Cli {
    /// Address/port to bind the HTTP server (e.g. 127.0.0.1:3000).
    #[arg(long, default_value = "0.0.0.0:3000")]
    bind: std::net::SocketAddr,

    /// Shut the server down after this many seconds (mainly for tests; off by default).
    #[arg(long)]
    timeout: Option<u64>,

    /// Automation engine evaluation tick, in seconds. Set to 0 to disable the
    /// engine entirely. Default 60 (once per minute).
    #[arg(long, default_value = "60")]
    automation_tick_secs: u64,

    /// Path to the automation config file (enable/disable + parameters).
    /// Created/updated on change; loaded on startup. When unset, defaults to
    /// `$XDG_CONFIG_HOME/airtouch5-controller-webui/automation.json` (~/.config/airtouch5-controller-webui/...).
    #[arg(long)]
    automation_config: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Tracing init. Logging is the one env-driven option: the tracing filter
    // is read from RUST_LOG, falling back to a sensible default.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("airtouch5_controller_webui=info,tower_http=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    tracing::info!("airtouch5-controller-webui-mock starting; listening on {}", cli.bind);

    // Spawn the mock controller with the sample (mockup-like) state.
    let (manager, _mock) = mock::spawn_mock_controller(mock::sample_snapshot());

    // Load automation config + spawn the engine. Defaults to the XDG config
    // path (so the mock UI persists toggles just like the real binary); use
    // --automation-config to override.
    let automation = AutomationStore::load(
        cli.automation_config
            .or_else(automation::default_config_path)
            .unwrap_or_else(|| PathBuf::from("automation.json")),
    );
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
