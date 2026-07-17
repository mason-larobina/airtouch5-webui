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
//!    not trip it.
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
/// program. 15 minutes is the default and matches the original requirement.
pub const SETPOINT_HOLD_PRESETS: &[u64] = &[15, 30, 60];

/// The selectable idle-timeout presets (in minutes) for the idle auto-off
/// program.
pub const IDLE_TIMEOUT_PRESETS: &[u64] = &[15, 30, 60, 120];

/// Default setpoint auto-off hold time.
pub const DEFAULT_SETPOINT_HOLD: Duration = Duration::from_secs(15 * 60);
/// Default idle auto-off timeout.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

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
#[derive(Clone)]
pub struct AutomationStore {
    config: Arc<RwLock<AutomationConfig>>,
    path: Option<PathBuf>,
}

impl AutomationStore {
    /// An in-memory store with no persistence (used by tests; production uses
    /// [`AutomationStore::load`]).
    pub fn new(config: AutomationConfig) -> Self {
        Self {
            config: Arc::new(RwLock::new(config)),
            path: None,
        }
    }

    /// Load the config from `path` if it exists (and is valid JSON); otherwise
    /// start from defaults. Subsequent updates are written back atomically.
    pub fn load(path: PathBuf) -> Self {
        let config = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<AutomationConfig>(&b).ok())
            .unwrap_or_default();
        tracing::info!("automation config loaded from {}", path.display());
        Self {
            config: Arc::new(RwLock::new(config)),
            path: Some(path),
        }
    }

    /// A snapshot copy of the current config.
    pub fn get(&self) -> AutomationConfig {
        self.config.read().expect("config lock poisoned").clone()
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

    /// Enable/disable the setpoint auto-off program.
    pub fn set_setpoint_off_enabled(&self, enabled: bool) -> Result<(), String> {
        self.update(|c| c.setpoint_off_enabled = enabled)
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

    /// Enable/disable the idle auto-off program.
    pub fn set_idle_off_enabled(&self, enabled: bool) -> Result<(), String> {
        self.update(|c| c.idle_off_enabled = enabled)
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
        let mut last_change = Instant::now();
        let mut setpoint_since: Option<Instant> = None;

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
                                last_change = Instant::now();
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
                        match setpoint_since {
                            Some(since) if cond => {
                                if since.elapsed() >= cfg.setpoint_off_hold {
                                    tracing::info!(
                                        "automation: setpoint auto-off firing (held {:?})",
                                        since.elapsed()
                                    );
                                    turn_off_acs(&manager, &snap).await;
                                    setpoint_since = None;
                                }
                            }
                            _ => {
                                setpoint_since = if cond { Some(Instant::now()) } else { None };
                            }
                        }
                    } else {
                        setpoint_since = None;
                    }

                    // (2) Idle auto-off.
                    if cfg.idle_off_enabled
                        && any_ac_on(&snap)
                        && last_change.elapsed() >= cfg.idle_off_timeout
                    {
                        tracing::info!(
                            "automation: idle auto-off firing (idle {:?})",
                            last_change.elapsed()
                        );
                        turn_off_acs(&manager, &snap).await;
                        // Reset so we do not re-fire on the next tick; the
                        // power-off publish will also reset `last_change`
                        // via the changed() arm.
                        last_change = Instant::now();
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

/// The setpoint auto-off condition: every on-zone is in temperature control
/// mode and has reached its setpoint (per the owning AC's mode). Sensorless
/// on-zones, or sensor zones whose reading is unavailable, can never be
/// confirmed "at setpoint" so they fail the condition (safe: we do not turn
/// off). Returns false when there are no on-zones.
fn setpoint_condition(snap: &Snapshot) -> bool {
    let mut any_on = false;
    for z in snap.zones.values().filter(|z| z.is_on()) {
        any_on = true;
        // "only active when using temp control for all on-zones": any on-zone
        // in airflow (or unknown) mode disqualifies the program.
        if !z.is_temp() {
            return false;
        }
        let Some(reading) = zone_reading_f32(z) else {
            return false;
        };
        let Some(setpoint) = z.setpoint.and_then(snapshot::temp_to_f32) else {
            return false;
        };
        let ac_mode = z
            .ac_id
            .and_then(|aid| snap.acs.get(&aid))
            .and_then(|a| a.mode());
        if !zone_satisfied(ac_mode, reading, setpoint) {
            return false;
        }
    }
    any_on
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
        Some("Cool") | Some("AutoCool") | Some("Dry") => reading <= setpoint + SETPOINT_TOLERANCE_C,
        Some("Heat") | Some("AutoHeat") => reading >= setpoint - SETPOINT_TOLERANCE_C,
        // Auto (plain), Fan, or unknown: require it to be at the setpoint.
        _ => (reading - setpoint).abs() <= SETPOINT_TOLERANCE_C,
    }
}

/// A compact, stable string summarising the *control* state of the system --
/// everything a user (or the wall console) could change that should reset the
/// idle timer. Crucially excludes the live sensor readings and AC "now"
/// temperatures, which drift continuously and would otherwise keep the idle
/// timer alive forever.
fn control_fingerprint(snap: &Snapshot) -> String {
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
            "aircon-automation-test-{}-{}.json",
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
}
