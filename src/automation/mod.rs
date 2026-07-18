//! Automation programs: hard-coded, configurable rules that the engine
//! evaluates on a background tick and actions by sending controls to the
//! manager.
//!
//! Two programs ship today, both expressed as an enable toggle plus a small
//! set of parameter presets surfaced in the UI (below the zones section):
//!
//! 1. **Setpoint auto-off** -- when every on-zone is in temperature control
//!    mode and has *reached its setpoint* (cooling satisfied / heating
//!    satisfied, decided by the owning AC's mode), turn the AC(s) off. The
//!    condition must remain true for a configurable hold period (default 15
//!    minutes) before the action fires, so a brief dip past the setpoint does
//!    not trip it. The program only runs when every On AC is in a heating or
//!    cooling mode (Heat, Cool, AutoHeat, AutoCool); in any other mode (Auto,
//!    Dry, Fan) it stays idle and the card shows a "not active for this mode"
//!    note.
//! 2. **Idle auto-off** -- if there have been no control-relevant state
//!    changes for a configurable timeout (15/30/60/120 minutes), turn the
//!    AC(s) off. "Control-relevant" excludes the live sensor/temperature
//!    readings (which drift constantly) so the timer is only reset by real
//!    interaction -- a power, mode, fan, setpoint, or airflow change.
//!
//! Both programs turn the **AC units** off (zones are left untouched): an AC
//! running with no open airflow path is undesirable, and the AC power-off is
//! the single action that actually stops the system.
//!
//! The engine is a background task with a 1-minute tick (configurable via the
//! `--automation-tick-secs` flag, default 60). It reads the live snapshot
//! through the manager handle, tracks an idle fingerprint, and sends
//! `ControlAc` commands when a program fires. Configuration lives in an
//! [`AutomationStore`] (an in-memory config optionally persisted to a JSON
//! file) shared with the web layer so the UI toggles/parameter buttons mutate
//! the same config the engine reads.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use airtouch5::types::control::AcPower;

use crate::manager::ManagerHandle;
use crate::manager::command::{AcControlReq, Command};
use crate::manager::snapshot::{self, SensorView, Snapshot, ZoneView};

/// How close a zone's reading needs to be to its setpoint to count as "at
/// setpoint" for the modes without a clear heat/cool direction (Auto, Fan, or
/// unknown). Adds a little noise margin to the directed comparisons too.
const SETPOINT_TOLERANCE_C: f32 = 0.5;

/// The selectable hold-time presets (in minutes) for the setpoint auto-off
/// program. 15 minutes is the default and matches the original requirement;
/// the row mirrors the idle auto-off timeouts (15/30/60/120).
pub const SETPOINT_HOLD_PRESETS: &[u64] = &[15, 30, 60, 120];

/// The selectable idle-timeout presets (in minutes) for the idle auto-off
/// program.
pub const IDLE_TIMEOUT_PRESETS: &[u64] = &[15, 30, 60, 120];

/// Default setpoint auto-off hold time.
pub const DEFAULT_SETPOINT_HOLD: Duration = Duration::from_secs(15 * 60);
/// Default idle auto-off timeout.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// The XDG-based default config path: `$XDG_CONFIG_HOME/airtouch5-controller-webui/automation.json`
/// (falling back to `~/.config/airtouch5-controller-webui/automation.json` when `XDG_CONFIG_HOME`
/// is unset). Returns `None` only if no home directory can be determined.
/// Overridden by the `--automation-config` flag in both binaries.
pub fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("airtouch5-controller-webui").join("automation.json"))
}

/// Configuration for the two automation programs. Persisted as JSON when the
/// store has a path (see [`AutomationStore::load`]).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AutomationConfig {
    /// Whether the setpoint auto-off program is enabled.
    pub setpoint_off_enabled: bool,
    /// How long the setpoint condition must hold before the AC(s) are turned
    /// off.
    #[serde(with = "duration_secs")]
    pub setpoint_off_hold: Duration,
    /// Whether the idle auto-off program is enabled.
    pub idle_off_enabled: bool,
    /// How long the system may sit with no control changes before the AC(s)
    /// are turned off.
    #[serde(with = "duration_secs")]
    pub idle_off_timeout: Duration,
}

impl Default for AutomationConfig {
    fn default() -> Self {
        Self {
            setpoint_off_enabled: false,
            setpoint_off_hold: DEFAULT_SETPOINT_HOLD,
            idle_off_enabled: false,
            idle_off_timeout: DEFAULT_IDLE_TIMEOUT,
        }
    }
}

impl AutomationConfig {
    /// Setpoint hold time in whole minutes (for the UI / parameter parsing).
    pub fn setpoint_off_hold_minutes(&self) -> u64 {
        self.setpoint_off_hold.as_secs() / 60
    }

    /// Idle timeout in whole minutes (for the UI / parameter parsing).
    pub fn idle_off_timeout_minutes(&self) -> u64 {
        self.idle_off_timeout.as_secs() / 60
    }
}

/// Serde helper: store `Duration` as seconds in JSON so the file stays
/// human-readable and stable across versions.
mod duration_secs {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_secs(u64::deserialize(d)?))
    }
}

/// Shared, cloneable handle holding the automation config plus an optional
/// persistence path. The engine reads via [`AutomationStore::get`]; the web
/// layer mutates via the typed setters, which persist on every change.
///
/// It also holds the live setpoint-off countdown instant (`setpoint_since`),
/// written by the engine and read by the web layer so the UI can show how long
/// remains before the AC(s) turn off. This is volatile (not persisted): it is
/// only meaningful while the engine is running and the condition holds.
#[derive(Clone)]
pub struct AutomationStore {
    config: Arc<RwLock<AutomationConfig>>,
    path: Option<PathBuf>,
    setpoint_since: Arc<RwLock<Option<Instant>>>,
    /// The instant of the last control-relevant state change, used by both the
    /// idle auto-off engine (to measure elapsed idle time) and the web layer
    /// (to compute the "powering off at HH:MM" target). Volatile: not persisted.
    idle_last_change: Arc<RwLock<Option<Instant>>>,
}

