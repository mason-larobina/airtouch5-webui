//! Connection manager: a long-lived task that owns the `AirTouch5` handle,
//! applies incoming `Command`s, watches live status, and publishes a `Snapshot`
//! via a `tokio::sync::watch` channel for the web layer to render from.

use std::time::Duration;

use tokio::sync::{mpsc, watch};

use airtouch5::types::status::{CurrentStatus, StatusChange};
use airtouch5::AirTouch5;

use crate::airtouch;
use crate::config::Config;
use crate::manager::command::Command;
use crate::manager::snapshot::{build_snapshot, StaticInfo};

pub mod command;
pub mod snapshot;

/// Maximum time allowed for a single console API call (a control message or a
/// status re-pull). A request that exceeds this is aborted and the connection
/// is treated as poisoned: the manager drops the `AirTouch5` handle and
/// re-discovers/reconnects. Without this, a single hung request blocks the
/// one-task command loop (commands are applied serially), so every later
/// click piles up behind it and the UI silently deadlocks.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

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
    let mut status_rx = at5
        .subscribe_status()
        .ok_or_else(|| tracing::error!("subscribe_status returned None"))?;

    // Connection-liveness signal. When the airtouch5 IO loop dies (e.g. the
    // console resets the socket) it drops the only strong `broadcast::Sender`,
    // so this receiver's `recv()` returns `Closed`. We rely on this rather than
    // `status_rx.changed()` because the handle keeps a *clone* of the status
    // watch sender alive, so `changed()` blocks forever after a disconnect and
    // never reports the loss. We only use this channel as a liveness signal --
    // the payload is redundant with the status watch, which rebuilds the
    // snapshot.
    let mut changes_rx = at5
        .subscribe_changes()
        .ok_or_else(|| tracing::error!("subscribe_changes returned None"))?;

    // A read-only clone handed to command handlers so they can `borrow()` the
    // current status. `borrow()` returns the latest value regardless of the
    // receiver's seen-version, so this clone's version being frozen at
    // subscribe time is harmless here -- it is never used with `changed()`.
    //
    // The canonical `status_rx` is the only receiver whose seen-version we
    // advance (via `changed()` / `borrow_and_update()`), and it is the one we
    // wait on in the `select!`. Keeping the version tracking on a single
    // receiver is what avoids the busy-loop: cloning a `watch::Receiver`
    // copies its seen-version, and the previous code cloned `status_rx` fresh
    // every loop iteration without ever advancing the original's version.
    // Once the console's status watch advanced past the subscribe-time
    // version -- which first happens on the very first real status change,
    // i.e. the first control interaction -- every fresh clone's `changed()`
    // returned `Ready` immediately, rebuilding and re-publishing the same
    // snapshot forever and pinning a core at 100%.
    let status_read_rx = status_rx.clone();

    // Publish an initial snapshot from the primed watch.
    {
        let cur = status_rx.borrow().clone();
        let snap = build_snapshot(true, &static_info, &cur);
        let _ = snapshot_tx.send(snap);
    }

    loop {
        tokio::select! {
            // Live status update -> rebuild snapshot and publish. `changed()`
            // runs on the single canonical receiver and advances its
            // seen-version when it resolves, so it blocks until the *next*
            // change rather than spinning on a stale clone.
            res = status_rx.changed() => {
                match res {
                    Ok(()) => {
                        let cur = status_rx.borrow_and_update().clone();
                        let snap = build_snapshot(true, &static_info, &cur);
                        let _ = snapshot_tx.send(snap);
                    }
                    Err(_) => {
                        tracing::warn!("status watch closed; connection lost");
                        return Err(());
                    }
                }
            }

            // Connection died: the IO loop dropped its broadcast sender.
            // `changed()` above can't see this (the handle holds a clone of the
            // watch sender), so this is the arm that actually catches a reset
            // socket and triggers a reconnect.
            res = changes_rx.recv() => {
                match res {
                    Ok(_) => {} // Redundant with the status watch; ignore payload.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("status change stream lagged by {n}; continuing");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        tracing::warn!("status change stream closed; connection lost");
                        return Err(());
                    }
                }
            }

            // Incoming command from the web layer. A timeout poisons the
            // connection: we return `Err(())` to trigger a reconnect rather
            // than keep serving on a stalled socket.
            Some(cmd) = cmd_rx.recv() => {
                if handle_command(&at5, cmd, &static_info, &status_read_rx, snapshot_tx).await.is_err() {
                    tracing::warn!("command timed out; dropping connection to reconnect");
                    return Err(());
                }
            }
        }
    }
}

