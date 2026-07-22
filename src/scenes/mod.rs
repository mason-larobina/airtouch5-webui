//! User-defined presets: named snapshots of the whole controllable state that
//! can be re-applied on a click.
//!
//! A preset captures, for every AC unit, its power on/off, mode, fan speed and
//! setpoint; and for every zone, whether it is enabled, its control mode
//! (airflow vs temperature), its airflow percentage and its setpoint. Applying
//! a preset replays those values through the normal command path.
//!
//! Presets are surfaced as the "Presets" card above the AC-unit section. The
//! internal identifiers here use `scene` rather than "preset" to avoid a name
//! collision with two existing, unrelated "preset" concepts: the bulk zone
//! quick-values (`POST /zones/preset`) and the automation hold/timeout
//! parameter buttons. The user-facing label stays "Presets".
//!
//! Configuration lives in a [`SceneStore`] (an in-memory list optionally
//! persisted to a JSON file) shared with the web layer, mirroring the
//! [`crate::automation::AutomationStore`] persistence pattern.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crate::manager::snapshot::{Snapshot, fmt_temp, temp_to_f32};

/// The filename for the presets config within the state directory
/// (see [`crate::config::default_state_dir`]).
pub const CONFIG_FILE_NAME: &str = "scenes.json";

/// The persisted list of presets.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct SceneConfig {
    pub scenes: Vec<Scene>,
}

/// A single named preset: a full capture of the controllable state.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Scene {
    pub name: String,
    pub acs: Vec<SceneAc>,
    pub zones: Vec<SceneZone>,
}

/// Captured state for one AC unit. `mode`/`fan` are the lowercase control slugs
/// (matching the `POST /ac/:id/*` handlers), so they map straight back to
/// commands on apply. `setpoint_c` is stored as an `f32` (serde-friendly,
/// unlike `Temperature`), `None` when the unit has no setpoint.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SceneAc {
    pub id: u8,
    pub power: bool,
    pub mode: String,
    pub fan: String,
    pub setpoint_c: Option<f32>,
}

/// Captured state for one zone.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SceneZone {
    pub id: u8,
    pub enabled: bool,
    /// "airflow" or "temperature".
    pub control_mode: String,
    pub airflow_pct: u8,
    pub setpoint_c: Option<f32>,
}

impl SceneConfig {
    /// The name of the preset that exactly matches the current live state, if
    /// any. Used to light up the active tile and enable the Remove button.
    /// Returns the first match when several presets share the same config.
    pub fn active_name(&self, snap: &Snapshot) -> Option<String> {
        self.scenes
            .iter()
            .find(|s| s.matches(snap))
            .map(|s| s.name.clone())
    }
}

impl Scene {
    /// Whether every captured AC and zone value equals the live snapshot, i.e.
    /// the preset is currently "selected". Setpoints are compared at one-decimal
    /// precision to absorb float round-trips; airflow percentage is compared
    /// only in airflow mode and setpoint only in temperature mode.
    pub fn matches(&self, snap: &Snapshot) -> bool {
        for a in &self.acs {
            let Some(live) = snap.acs.get(&a.id) else {
                return false;
            };
            if live.power_on() != a.power
                || live.mode_slug() != a.mode.as_str()
                || live.fan_slug() != a.fan.as_str()
            {
                return false;
            }
            let live_sp = live
                .status
                .as_ref()
                .and_then(|s| s.setpoint)
                .and_then(temp_to_f32);
            if !setpoint_eq(live_sp, a.setpoint_c) {
                return false;
            }
        }
        for z in &self.zones {
            let Some(live) = snap.zones.get(&z.id) else {
                return false;
            };
            let live_mode = if live.is_temp() { "temperature" } else { "airflow" };
            if live.is_on() != z.enabled || live_mode != z.control_mode.as_str() {
                return false;
            }
            if z.control_mode == "temperature" {
                if !setpoint_eq(live.setpoint.and_then(temp_to_f32), z.setpoint_c) {
                    return false;
                }
            } else if live.airflow_pct != z.airflow_pct {
                return false;
            }
        }
        true
    }
}

/// Compare two optional setpoints at one-decimal precision.
fn setpoint_eq(a: Option<f32>, b: Option<f32>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => fmt_temp(x) == fmt_temp(y),
        (None, None) => true,
        _ => false,
    }
}