impl AutomationStore {
    /// An in-memory store with no persistence (used by tests; production uses
    /// [`AutomationStore::load`]).
    pub fn new(config: AutomationConfig) -> Self {
        Self {
            config: Arc::new(RwLock::new(config)),
            path: None,
            setpoint_since: Arc::new(RwLock::new(None)),
            idle_last_change: Arc::new(RwLock::new(None)),
        }
    }

    /// Load the config from `path` if it exists (and is valid JSON); otherwise
    /// start from defaults. Subsequent updates are written back atomically.
    pub fn load(path: PathBuf) -> Self {
        let config = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<AutomationConfig>(&b).ok())
            .unwrap_or_default();
        if path.parent().is_some() {
            tracing::info!("automation config path: {}", path.display());
        } else {
            tracing::info!("automation config path: {}", path.display());
        }
        Self {
            config: Arc::new(RwLock::new(config)),
            path: Some(path),
            setpoint_since: Arc::new(RwLock::new(None)),
            idle_last_change: Arc::new(RwLock::new(None)),
        }
    }

    /// A snapshot copy of the current config.
    pub fn get(&self) -> AutomationConfig {
        self.config.read().expect("config lock poisoned").clone()
    }

    /// The instant the setpoint-off hold countdown started, if the condition
    /// is currently holding (None otherwise). Written by the automation
    /// engine; read by the web layer for the UI countdown badge.
    pub fn setpoint_since(&self) -> Option<Instant> {
        *self
            .setpoint_since
            .read()
            .expect("setpoint_since lock poisoned")
    }

    /// Replace the setpoint-off countdown instant. Engine-only writer.
    pub fn set_setpoint_since(&self, since: Option<Instant>) {
        *self
            .setpoint_since
            .write()
            .expect("setpoint_since lock poisoned") = since;
    }

    /// Compute the live setpoint-off UI status from the current config, the
    /// shared countdown instant and the given snapshot.
    pub fn setpoint_off_status(&self, snap: &Snapshot) -> SetpointOffStatus {
        setpoint_off_status(snap, &self.get(), self.setpoint_since())
    }

    /// Start the setpoint-off countdown if the condition currently holds but
    /// the countdown has not been started yet (e.g. the engine has not ticked,
    /// as in the test harness or right after a fresh server start). Idempotent:
    /// it only writes when the countdown is None. Mirrors the engine so the UI
    /// shows the "powering off at HH:MM" target as soon as the condition is
    /// met, not on the next engine tick.
    pub fn ensure_setpoint_countdown(&self, snap: &Snapshot) {
        let cfg = self.get();
        if cfg.setpoint_off_enabled
            && any_ac_on(snap)
            && setpoint_condition(snap)
            && self.setpoint_since().is_none()
        {
            self.set_setpoint_since(Some(Instant::now()));
        }
    }

    /// The instant of the last control-relevant change, if the idle auto-off
    /// countdown is running (None when the program is disabled or has not yet
    /// started). Written by the engine and the enable handler; read by the
    /// web layer for the "powering off at HH:MM" badge.
    pub fn idle_last_change(&self) -> Option<Instant> {
        *self
            .idle_last_change
            .read()
            .expect("idle_last_change lock poisoned")
    }

    /// Replace the idle last-change instant. Engine + enable-handler writer.
    pub fn set_idle_last_change(&self, since: Option<Instant>) {
        *self
            .idle_last_change
            .write()
            .expect("idle_last_change lock poisoned") = since;
    }

    /// Compute the live idle auto-off UI status from the current config, the
    /// shared last-change instant and the given snapshot.
    pub fn idle_off_status(&self, snap: &Snapshot) -> IdleOffStatus {
        idle_off_status(snap, &self.get(), self.idle_last_change())
    }

    /// Apply a mutation and persist it (if a path is set). Returns an error
    /// string if persistence fails.
    pub fn update<F>(&self, f: F) -> Result<(), String>
    where
        F: FnOnce(&mut AutomationConfig),
    {
        let new = {
            let mut g = self.config.write().expect("config lock poisoned");
            f(&mut g);
            g.clone()
        };
        self.persist(&new)
    }

    /// Enable/disable the setpoint auto-off program. Disabling also clears
    /// the live countdown so the UI badge disappears immediately rather than
    /// waiting for the next engine tick.
    pub fn set_setpoint_off_enabled(&self, enabled: bool) -> Result<(), String> {
        let res = self.update(|c| c.setpoint_off_enabled = enabled);
        if res.is_ok() && !enabled {
            self.set_setpoint_since(None);
        }
        res
    }

    /// Set the setpoint auto-off hold time (in minutes). Rejects values that
    /// are not one of [`SETPOINT_HOLD_PRESETS`].
    pub fn set_setpoint_off_hold(&self, minutes: u64) -> Result<(), String> {
        if !SETPOINT_HOLD_PRESETS.contains(&minutes) {
            return Err(format!(
                "hold must be one of {:?} minutes",
                SETPOINT_HOLD_PRESETS
            ));
        }
        self.update(|c| c.setpoint_off_hold = Duration::from_secs(minutes * 60))
    }

    /// Enable/disable the idle auto-off program. Enabling (re)starts the idle
    /// countdown so the UI badge shows a fresh target time and the engine does
    /// not fire immediately from a stale last-change. Disabling clears the
    /// countdown so the badge disappears right away.
    pub fn set_idle_off_enabled(&self, enabled: bool) -> Result<(), String> {
        let res = self.update(|c| c.idle_off_enabled = enabled);
        if res.is_ok() {
            self.set_idle_last_change(if enabled { Some(Instant::now()) } else { None });
        }
        res
    }

    /// Set the idle auto-off timeout (in minutes). Rejects values that are not
    /// one of [`IDLE_TIMEOUT_PRESETS`].
    pub fn set_idle_off_timeout(&self, minutes: u64) -> Result<(), String> {
        if !IDLE_TIMEOUT_PRESETS.contains(&minutes) {
            return Err(format!(
                "timeout must be one of {:?} minutes",
                IDLE_TIMEOUT_PRESETS
            ));
        }
        self.update(|c| c.idle_off_timeout = Duration::from_secs(minutes * 60))
    }

    fn persist(&self, cfg: &AutomationConfig) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        // Ensure the parent directory exists (the XDG default lives under
        // `~/.config/airtouch5-controller-webui`, which may not exist on a fresh install).
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let bytes = serde_json::to_vec_pretty(cfg).map_err(|e| e.to_string())?;
        // Atomic-ish: write to a sibling temp file then rename over the target.
        let tmp_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| format!("{n}.tmp"))
            .unwrap_or_else(|| "automation.json.tmp".to_string());
        let tmp = path.with_file_name(tmp_name);
        std::fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// Spawn the automation engine as a background task. It ticks every
