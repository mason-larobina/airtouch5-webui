//! `airtouch5-webui-mock` binary: the mock AirTouch 5 entrypoint.
//!
//! Same CLI surface as `airtouch5-webui` (minus the discovery timeout), but it serves the
//! UI against an in-memory mock controller ([`airtouch5_webui::mock`]) instead of
//! discovering a real console. Handy for manual UI development in a browser
//! without hardware, and for integration tests.
//!
//! `--timeout` arms an auto-shutdown deadline; logging is env-driven (`RUST_LOG`).

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

use airtouch5_webui::automation::{self, AutomationStore};
use airtouch5_webui::config;
use airtouch5_webui::scenes::{self, SceneStore};
use airtouch5_webui::{mock, serve};

/// airtouch5-webui-mock: AirTouch 5 web UI against an in-memory mock controller.
#[derive(Parser, Debug)]
#[command(
    name = "airtouch5-webui-mock",
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

    // Tracing init. Logging is the one env-driven option: the tracing filter
    // is read from RUST_LOG, falling back to a sensible default.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("airtouch5_webui=info,tower_http=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    tracing::info!("airtouch5-webui-mock starting; listening on {}", cli.bind);

    // Spawn the mock controller with the sample (mockup-like) state.
    let (manager, _mock) = mock::spawn_mock_controller(mock::sample_snapshot());

    // Resolve the state directory (--state-dir, else the XDG default, else the
    // current directory) that holds the persisted config files. Defaults match
    // the real binary so the mock UI persists toggles/presets the same way.
    let state_dir = cli
        .state_dir
        .or_else(config::default_state_dir)
        .unwrap_or_else(|| PathBuf::from("."));

    // Load automation config from the state dir + spawn the engine.
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
        cli.bind,
        cli.timeout.map(Duration::from_secs),
    )
    .await;
}