/// Apply a single command, fold any post-change status into the snapshot, then
/// reply on the command's oneshot.
///
/// Returns `Err(())` when a console API call exceeded [`COMMAND_TIMEOUT`]: the
/// connection is poisoned and the caller should drop it and reconnect. A
/// normal API error (the console responded with an error) is replied to the
/// handler and the connection is kept; only a timeout forces a reconnect.
#[allow(clippy::too_many_lines)]
async fn handle_command(
    at5: &AirTouch5,
    cmd: Command,
    static_info: &StaticInfo,
    status_rx: &watch::Receiver<CurrentStatus>,
    snapshot_tx: &watch::Sender<snapshot::Snapshot>,
) -> Result<(), ()> {
    match cmd {
        Command::Refresh { reply } => {
            let outcome = tokio::time::timeout(COMMAND_TIMEOUT, async {
                at5.ac_status().await?;
                at5.zone_status().await?;
                Ok::<_, std::io::Error>(())
            })
            .await;
            match outcome {
                Ok(Ok(())) => {
                    let cur = status_rx.borrow().clone();
                    let snap = build_snapshot(true, static_info, &cur);
                    let _ = snapshot_tx.send(snap);
                    let _ = reply.send(Ok(()));
                    Ok(())
                }
                Ok(Err(e)) => {
                    let msg = e.to_string();
                    tracing::warn!("refresh failed: {msg}");
                    let _ = reply.send(Err(msg));
                    Ok(())
                }
                Err(_elapsed) => {
                    tracing::error!(
                        "refresh timed out after {COMMAND_TIMEOUT:?}; reconnecting"
                    );
                    let _ = reply.send(Err("console request timed out".to_string()));
                    Err(())
                }
            }
        }
        Command::ControlZone { id, req, reply } => {
            let outcome =
                tokio::time::timeout(COMMAND_TIMEOUT, at5.control_zone(id, req.to_zone_control()))
                    .await;
            match outcome {
                Ok(Ok(msg)) => {
                    let mut cur = status_rx.borrow().clone();
                    cur.apply(&StatusChange::ZoneStatusChange(msg.into()));
                    let snap = build_snapshot(true, static_info, &cur);
                    let _ = snapshot_tx.send(snap);
                    let _ = reply.send(Ok(()));
                    Ok(())
                }
                Ok(Err(e)) => {
                    let msg = e.to_string();
                    tracing::warn!("zone {id} control failed: {msg}");
                    let _ = reply.send(Err(msg));
                    Ok(())
                }
                Err(_elapsed) => {
                    tracing::error!(
                        "zone {id} control timed out after {COMMAND_TIMEOUT:?}; reconnecting"
                    );
                    let _ = reply.send(Err("console request timed out".to_string()));
                    Err(())
                }
            }
        }
        Command::ControlAc { id, req, reply } => {
            let outcome =
                tokio::time::timeout(COMMAND_TIMEOUT, at5.control_ac(id, req.to_ac_control()))
                    .await;
            match outcome {
                Ok(Ok(msg)) => {
                    let mut cur = status_rx.borrow().clone();
                    cur.apply(&StatusChange::AcStatusChange(msg.into()));
                    let snap = build_snapshot(true, static_info, &cur);
                    let _ = snapshot_tx.send(snap);
                    let _ = reply.send(Ok(()));
                    Ok(())
                }
                Ok(Err(e)) => {
                    let msg = e.to_string();
                    tracing::warn!("ac {id} control failed: {msg}");
                    let _ = reply.send(Err(msg));
                    Ok(())
                }
                Err(_elapsed) => {
                    tracing::error!(
                        "ac {id} control timed out after {COMMAND_TIMEOUT:?}; reconnecting"
                    );
                    let _ = reply.send(Err("console request timed out".to_string()));
                    Err(())
                }
            }
        }
    }
}