/// `tick_interval`, evaluates the enabled programs against the live snapshot,
/// and sends AC power-off commands when one fires. A `tick_interval` of zero
/// disables the engine (the task exits immediately).
pub fn spawn_automation(
    manager: ManagerHandle,
    store: AutomationStore,
    tick_interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if tick_interval.is_zero() {
            tracing::info!("automation engine disabled (tick interval is zero)");
            return;
        }
        tracing::info!("automation engine started; tick every {:?}", tick_interval);

        let mut rx = manager.snapshot_rx.clone();
        let mut last_fp = control_fingerprint(&rx.borrow());
        // Seed the idle countdown at engine start (mirrors the original
        // `last_change = Instant::now()`) unless a value is already present
        // (e.g. the enable handler just set it).
        if store.idle_last_change().is_none() {
            store.set_idle_last_change(Some(Instant::now()));
        }

        let mut interval = tokio::time::interval(tick_interval);
        // A burst of catch-up ticks after a stall (e.g. the process was
        // suspended) would re-fire programs repeatedly; `Delay` collapses them
        // into one tick at the next interval boundary.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                // A snapshot was published. Recompute the control fingerprint;
                // if real control state changed, reset the idle timer. Sensor
                // drift is excluded from the fingerprint so it does not keep
                // the idle timer alive forever.
                res = rx.changed() => {
                    match res {
                        Ok(()) => {
                            let snap = rx.borrow_and_update().clone();
                            let fp = control_fingerprint(&snap);
                            if fp != last_fp {
                                last_fp = fp;
                                // A real control change resets the idle
                                // countdown; the shared store mirrors it so the
                                // UI "powering off at HH:MM" target updates.
                                store.set_idle_last_change(Some(Instant::now()));
                            }
                        }
                        Err(_) => {
                            tracing::warn!("automation: snapshot watch closed; engine exiting");
                            break;
                        }
                    }
                }
                // Evaluation tick: check both programs against the live state.
                _ = interval.tick() => {
                    let cfg = store.get();
                    let snap = rx.borrow().clone();
                    if !snap.connected {
                        continue;
                    }

                    // (1) Setpoint auto-off.
                    if cfg.setpoint_off_enabled {
                        let cond = any_ac_on(&snap) && setpoint_condition(&snap);
                        let since = store.setpoint_since();
                        let new_since = match (since, cond) {
                            // Holding: if the hold has elapsed, fire and clear.
                            (Some(start), true) if start.elapsed() >= cfg.setpoint_off_hold => {
                                tracing::info!(
                                    "automation: setpoint auto-off firing (held {:?})",
                                    start.elapsed()
                                );
                                turn_off_acs(&manager, &snap).await;
                                None
                            }
                            // Holding but hold not yet elapsed: keep counting.
                            (Some(start), true) => Some(start),
                            // Just became true: start the countdown.
                            (None, true) => Some(Instant::now()),
                            // Condition false (or flickered off): reset.
                            _ => None,
                        };
                        if new_since != since {
                            store.set_setpoint_since(new_since);
                        }
                    } else if store.setpoint_since().is_some() {
                        store.set_setpoint_since(None);
                    }

                    // (2) Idle auto-off.
                    if cfg.idle_off_enabled {
                        let last_change = store.idle_last_change();
                        // No countdown running (e.g. just enabled without a
                        // control change yet): start it now.
                        let last_change = match last_change {
                            Some(lc) => lc,
                            None => {
                                let now = Instant::now();
                                store.set_idle_last_change(Some(now));
                                now
                            }
                        };
                        if any_ac_on(&snap)
                            && last_change.elapsed() >= cfg.idle_off_timeout
                        {
                            tracing::info!(
                                "automation: idle auto-off firing (idle {:?})",
                                last_change.elapsed()
                            );
                            turn_off_acs(&manager, &snap).await;
                            // Reset so we do not re-fire on the next tick; the
                            // power-off publish will also reset the countdown
                            // via the changed() arm.
                            store.set_idle_last_change(Some(Instant::now()));
                        }
                    } else if store.idle_last_change().is_some() {
                        store.set_idle_last_change(None);
                    }
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Program conditions
// ---------------------------------------------------------------------------

/// True if at least one AC is currently On (Away/Sleep are left alone -- those
/// are intentional states the user set explicitly).
fn any_ac_on(snap: &Snapshot) -> bool {
    snap.acs.values().any(|a| a.power() == Some("On"))
}

/// True when the given AC mode is a heating or cooling mode -- the only modes
/// the setpoint auto-off program runs for. `AutoHeat`/`AutoCool` are the
/// console's auto-resolved heating/cooling decision; plain `Auto` (undecided),
/// `Dry`, `Fan`, and unknown are not active.
fn ac_mode_active(mode: Option<&str>) -> bool {
    matches!(
        mode,
        Some("Heat") | Some("Cool") | Some("AutoHeat") | Some("AutoCool")
    )
}

/// True when every On AC is in a heating or cooling mode. The setpoint
/// auto-off program only runs in this case; otherwise it stays idle and the
/// card shows a "not active for this mode" note. Vacuously true when no AC is
/// On (the program is then not eligible for the unrelated reason that nothing
/// is running).
fn mode_eligible(snap: &Snapshot) -> bool {
    snap.acs
        .values()
        .filter(|a| a.power() == Some("On"))
        .all(|a| ac_mode_active(a.mode()))
}

/// The setpoint auto-off condition: every On AC is in a heating or cooling
/// mode, and every on-zone is in temperature control mode and has reached its
/// setpoint (per the owning AC's mode). Sensorless on-zones, or sensor zones
/// whose reading is unavailable, can never be confirmed "at setpoint" so they
/// fail the condition (safe: we do not turn off). Returns false when there
/// are no on-zones.
fn setpoint_condition(snap: &Snapshot) -> bool {
    mode_eligible(snap) && setpoint_detail(snap).0
}

/// A zone's current sensor reading as f32, or None if no sensor / reading
/// unavailable.
fn zone_reading_f32(z: &ZoneView) -> Option<f32> {
    match &z.sensor {
        Some(SensorView::Temperature(t)) => snapshot::temp_to_f32(*t),
        _ => None,
    }
}

/// Whether a zone's reading counts as "reached setpoint" given the owning AC's
/// mode. Cooling modes are satisfied when the room has cooled to (or below)
/// the setpoint; heating modes when it has warmed to (or above) it. Modes
/// without a clear direction (Auto, Fan, or unknown) fall back to a symmetric
/// "within tolerance" check so the program can still fire when the room truly
/// is at the target. A small tolerance avoids measurement noise blocking the
/// condition.
fn zone_satisfied(ac_mode: Option<&str>, reading: f32, setpoint: f32) -> bool {
    match ac_mode {
        Some("Cool") | Some("AutoCool") => reading <= setpoint + SETPOINT_TOLERANCE_C,
        Some("Heat") | Some("AutoHeat") => reading >= setpoint - SETPOINT_TOLERANCE_C,
        // Auto (plain), Dry, Fan, or unknown: the setpoint auto-off program
        // only runs for heating and cooling, so a zone on an AC in any other
        // mode is never "satisfied" -- the card shows a mode note instead.
        _ => false,
    }
}

/// Live, derived status of the setpoint auto-off program for the UI. It
/// summarises whether the program is currently counting down to a shutoff
/// (every on-zone is in temperature mode and has reached its setpoint) and,
/// if so, how long remains before the engine turns the AC(s) off. The web
/// layer recomputes this from the snapshot plus the shared `setpoint_since`
/// countdown handle on every change so the card can show live feedback.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SetpointOffStatus {
    /// Whether the program is enabled (copied from config for convenience).
    pub enabled: bool,
    /// Whether at least one AC is currently On (the program is eligible).
    pub ac_on: bool,
    /// Whether every On AC is in a heating/cooling mode. When false (and an
    /// AC is on) the card shows a "not active for this mode" note instead of
    /// the countdown/waiting status, and the program never fires.
    pub mode_eligible: bool,
    /// Whether every on-zone is in temperature mode and has reached its
    /// setpoint -- i.e. the hold countdown is (or is about to be) running.
    pub at_setpoint: bool,
    /// Number of on-zones currently confirmed at their setpoint.
    pub satisfied: usize,
    /// Total number of on-zones.
    pub on_zones: usize,
    /// The wall-clock shutoff time as "HH:MM" (24-hour, local) while the hold
    /// countdown is running (at setpoint + countdown started, hold not yet
    /// elapsed); None otherwise. Fixed at `since + hold` so it does not tick
    /// as time passes -- the card only re-renders when the target actually
    /// shifts (on a new control change or when the condition flips).
    pub target_time: Option<String>,
}

/// Compute the setpoint auto-off status for the UI from the live snapshot,
/// the program config and the shared countdown instant. Mirrors the engine's
/// `cond = any_ac_on && setpoint_condition` so the badge and the engine agree.
pub fn setpoint_off_status(
    snap: &Snapshot,
    cfg: &AutomationConfig,
    since: Option<Instant>,
) -> SetpointOffStatus {
    let (at_setpoint, satisfied, on_zones) = setpoint_detail(snap);
    let ac_on = any_ac_on(snap);
    let mode_eligible = mode_eligible(snap);
    // The program only counts down when every On AC is in a heating/cooling
    // mode; mirror the engine's `setpoint_condition` so the UI and engine
    // agree and a mode change resets the badge immediately.
    let at_setpoint = at_setpoint && mode_eligible;
    let target_time = if cfg.setpoint_off_enabled && ac_on && at_setpoint {
        since.and_then(|start| {
            let deadline = start + cfg.setpoint_off_hold;
            // Once the hold has elapsed the engine fires on its next tick;
            // show no time then (a past clock time would look broken). The
            // card falls back to a stable "powering system off" line.
            if deadline <= Instant::now() {
                None
            } else {
                instant_to_local(deadline).map(|dt| dt.format("%H:%M").to_string())
            }
        })
    } else {
        None
    };
    SetpointOffStatus {
        enabled: cfg.setpoint_off_enabled,
        ac_on,
        mode_eligible,
        at_setpoint,
        satisfied,
        on_zones,
        target_time,
    }
}

/// Same evaluation as [`setpoint_condition`] but also returns the count of
/// on-zones confirmed at setpoint and the total on-zone count, for the UI
/// status line. `at_setpoint` is true only when every on-zone qualifies
/// (and there is at least one on-zone).
fn setpoint_detail(snap: &Snapshot) -> (bool, usize, usize) {
    let mut on_zones = 0;
    let mut satisfied = 0;
    for z in snap.zones.values().filter(|z| z.is_on()) {
        on_zones += 1;
        // A non-temp on-zone disqualifies the program (cannot be "at setpoint").
        let ok = z.is_temp()
            && zone_reading_f32(z)
                .zip(z.setpoint.and_then(snapshot::temp_to_f32))
                .map(|(reading, setpoint)| {
                    let ac_mode = z
                        .ac_id
                        .and_then(|aid| snap.acs.get(&aid))
                        .and_then(|a| a.mode());
                    zone_satisfied(ac_mode, reading, setpoint)
                })
                .unwrap_or(false);
        if ok {
            satisfied += 1;
        }
    }
    // at_setpoint mirrors setpoint_condition: at least one on-zone and every
    // on-zone confirmed at its setpoint.
    let at_setpoint = on_zones > 0 && satisfied == on_zones;
    (at_setpoint, satisfied, on_zones)
}

/// Live, derived status of the idle auto-off program for the UI. It reports the
/// wall-clock time at which the engine will power the system off if no further
/// control change happens. The target time is fixed at the moment of the last
/// control change (plus the configured timeout), so it only shifts when the
/// user interacts with the system or changes the timeout preset -- it does not
/// tick as time passes. The web layer recomputes this on every snapshot change
/// but the SSE diff only re-emits the card when the formatted time actually
/// changes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IdleOffStatus {
    /// Whether the program is enabled (copied from config for convenience).
    pub enabled: bool,
    /// Whether at least one AC is currently On (the program is eligible).
    pub ac_on: bool,
    /// The wall-clock shutoff time as "HH:MM" (24-hour, local time), or None
    /// when the program is disabled / no AC is on / the countdown has not
    /// started. None hides the status line.
    pub target_time: Option<String>,
}

