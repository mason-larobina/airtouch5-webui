//! Connection manager: a long-lived task that owns the `AirTouch5` handle,
//! applies incoming `Command`s, watches live status, and publishes a `Snapshot`
//! via a `tokio::sync::watch` channel for the web layer to render from.

use std::time::Duration;

use tokio::sync::{mpsc, watch};

use airtouch5::types::status::{CurrentStatus, StatusChange};
use airtouch5::AirTouch5;

use crate::airtouch;
use crate::config::Config;
use crate::manager::command::{AcControlReq, Command, ZoneControlReq};
use crate::manager::snapshot::{build_snapshot, StaticInfo};

pub mod command;
pub mod snapshot;

/// A cheap, cloneable handle the web layer uses to talk to the manager.
#[derive(Clone)]
pub struct ManagerHandle {
    /// Read-only current snapshot (clone the receiver to fan out to many SSE
    /// clients).
    pub snapshot_rx: watch::Receiver<snapshot::Snapshot>,
    /// Send a control/refresh command; reply comes back on the embedded
    /// oneshot.
    pub cmd_tx: mpsc::Sender<Command>,
}

/// Spawn the connection manager supervisor and return a handle.
///
/// The supervisor discovers and connects, prefills static info, subscribes to
/// live status, and rebuilds the `Snapshot` on every status change (pushing it
/// through the watch channel). On connection loss it reconnects with backoff.
pub async fn spawn_manager(config: Config) -> ManagerHandle {
    let (snapshot_tx, snapshot_rx) = watch::channel(snapshot::Snapshot {
        connected: false,
        console: snapshot::ConsoleInfo::default(),
        acs: Default::default(),
        zones: Default::default(),
    });
    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(64);

    let initial = snapshot_rx.clone();
    tokio::spawn(manager_loop(config, snapshot_tx, cmd_rx));

    ManagerHandle {
        snapshot_rx: initial,
        cmd_tx,
    }
}