/// Capture the current live state as a preset with the given name.
pub fn capture_scene(name: String, snap: &Snapshot) -> Scene {
    let acs = snap
        .acs
        .values()
        .filter(|a| a.has_status())
        .map(|a| SceneAc {
            id: a.id,
            power: a.power_on(),
            mode: a.mode_slug().to_string(),
            fan: a.fan_slug().to_string(),
            setpoint_c: a
                .status
                .as_ref()
                .and_then(|s| s.setpoint)
                .and_then(temp_to_f32),
        })
        .collect();
    let zones = snap
        .zones
        .values()
        .map(|z| SceneZone {
            id: z.id,
            enabled: z.is_on(),
            control_mode: if z.is_temp() {
                "temperature".to_string()
            } else {
                "airflow".to_string()
            },
            airflow_pct: z.airflow_pct,
            setpoint_c: z.setpoint.and_then(temp_to_f32),
        })
        .collect();
    Scene { name, acs, zones }
}

/// Shared, cloneable handle holding the preset list plus an optional
/// persistence path. Mirrors [`crate::automation::AutomationStore`]: the web
/// layer mutates via the typed methods, which persist on every change.
#[derive(Clone)]
pub struct SceneStore {
    config: Arc<RwLock<SceneConfig>>,
    path: Option<PathBuf>,
}

impl SceneStore {
    /// An in-memory store with no persistence (used by tests).
    pub fn new(config: SceneConfig) -> Self {
        Self {
            config: Arc::new(RwLock::new(config)),
            path: None,
        }
    }

    /// Load the config from `path` if it exists (and is valid JSON); otherwise
    /// start empty. Subsequent updates are written back atomically.
    pub fn load(path: PathBuf) -> Self {
        let config = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<SceneConfig>(&b).ok())
            .unwrap_or_default();
        tracing::info!("scenes config path: {}", path.display());
        Self {
            config: Arc::new(RwLock::new(config)),
            path: Some(path),
        }
    }

    /// A snapshot copy of the current config.
    pub fn get(&self) -> SceneConfig {
        self.config.read().expect("scenes lock poisoned").clone()
    }

    /// Insert a preset, replacing any existing one with the same name.
    pub fn upsert(&self, scene: Scene) -> Result<(), String> {
        self.update(move |c| {
            if let Some(slot) = c.scenes.iter_mut().find(|s| s.name == scene.name) {
                *slot = scene;
            } else {
                c.scenes.push(scene);
            }
        })
    }

    /// Remove the preset with the given name (no-op if absent).
    pub fn remove(&self, name: &str) -> Result<(), String> {
        self.update(|c| c.scenes.retain(|s| s.name != name))
    }

    /// Apply a mutation and persist it (if a path is set).
    fn update<F>(&self, f: F) -> Result<(), String>
    where
        F: FnOnce(&mut SceneConfig),
    {
        let new = {
            let mut g = self.config.write().expect("scenes lock poisoned");
            f(&mut g);
            g.clone()
        };
        self.persist(&new)
    }

    fn persist(&self, cfg: &SceneConfig) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
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
            .unwrap_or_else(|| "scenes.json.tmp".to_string());
        let tmp = path.with_file_name(tmp_name);
        std::fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock;

    #[test]
    fn capture_then_matches_live() {
        let snap = mock::sample_snapshot();
        let scene = capture_scene("evening".to_string(), &snap);
        assert!(scene.matches(&snap), "a fresh capture must match its source");
        assert_eq!(scene.acs.len(), 1);
        assert_eq!(scene.zones.len(), snap.zones.len());
    }

    #[test]
    fn match_fails_after_ac_mode_change() {
        let mut snap = mock::sample_snapshot();
        let scene = capture_scene("s".to_string(), &snap);
        snap.acs.get_mut(&0).unwrap().status.as_mut().unwrap().mode = Some("Heat");
        assert!(!scene.matches(&snap), "mode change should break the match");
    }

    #[test]
    fn store_upsert_persists_and_overwrites() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "airtouch5-webui-scenes-test-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = SceneStore::load(path.clone());
        let snap = mock::sample_snapshot();
        store.upsert(capture_scene("home".to_string(), &snap)).unwrap();
        // Overwrite with the same name -> still a single preset.
        store.upsert(capture_scene("home".to_string(), &snap)).unwrap();
        let reloaded = SceneStore::load(path.clone());
        assert_eq!(reloaded.get().scenes.len(), 1);
        assert_eq!(reloaded.get().scenes[0].name, "home");
        // Remove clears it.
        store.remove("home").unwrap();
        assert_eq!(SceneStore::load(path.clone()).get().scenes.len(), 0);
        let _ = std::fs::remove_file(&path);
    }
}