/// Compute the idle auto-off status for the UI from the live snapshot, the
/// program config and the shared last-control-change instant. The target time
/// is `last_change + timeout` expressed as local wall clock.
pub fn idle_off_status(
    snap: &Snapshot,
    cfg: &AutomationConfig,
    last_change: Option<Instant>,
) -> IdleOffStatus {
    let ac_on = any_ac_on(snap);
    let target_time = if cfg.idle_off_enabled && ac_on {
        last_change
            .and_then(|lc| instant_to_local(lc + cfg.idle_off_timeout))
            .map(|dt| dt.format("%H:%M").to_string())
    } else {
        None
    };
    IdleOffStatus {
        enabled: cfg.idle_off_enabled,
        ac_on,
        target_time,
    }
}

/// Convert a monotonic [`Instant`] to a local wall-clock [`DateTime<Local>`],
/// using a baseline pair captured on first use (a monotonic instant and the
/// matching system time). Conversion is relative to that baseline, so it stays
/// correct across short runs without needing a persistent clock reference.
/// Returns None only if the system time is before the Unix epoch (which would
/// be a broken clock).
fn instant_to_local(instant: Instant) -> Option<chrono::DateTime<chrono::Local>> {
    use std::sync::OnceLock;
    use std::time::SystemTime;
    static BASE_INSTANT: OnceLock<Instant> = OnceLock::new();
    static BASE_SYSTEM: OnceLock<SystemTime> = OnceLock::new();
    let base_i = *BASE_INSTANT.get_or_init(Instant::now);
    let base_s = *BASE_SYSTEM.get_or_init(SystemTime::now);
    // Offset of `instant` from the baseline instant. `saturating_elapsed_since`
    // yields 0 when `instant` predates the baseline (e.g. a last-change captured
    // before the first render); the result is then off by at most the startup
    // gap, which the minute-granular format absorbs.
    let off = instant.saturating_duration_since(base_i);
    Some(chrono::DateTime::<chrono::Local>::from(base_s + off))
}