async fn manager_loop(
    config: Config,
    snapshot_tx: watch::Sender<snapshot::Snapshot>,
    mut cmd_rx: mpsc::Receiver<Command>,
) {
    loop {
        // 1. Discover.
        let console = airtouch::discover_with_retry(config.discovery_timeout).await;
        tracing::info!(
            "discovered AirTouch 5 console \"{}\" at {}",
            console.name,
            console.address
        );

        // 2. Connect and prefill, or backoff and retry on failure.
        let connected = match airtouch::connect_and_prefill(console).await {
            Ok((at5, static_info)) => {
                run_connected(at5, static_info, &snapshot_tx, &mut cmd_rx).await
            }
            Err(e) => {
                tracing::error!("connection/prefill failed: {e}");
                push_disconnected(&snapshot_tx);
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        if connected.is_err() {
            // `run_connected` only returns when the connection is lost.
            tracing::warn!("connection lost; reconnecting");
            push_disconnected(&snapshot_tx);
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

/// Run a single connected session. Returns `Ok(())` when the status watch
/// reports the connection lost gracefully, or `Err(())` to trigger a reconnect.
#[allow(clippy::too_many_lines)]
async fn run_connected(
    at5: AirTouch5,
    static_info: StaticInfo,
    snapshot_tx: &watch::Sender<snapshot::Snapshot>,
    cmd_rx: &mut mpsc::Receiver<Command>,
) -> Result<(), ()> {
    let status_rx = at5
        .subscribe_status()
        .ok_or_else(|| tracing::error!("subscribe_status returned None"))?;

    // Publish an initial snapshot from the primed watch.
    {
        let cur = status_rx.borrow().clone();
        let snap = build_snapshot(true, &static_info, &cur);
        let _ = snapshot_tx.send(snap);
    }

    loop {
        tokio::select! {
            // Live status update -> rebuild snapshot and publish.
            res = status_changed(status_rx.clone()) => {
                match res {
                    Ok(cur) => {
                        let snap = build_snapshot(true, &static_info, &cur);
                        let _ = snapshot_tx.send(snap);
                    }
                    Err(()) => {
                        tracing::warn!("status watch closed; connection lost");
                        return Err(());
                    }
                }
            }

            // Incoming command from the web layer.
            Some(cmd) = cmd_rx.recv() => {
                handle_command(&at5, cmd, &static_info, &status_rx, snapshot_tx).await;
            }
        }
    }
}

/// Wait for the next status change on a watch receiver, returning the new
/// `CurrentStatus`. Returns `Err(())` if the sender was dropped (connection
/// lost).
async fn status_changed(rx: watch::Receiver<CurrentStatus>) -> Result<CurrentStatus, ()> {
    let mut rx = rx;
    rx.changed().await.map_err(|_| ())?;
    Ok(rx.borrow().clone())
}

/// Apply a single command, fold any post-change status into the snapshot, then
/// reply on the command's oneshot.
async fn handle_command(
    at5: &AirTouch5,
    cmd: Command,
    static_info: &StaticInfo,
    status_rx: &watch::Receiver<CurrentStatus>,
    snapshot_tx: &watch::Sender<snapshot::Snapshot>,
) {
    match cmd {
        Command::Refresh { reply } => {
            let res = async {
                at5.ac_status().await?;
                at5.zone_status().await?;
                Ok::<_, std::io::Error>(())
            }
            .await
            .map_err(|e| e.to_string());
            // Publish a freshly-built snapshot from the now-updated watch.
            if res.is_ok() {
                let cur = status_rx.borrow().clone();
                let snap = build_snapshot(true, static_info, &cur);
                let _ = snapshot_tx.send(snap);
            }
            let _ = reply.send(res);
        }
        Command::ControlZone { id, req, reply } => {
            let res = apply_zone_control(at5, id, req, static_info, status_rx, snapshot_tx).await;
            let _ = reply.send(res);
        }
        Command::ControlAc { id, req, reply } => {
            let res = apply_ac_control(at5, id, req, static_info, status_rx, snapshot_tx).await;
            let _ = reply.send(res);
        }
    }
}

async fn apply_zone_control(
    at5: &AirTouch5,
    id: u8,
    req: ZoneControlReq,
    static_info: &StaticInfo,
    status_rx: &watch::Receiver<CurrentStatus>,
    snapshot_tx: &watch::Sender<snapshot::Snapshot>,
) -> Result<(), String> {
    let zc = req.to_zone_control();
    let msg = at5.control_zone(id, zc).await.map_err(|e| e.to_string())?;
    // Fold the post-change zone status into the current status and republish
    // the snapshot for snappy UX (the async watch will reconcile shortly).
    let mut cur = status_rx.borrow().clone();
    cur.apply(&StatusChange::ZoneStatusChange(msg.into()));
    let snap = build_snapshot(true, static_info, &cur);
    let _ = snapshot_tx.send(snap);
    Ok(())
}

async fn apply_ac_control(
    at5: &AirTouch5,
    id: u8,
    req: AcControlReq,
    static_info: &StaticInfo,
    status_rx: &watch::Receiver<CurrentStatus>,
    snapshot_tx: &watch::Sender<snapshot::Snapshot>,
) -> Result<(), String> {
    let ac = req.to_ac_control();
    let msg = at5.control_ac(id, ac).await.map_err(|e| e.to_string())?;
    let mut cur = status_rx.borrow().clone();
    cur.apply(&StatusChange::AcStatusChange(msg.into()));
    let snap = build_snapshot(true, static_info, &cur);
    let _ = snapshot_tx.send(snap);
    Ok(())
}

/// Mark the snapshot as disconnected while preserving last-known state (so the
/// UI keeps showing cards with a "disconnected" banner).
fn push_disconnected(snapshot_tx: &watch::Sender<snapshot::Snapshot>) {
    let mut snap = snapshot_tx.borrow().clone();
    snap.connected = false;
    let _ = snapshot_tx.send(snap);
}