/// Mark the snapshot as disconnected while preserving last-known state (so the
/// UI keeps showing cards with a "disconnected" banner).
fn push_disconnected(snapshot_tx: &watch::Sender<snapshot::Snapshot>) {
    let mut snap = snapshot_tx.borrow().clone();
    snap.connected = false;
    let _ = snapshot_tx.send(snap);
}

#[cfg(test)]
mod tests {
    //! Regression tests for the status-watch pattern in `run_connected`.
    //!
    //! The real connection manager (unlike the mock used by the e2e tests)
    //! waits on a `tokio::sync::watch::Receiver<CurrentStatus>` from the
    //! airtouch5 crate's IO loop. The original code cloned that receiver fresh
    //! every `tokio::select!` iteration; cloning copies the receiver's
    //! seen-version, and the original's version was never advanced, so every
    //! clone carried the stale subscribe-time version. Once the watch had
    //! advanced past it (first real status change = first interaction),
    //! `changed()` on every fresh clone returned `Ready` immediately and the
    //! loop spun at 100% CPU.
    //!
    //! These tests pin down the `watch` invariant the fix relies on: a single
    //! receiver advanced via `changed()` blocks until the *next* change, while a
    //! freshly cloned receiver (whose seen-version is frozen) reports the last
    //! change as new forever. They guard against a revert that reintroduces the
    //! per-iteration clone.

    use std::time::Duration;
    use tokio::sync::watch;

    /// A single receiver kept in sync via `changed()` blocks until the *next*
    /// change -- it does not repeatedly return `Ready` for the same change.
    #[tokio::test]
    async fn single_receiver_changed_blocks_for_next_change() {
        let (tx, mut rx) = watch::channel(0u32);

        // First change: changed() resolves, advancing the receiver's version.
        tx.send(1).unwrap();
        assert!(rx.changed().await.is_ok());
        assert_eq!(*rx.borrow(), 1);

        // No further change: changed() must BLOCK, not spin. Give it a short
        // window to (incorrectly) resolve; if it ever returns, that's the bug.
        let polled = tokio::time::timeout(Duration::from_millis(50), rx.changed()).await;
        assert!(
            polled.is_err(),
            "changed() returned after no new change; the receiver is not tracking \
             its seen-version and would busy-loop the manager select"
        );
    }

    /// A fresh clone of a receiver whose seen-version was never advanced reports
    /// every already-occurred change as new -- this is the footgun the fix
    /// avoids by waiting on a single canonical receiver instead of cloning per
    /// iteration.
    #[tokio::test]
    async fn fresh_clone_reports_stale_change_as_ready() {
        let (tx, rx) = watch::channel(0u32);

        // Advance the watch once. The original receiver's version is never
        // advanced (it is only ever cloned, mirroring the old buggy pattern).
        tx.send(1).unwrap();

        // A fresh clone carries the original's frozen (subscribe-time) version,
        // so changed() resolves immediately -- and would keep doing so on every
        // subsequent fresh clone, spinning the loop.
        let mut clone = rx.clone();
        assert!(clone.changed().await.is_ok());

        // A second fresh clone still sees the same already-occurred change as
        // new: the loop never makes progress, just burns a core.
        let mut clone2 = rx.clone();
        assert!(clone2.changed().await.is_ok());
        assert_eq!(*clone2.borrow(), 1);

        // Contrast: the single-receiver pattern (borrow_and_update + changed on
        // the same receiver) blocks for the next change.
        let mut single = rx.clone();
        let _ = single.borrow_and_update(); // sync its version to current
        let polled = tokio::time::timeout(Duration::from_millis(50), single.changed()).await;
        assert!(
            polled.is_err(),
            "a synced single receiver must block for the next change, not return \
             Ready for a stale one"
        );
    }
}