/// A compact, stable string summarising the *control* state of the system --
/// everything a user (or the wall console) could change that should reset the
/// idle timer. Crucially excludes the live sensor readings and AC "now"
/// temperatures, which drift continuously and would otherwise keep the idle
/// timer alive forever.
pub fn control_fingerprint(snap: &Snapshot) -> String {
    let mut s = String::new();
    for (id, z) in &snap.zones {
        s.push_str(&format!(
            "z{}:{:?}:{:?}:{}:{}|",
            id,
            z.power,
            z.control_mode,
            z.airflow_pct,
            z.setpoint
                .and_then(snapshot::temp_to_f32)
                .map(|x| x.to_string())
                .unwrap_or_default(),
        ));
    }
    for (id, a) in &snap.acs {
        let st = a.status.as_ref();
        s.push_str(&format!(
            "a{}:{:?}:{:?}:{:?}:{}:{}|",
            id,
            st.and_then(|s| s.power),
            st.and_then(|s| s.mode),
            st.and_then(|s| s.fan_speed),
            st.map(|s| s.fan_intelligent_auto).unwrap_or(false),
            st.and_then(|s| s.setpoint)
                .and_then(snapshot::temp_to_f32)
                .map(|x| x.to_string())
                .unwrap_or_default(),
        ));
    }
    s
}

/// Turn every On AC off. Zones are left untouched (per the "turn off = ACs
/// only" decision). Errors are logged and otherwise ignored: a failed
/// command for one AC should not stop the rest.
async fn turn_off_acs(manager: &ManagerHandle, snap: &Snapshot) {
    for (id, ac) in &snap.acs {
        if ac.power() != Some("On") {
            continue;
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        let res = manager
            .cmd_tx
            .send(Command::ControlAc {
                id: *id,
                req: AcControlReq::Power(AcPower::Off),
                reply: tx,
            })
            .await;
        if res.is_err() {
            tracing::warn!("automation: failed to send AC {id} off command");
            continue;
        }
        if let Err(e) = rx.await {
            tracing::warn!("automation: AC {id} off reply dropped: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock;
    use airtouch5::types::Temperature;

    /// Build a one-AC/on snapshot with a single on sensor zone at a given
    /// reading/setpoint, AC in Cool mode and On.
    fn snap_with_zone(reading: f32, setpoint: f32) -> Snapshot {
        let mut s = mock::sample_snapshot();
        // Single on zone in temperature mode.
        s.zones.retain(|&id, _| id == 0);
        let z = s.zones.get_mut(&0).unwrap();
        z.power = snapshot::ZonePowerView::On;
        z.has_sensor = true;
        z.control_mode = snapshot::ControlModeView::Temperature;
        z.setpoint = Some(Temperature::from_float(setpoint));
        z.sensor = Some(SensorView::Temperature(Temperature::from_float(reading)));
        // Make sure the AC is on and in Cool mode.
        let ac = s.acs.get_mut(&0).unwrap();
        let st = ac.status.as_mut().unwrap();
        st.power = Some("On");
        st.mode = Some("Cool");
        s
    }

    #[test]
    fn setpoint_condition_cooling_satisfied() {
        // reading 23.0, setpoint 23.0, AC Cool -> satisfied.
        let s = snap_with_zone(23.0, 23.0);
        assert!(setpoint_condition(&s));
        // reading 23.4 still within tolerance of 23.0 -> satisfied (<= 23.5).
        let s = snap_with_zone(23.4, 23.0);
        assert!(setpoint_condition(&s));
        // reading 24.0 -> not satisfied.
        let s = snap_with_zone(24.0, 23.0);
        assert!(!setpoint_condition(&s));
    }

    #[test]
    fn setpoint_condition_requires_temp_mode() {
        let mut s = snap_with_zone(23.0, 23.0);
        s.zones.get_mut(&0).unwrap().control_mode = snapshot::ControlModeView::Airflow;
        assert!(!setpoint_condition(&s), "airflow-mode on zone disqualifies");
    }

    #[test]
    fn setpoint_condition_no_on_zones_is_false() {
        let mut s = snap_with_zone(23.0, 23.0);
        s.zones.get_mut(&0).unwrap().power = snapshot::ZonePowerView::Off;
        assert!(!setpoint_condition(&s));
    }

    #[test]
    fn setpoint_condition_sensor_unavailable_fails() {
        let mut s = snap_with_zone(23.0, 23.0);
        s.zones.get_mut(&0).unwrap().sensor = Some(SensorView::NotAvailable);
        assert!(!setpoint_condition(&s), "no reading -> cannot confirm");
    }

    #[test]
    fn setpoint_condition_heating_satisfied() {
        let mut s = snap_with_zone(23.0, 23.0);
        s.acs.get_mut(&0).unwrap().status.as_mut().unwrap().mode = Some("Heat");
        // heating: satisfied when reading >= setpoint - tol.
        assert!(setpoint_condition(&s));
        let mut s2 = s.clone();
        s2.zones.get_mut(&0).unwrap().sensor =
            Some(SensorView::Temperature(Temperature::from_float(22.0)));
        assert!(!setpoint_condition(&s2), "below setpoint -> not yet heated");
    }

    /// The program is only active for heating/cooling modes. A zone at its
    /// setpoint on an AC in Fan mode must NOT satisfy the condition (and the
    /// mode is reported ineligible so the card shows the mode note).
    #[test]
    fn setpoint_condition_fan_mode_not_active() {
        let mut s = snap_with_zone(23.0, 23.0); // would be satisfied in Cool.
        s.acs.get_mut(&0).unwrap().status.as_mut().unwrap().mode = Some("Fan");
        assert!(!mode_eligible(&s), "Fan is not a heating/cooling mode");
        assert!(!setpoint_condition(&s), "Fan mode must not fire");
    }

    /// Dry and plain Auto are not heating/cooling modes either, even when the
    /// zone reading is at the setpoint.
    #[test]
    fn setpoint_condition_dry_and_auto_not_active() {
        let mut s = snap_with_zone(23.0, 23.0);
        s.acs.get_mut(&0).unwrap().status.as_mut().unwrap().mode = Some("Dry");
        assert!(!mode_eligible(&s));
        assert!(!setpoint_condition(&s), "Dry mode must not fire");
        s.acs.get_mut(&0).unwrap().status.as_mut().unwrap().mode = Some("Auto");
        assert!(!mode_eligible(&s));
        assert!(!setpoint_condition(&s), "plain Auto mode must not fire");
    }

    /// AutoHeat/AutoCool (the console's auto-resolved heating/cooling
    /// directions) ARE active.
    #[test]
    fn setpoint_condition_auto_heat_cool_active() {
        let mut s = snap_with_zone(23.0, 23.0);
        s.acs.get_mut(&0).unwrap().status.as_mut().unwrap().mode = Some("AutoHeat");
        assert!(mode_eligible(&s) && setpoint_condition(&s));
        s.acs.get_mut(&0).unwrap().status.as_mut().unwrap().mode = Some("AutoCool");
        assert!(mode_eligible(&s) && setpoint_condition(&s));
    }

    #[test]
    fn fingerprint_excludes_sensor_drift() {
        let s1 = snap_with_zone(23.0, 23.0);
        let mut s2 = s1.clone();
        // Drift the sensor reading only; fingerprint must be unchanged.
        s2.zones.get_mut(&0).unwrap().sensor =
            Some(SensorView::Temperature(Temperature::from_float(25.0)));
        assert_eq!(control_fingerprint(&s1), control_fingerprint(&s2));
    }

    #[test]
    fn fingerprint_changes_on_power_change() {
        let s1 = snap_with_zone(23.0, 23.0);
        let mut s2 = s1.clone();
        s2.acs.get_mut(&0).unwrap().status.as_mut().unwrap().power = Some("Off");
        assert_ne!(control_fingerprint(&s1), control_fingerprint(&s2));
    }

    #[test]
    fn store_update_persists_to_file() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "airtouch5-controller-webui-automation-test-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = AutomationStore::load(path.clone());
        store.set_setpoint_off_enabled(true).unwrap();
        store.set_idle_off_timeout(60).unwrap();
        // A fresh store loading the same path sees the persisted values.
        let reloaded = AutomationStore::load(path.clone());
        assert!(reloaded.get().setpoint_off_enabled);
        assert_eq!(reloaded.get().idle_off_timeout_minutes(), 60);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn store_rejects_unknown_preset() {
        let store = AutomationStore::new(AutomationConfig::default());
        assert!(store.set_setpoint_off_hold(7).is_err());
        assert!(store.set_idle_off_timeout(7).is_err());
        assert!(store.set_setpoint_off_hold(15).is_ok());
        assert!(store.set_idle_off_timeout(120).is_ok());
    }

    /// End-to-end-ish: the engine turns the AC off after the setpoint hold
    /// elapses, using short durations and the mock controller.
    #[tokio::test]
    async fn engine_fires_setpoint_off() {
        let snap = snap_with_zone(23.0, 23.0); // satisfied, AC on, Cool.
        let (manager, _mock) = mock::spawn_mock_controller(snap);
        let store = AutomationStore::new(AutomationConfig {
            setpoint_off_enabled: true,
            setpoint_off_hold: Duration::from_millis(400),
            idle_off_enabled: false,
            idle_off_timeout: Duration::from_secs(3600),
        });
        let _h = spawn_automation(manager.clone(), store, Duration::from_millis(100));

        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if manager
                    .snapshot_rx
                    .borrow()
                    .acs
                    .get(&0)
                    .and_then(|a| a.power())
                    == Some("Off")
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("AC was not turned off within 3s");
    }

    /// The engine does not fire while the hold is unmet (condition flickers
    /// reset it). Here the condition is true but the hold is long.
    #[tokio::test]
    async fn engine_does_not_fire_before_hold() {
        let snap = snap_with_zone(23.0, 23.0);
        let (manager, _mock) = mock::spawn_mock_controller(snap);
        let store = AutomationStore::new(AutomationConfig {
            setpoint_off_enabled: true,
            setpoint_off_hold: Duration::from_secs(3600),
            idle_off_enabled: false,
            idle_off_timeout: Duration::from_secs(3600),
        });
        let _h = spawn_automation(manager.clone(), store, Duration::from_millis(100));
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            manager
                .snapshot_rx
                .borrow()
                .acs
                .get(&0)
                .and_then(|a| a.power()),
            Some("On"),
            "AC should still be on before the hold elapses"
        );
    }

    /// The engine does not fire when an On AC is in a non-heating/cooling
    /// mode, even with the zone at its setpoint and the hold elapsed: the
    /// program only runs for heating and cooling.
    #[tokio::test]
    async fn engine_does_not_fire_for_fan_mode() {
        let mut snap = snap_with_zone(23.0, 23.0); // satisfied in Cool...
        snap.acs.get_mut(&0).unwrap().status.as_mut().unwrap().mode = Some("Fan");
        let (manager, _mock) = mock::spawn_mock_controller(snap);
        let store = AutomationStore::new(AutomationConfig {
            setpoint_off_enabled: true,
            setpoint_off_hold: Duration::from_millis(400),
            idle_off_enabled: false,
            idle_off_timeout: Duration::from_secs(3600),
        });
        let _h = spawn_automation(manager.clone(), store, Duration::from_millis(100));
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            manager.snapshot_rx.borrow().acs.get(&0).and_then(|a| a.power()),
            Some("On"),
            "Fan mode must not trigger setpoint auto-off"
        );
    }

    /// The idle program turns the AC off after the idle timeout with no
    /// control changes.
    #[tokio::test]
    async fn engine_fires_idle_off() {
        let snap = mock::sample_snapshot(); // AC on, plenty of on-zones.
        let (manager, mock) = mock::spawn_mock_controller(snap);
        // Mutate sensor readings only (drift) -- must NOT reset idle.
        mock.try_mutate(|s| {
            if let Some(z) = s.zones.get_mut(&0) {
                if let Some(SensorView::Temperature(t)) = z.sensor.as_mut() {
                    *t = Temperature::from_float(26.0);
                }
            }
        });
        let store = AutomationStore::new(AutomationConfig {
            setpoint_off_enabled: false,
            setpoint_off_hold: Duration::from_secs(3600),
            idle_off_enabled: true,
            idle_off_timeout: Duration::from_millis(400),
        });
        let _h = spawn_automation(manager.clone(), store, Duration::from_millis(100));

        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if manager
                    .snapshot_rx
                    .borrow()
                    .acs
                    .get(&0)
                    .and_then(|a| a.power())
                    == Some("Off")
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("AC was not turned off within 3s");
    }

    /// A real control change (a zone power toggle) within the idle window
    /// resets the idle timer so the program does not fire.
    #[tokio::test]
    async fn engine_idle_reset_by_control_change() {
        let snap = mock::sample_snapshot();
        let (manager, mock) = mock::spawn_mock_controller(snap);
        let store = AutomationStore::new(AutomationConfig {
            setpoint_off_enabled: false,
            setpoint_off_hold: Duration::from_secs(3600),
            idle_off_enabled: true,
            idle_off_timeout: Duration::from_millis(500),
        });
        let _h = spawn_automation(manager.clone(), store, Duration::from_millis(100));

        // Every 150ms flip a zone's power -- a control change that resets the
        // idle timer. The AC should stay on well past the 500ms timeout.
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(150)).await;
            mock.try_mutate(|s| {
                if let Some(z) = s.zones.get_mut(&1) {
                    z.power = if matches!(z.power, snapshot::ZonePowerView::Off) {
                        snapshot::ZonePowerView::On
                    } else {
                        snapshot::ZonePowerView::Off
                    };
                }
            });
        }
        assert_eq!(
            manager
                .snapshot_rx
                .borrow()
                .acs
                .get(&0)
                .and_then(|a| a.power()),
            Some("On"),
            "control changes should keep the idle timer from firing"
        );
    }

    #[test]
    fn default_config_is_disabled() {
        let c = AutomationConfig::default();
        assert!(!c.setpoint_off_enabled);
        assert!(!c.idle_off_enabled);
        assert_eq!(c.setpoint_off_hold_minutes(), 15);
        assert_eq!(c.idle_off_timeout_minutes(), 30);
    }

    /// The status reports "waiting" (no target time) while the on-zone has
    /// not reached its setpoint.
    #[test]
    fn status_waiting_when_not_at_setpoint() {
        let s = snap_with_zone(24.0, 23.0); // Cool: 24.0 > 23.5 -> not satisfied.
        let cfg = AutomationConfig {
            setpoint_off_enabled: true,
            ..AutomationConfig::default()
        };
        let st = setpoint_off_status(&s, &cfg, None);
        assert!(st.enabled && st.ac_on);
        assert!(!st.at_setpoint, "should not be at setpoint");
        assert_eq!(st.on_zones, 1);
        assert_eq!(st.satisfied, 0);
        assert!(st.target_time.is_none(), "no target time while waiting");
    }

    /// When an On AC is in a non-heating/cooling mode the status reports
    /// `mode_eligible = false` (so the card shows the "not active for this
    /// mode" note) and no countdown, even with the zone reading at its
    /// setpoint.
    #[test]
    fn status_mode_ineligible_for_fan_mode() {
        let mut s = snap_with_zone(23.0, 23.0); // at setpoint in Cool...
        s.acs.get_mut(&0).unwrap().status.as_mut().unwrap().mode = Some("Fan");
        let cfg = AutomationConfig {
            setpoint_off_enabled: true,
            ..AutomationConfig::default()
        };
        let st = setpoint_off_status(&s, &cfg, None);
        assert!(st.enabled && st.ac_on);
        assert!(!st.mode_eligible, "Fan mode -> not eligible");
        assert!(!st.at_setpoint, "ineligible mode -> not at setpoint");
        assert!(st.target_time.is_none(), "no countdown when ineligible");
    }

    /// Once the condition holds and the countdown is running, the status
    /// reports the fixed wall-clock shutoff time (since + hold) as "HH:MM".
    #[test]
    fn status_target_time_when_at_setpoint() {
        let s = snap_with_zone(23.0, 23.0); // satisfied.
        let cfg = AutomationConfig {
            setpoint_off_enabled: true,
            setpoint_off_hold: Duration::from_secs(15 * 60),
            ..AutomationConfig::default()
        };
        let since = Instant::now().checked_sub(Duration::from_secs(5 * 60));
        let st = setpoint_off_status(&s, &cfg, since);
        assert!(st.at_setpoint, "should be at setpoint");
        let t = st.target_time.expect("target time should be set");
        assert!(t.len() == 5 && t.as_bytes()[2] == b':', "HH:MM shape: {t}");
    }

    /// The target time is fixed at `since + hold` and does not drift as time
    /// passes (so the card is not re-rendered every minute).
    #[test]
    fn status_target_time_is_stable() {
        let s = snap_with_zone(23.0, 23.0);
        let cfg = AutomationConfig {
            setpoint_off_enabled: true,
            setpoint_off_hold: Duration::from_secs(15 * 60),
            ..AutomationConfig::default()
        };
        let since = Instant::now().checked_sub(Duration::from_secs(5 * 60));
        let a = setpoint_off_status(&s, &cfg, since).target_time;
        std::thread::sleep(Duration::from_millis(50));
        let b = setpoint_off_status(&s, &cfg, since).target_time;
        assert_eq!(a, b, "target time must not drift as time passes");
    }

    /// When the hold has fully elapsed the target time is None (the engine
    /// fires on its next tick); the card falls back to a time-less message
    /// rather than showing a past clock time.
    #[test]
    fn status_target_time_none_when_held() {
        let s = snap_with_zone(23.0, 23.0);
        let cfg = AutomationConfig {
            setpoint_off_enabled: true,
            setpoint_off_hold: Duration::from_secs(15 * 60),
            ..AutomationConfig::default()
        };
        // Countdown started well before the hold period.
        let since = Instant::now().checked_sub(Duration::from_secs(20 * 60));
        let st = setpoint_off_status(&s, &cfg, since);
        assert!(st.at_setpoint);
        assert!(
            st.target_time.is_none(),
            "no target time once the hold has elapsed"
        );
    }

    /// The idle status reports a wall-clock target time shaped "HH:MM" when
    /// the program is enabled and an AC is on, and None otherwise. The target
    /// is `last_change + timeout`; here we check it is Some and well-formed.
    #[test]
    fn idle_status_target_time_when_enabled_and_ac_on() {
        let s = snap_with_zone(23.0, 23.0); // AC on.
        let cfg = AutomationConfig {
            idle_off_enabled: true,
            idle_off_timeout: Duration::from_secs(30 * 60),
            ..AutomationConfig::default()
        };
        let lc = Instant::now();
        let st = idle_off_status(&s, &cfg, Some(lc));
        assert!(st.enabled && st.ac_on);
        let t = st.target_time.expect("target time should be set");
        assert!(t.len() == 5 && t.as_bytes()[2] == b':', "HH:MM shape: {t}");
    }

    /// No AC on -> no target time (nothing to power off).
    #[test]
    fn idle_status_no_target_when_ac_off() {
        let mut s = snap_with_zone(23.0, 23.0);
        s.acs.get_mut(&0).unwrap().status.as_mut().unwrap().power = Some("Off");
        let cfg = AutomationConfig {
            idle_off_enabled: true,
            ..AutomationConfig::default()
        };
        let st = idle_off_status(&s, &cfg, Some(Instant::now()));
        assert!(st.target_time.is_none(), "no target when AC is off");
    }

    /// Disabled program -> no target time regardless of the countdown.
    #[test]
    fn idle_status_no_target_when_disabled() {
        let s = snap_with_zone(23.0, 23.0);
        let cfg = AutomationConfig {
            idle_off_enabled: false,
            ..AutomationConfig::default()
        };
        let st = idle_off_status(&s, &cfg, Some(Instant::now()));
        assert!(st.target_time.is_none());
    }

    /// The target time is fixed at `last_change + timeout`; it does not shift
    /// as time passes (only on a new control change), so two reads with the
    /// same last_change yield the same formatted time.
    #[test]
    fn idle_status_target_is_stable() {
        let s = snap_with_zone(23.0, 23.0);
        let cfg = AutomationConfig {
            idle_off_enabled: true,
            idle_off_timeout: Duration::from_secs(30 * 60),
            ..AutomationConfig::default()
        };
        let lc = Instant::now();
        let a = idle_off_status(&s, &cfg, Some(lc)).target_time;
        std::thread::sleep(Duration::from_millis(50));
        let b = idle_off_status(&s, &cfg, Some(lc)).target_time;
        assert_eq!(a, b, "target time must not drift as time passes");
    }
}
