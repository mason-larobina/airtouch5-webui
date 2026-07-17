//! Canonical, render-ready state (`Snapshot`) and the crate-to-view mapping.
//!
//! `Snapshot` is our own type (the `airtouch5` crate's `CurrentStatus` has
//! private fields and no name/capability data). Every view struct derives
//! `Clone, PartialEq` so the SSE handler can diff old vs new snapshots per id
//! and only re-emit changed fragments.

use std::collections::BTreeMap;
use std::net::IpAddr;

use airtouch5::types::Temperature;
use airtouch5::types::status as st;
use airtouch5::types::status::StatusSet;

// ---------------------------------------------------------------------------
// View types
// ---------------------------------------------------------------------------

/// The full render-ready state of the system.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub connected: bool,
    pub console: ConsoleInfo,
    pub acs: BTreeMap<u8, AcView>,
    pub zones: BTreeMap<u8, ZoneView>,
}

// Manual `PartialEq` compares only the diffable fields; `Instant`-style
// metadata (omitted here) is not compared. This powers per-id SSE diffing.
impl PartialEq for Snapshot {
    fn eq(&self, other: &Self) -> bool {
        self.connected == other.connected
            && self.console == other.console
            && self.acs == other.acs
            && self.zones == other.zones
    }
}

impl Snapshot {
    /// Whether any zone has a temperature sensor -- i.e. whether temperature
    /// control is available for the bulk "all zones" bar at all. With no
    /// sensor zones the Temp button is disabled.
    pub fn bulk_temp_available(&self) -> bool {
        self.zones.values().any(|z| z.has_sensor)
    }

    /// The control mode currently in effect across all sensor-equipped zones:
    /// `Temperature` if every sensor zone is in temperature mode (and at least
    /// one exists), otherwise `Airflow`. Sensorless zones can never be
    /// temperature-controlled so they are ignored. This is the default the
    /// zones partial uses for the bulk bar on a fresh / SSE render.
    pub fn bulk_mode(&self) -> BulkModeView {
        let mut any_sensor = false;
        let mut all_temp = true;
        for z in self.zones.values() {
            if z.has_sensor {
                any_sensor = true;
                if !z.is_temp() {
                    all_temp = false;
                }
            }
        }
        if any_sensor && all_temp {
            BulkModeView::Temperature
        } else {
            BulkModeView::Airflow
        }
    }

    /// True if at least one zone belonging to AC `ac_id` is currently on (On or
    /// Turbo). Used by the AC power handler to reject starting an AC while
    /// every one of its zones is off -- the console would otherwise run the
    /// unit with no airflow path.
    pub fn ac_has_open_zone(&self, ac_id: u8) -> bool {
        self.zones
            .values()
            .any(|z| z.ac_id == Some(ac_id) && z.is_on())
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct ConsoleInfo {
    pub name: String,
    pub address: Option<IpAddr>,
    pub airtouch_id: Option<u32>,
    pub console_id: Option<String>,
    pub versions: Vec<String>,
    pub update_available: bool,
    pub ac_count: usize,
    pub zone_count: usize,
}

impl Default for ConsoleInfo {
    fn default() -> Self {
        Self {
            name: "AirTouch 5".to_string(),
            address: None,
            airtouch_id: None,
            console_id: None,
            versions: Vec::new(),
            update_available: false,
            ac_count: 0,
            zone_count: 0,
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct AcView {
    pub id: u8,
    pub name: String,
    pub zone_start_index: u8,
    pub zone_count: u8,
    pub supported_modes: Vec<&'static str>,
    pub supported_fan_speeds: Vec<&'static str>,
    pub setpoint_cool: (u8, u8),
    pub setpoint_heat: (u8, u8),
    pub status: Option<AcStatusView>,
}

/// Live AC status (render-ready). The `setpoint_str`/`setpoint_down`/
/// `setpoint_up` fields are pre-formatted because `Temperature` exposes no
/// numeric accessor; `recompute_setpoint_strings()` rebuilds them from
/// `setpoint`.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct AcStatusView {
    pub power: Option<&'static str>,
    pub mode: Option<&'static str>,
    pub fan_speed: Option<&'static str>,
    pub fan_intelligent_auto: bool,
    pub setpoint: Option<Temperature>,
    pub temperature: Option<Temperature>,
    pub flags: Vec<&'static str>,
    pub error: Option<u16>,
    /// Pre-formatted setpoint strings for the +/- stepper (server computes the
    /// arithmetic because `Temperature` exposes no numeric accessor).
    pub setpoint_str: Option<String>,
    pub setpoint_down: Option<String>,
    pub setpoint_up: Option<String>,
}

#[derive(Clone, PartialEq, Debug)]
pub struct ZoneView {
    pub id: u8,
    pub name: String,
    pub ac_id: Option<u8>,
    pub power: ZonePowerView,
    pub has_sensor: bool,
    pub control_mode: ControlModeView,
    pub airflow_pct: u8,
    pub setpoint: Option<Temperature>,
    pub sensor: Option<SensorView>,
    pub flags: Vec<&'static str>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ZonePowerView {
    Off,
    On,
    Turbo,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ControlModeView {
    Airflow,
    Temperature,
    Unknown,
}

/// The control mode selected on the bulk "all zones" bar. Drives which preset
/// row (airflow percentages vs temperature setpoints) the zones partial shows.
/// On a fresh render it is derived from the live zone states via
/// [`Snapshot::bulk_mode`]; a bulk control-type POST overrides it so the bar
/// reflects the user's last choice even before every zone has reported back.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum BulkModeView {
    Airflow,
    Temperature,
}

impl BulkModeView {
    /// "airflow" or "temperature" -- the string the bulk bar's
    /// `data-bulk-mode` attribute carries so the client-side toggle can
    /// show the matching preset row without a round-trip.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Airflow => "airflow",
            Self::Temperature => "temperature",
        }
    }
    /// True when the bulk bar is in airflow (%) mode.
    pub fn is_airflow(&self) -> bool {
        matches!(self, Self::Airflow)
    }
    /// True when the bulk bar is in temperature mode.
    pub fn is_temperature(&self) -> bool {
        matches!(self, Self::Temperature)
    }
}

#[derive(Clone, PartialEq, Debug)]
pub enum SensorView {
    NotAvailable,
    Temperature(Temperature),
}

// ---------------------------------------------------------------------------
// Crate -> view mapping
// ---------------------------------------------------------------------------

/// Convert a status `AcPower` to a static label.
fn ac_power_str(p: st::AcPower) -> &'static str {
    match p {
        st::AcPower::Off => "Off",
        st::AcPower::On => "On",
        st::AcPower::AwayOff => "AwayOff",
        st::AcPower::AwayOn => "AwayOn",
        st::AcPower::Sleep => "Sleep",
    }
}

/// Convert a status `AcMode` to a static label.
fn ac_mode_str(m: st::AcMode) -> &'static str {
    match m {
        st::AcMode::Auto => "Auto",
        st::AcMode::Heat => "Heat",
        st::AcMode::Dry => "Dry",
        st::AcMode::Fan => "Fan",
        st::AcMode::Cool => "Cool",
        st::AcMode::AutoHeat => "AutoHeat",
        st::AcMode::AutoCool => "AutoCool",
    }
}

/// Convert a status `FanSpeed` to a short static label used in the segmented
/// control. `Medium` is shown as "Med" to match the mockup.
fn fan_speed_short(s: st::FanSpeed) -> &'static str {
    match s {
        st::FanSpeed::Auto => "Auto",
        st::FanSpeed::Quiet => "Quiet",
        st::FanSpeed::Low => "Low",
        st::FanSpeed::Medium => "Med",
        st::FanSpeed::High => "High",
        st::FanSpeed::Powerful => "Power",
        st::FanSpeed::Turbo => "Turbo",
    }
}

/// Parse a `Temperature`'s `Display` string back to `f32`.
///
/// `Temperature` has no public numeric accessor (see the crate's
/// `temperature.rs` TODO), so for the rare numeric paths we parse the formatted
/// string. Returns `None` if the temperature is unset/unparseable.
pub(crate) fn temp_to_f32(t: Temperature) -> Option<f32> {
    let s = format!("{}", t);
    s.parse::<f32>().ok()
}

/// Format an f32 as a one-decimal setpoint string ("23.0").
pub(crate) fn fmt_temp(x: f32) -> String {
    format!("{:.1}", x)
}

/// Clamp a setpoint to the protocol's valid range (10.0 - 25.0 C).
pub(crate) fn clamp_setpoint(x: f32) -> f32 {
    x.clamp(10.0, 25.0)
}

// ---------------------------------------------------------------------------
// Static info (capabilities + names) retained across reconnects
// ---------------------------------------------------------------------------

/// Static info gathered once per console connection: capabilities, zone names,
/// and console identity. Live status is merged in to build a `Snapshot`.
#[derive(Clone, Debug, Default)]
pub struct StaticInfo {
    pub console: ConsoleInfo,
    /// AC index -> capability.
    pub caps: BTreeMap<u8, AcCap>,
    /// Zone index -> name.
    pub names: BTreeMap<u8, String>,
}

/// A trimmed copy of `airtouch5`'s `AcCapability` that we own and can store
/// without dragging in the crate's private bitflag types.
#[derive(Clone, Debug)]
pub struct AcCap {
    pub id: u8,
    pub name: String,
    pub zone_start_index: u8,
    pub zone_count: u8,
    pub supported_modes: Vec<&'static str>,
    pub supported_fan_speeds: Vec<&'static str>,
    pub setpoint_cool: (u8, u8),
    pub setpoint_heat: (u8, u8),
}

impl StaticInfo {
    /// Build from a discovery console, already-extracted AC capabilities, zone
    /// names, and console version info.
    ///
    /// The `airtouch5` crate's response wrapper types live in a private module
    /// and so cannot be named from outside the crate; callers extract the
    /// primitive data (using type inference) and pass it here.
    pub fn from_data(
        console: &airtouch5::discovery::Console,
        caps: BTreeMap<u8, AcCap>,
        names: BTreeMap<u8, String>,
        versions: Vec<String>,
        update_available: bool,
    ) -> Self {
        Self {
            console: ConsoleInfo {
                name: console.name.clone(),
                address: Some(console.address),
                airtouch_id: Some(console.airtouch_id),
                console_id: Some(console.console_id.clone()),
                versions,
                update_available,
                ac_count: caps.len(),
                zone_count: names.len(),
            },
            caps,
            names,
        }
    }

    /// Which AC owns the given zone index, derived from capability zone ranges.
    pub fn ac_for_zone(&self, zone_id: u8) -> Option<u8> {
        for cap in self.caps.values() {
            if (cap.zone_start_index..cap.zone_start_index + cap.zone_count).contains(&zone_id) {
                return Some(cap.id);
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Snapshot construction
// ---------------------------------------------------------------------------

/// Build a `Snapshot` by merging static info with live `CurrentStatus`.
pub fn build_snapshot(connected: bool, info: &StaticInfo, status: &st::CurrentStatus) -> Snapshot {
    // AC units: from capabilities (static), with live status folded in.
    let mut acs = BTreeMap::new();
    for (id, cap) in &info.caps {
        let live = status.acs().get(*id);
        acs.insert(*id, build_ac_view(*id, cap, live));
    }

    // Zones: union of named zones and live-status zones.
    let mut zone_ids: std::collections::BTreeSet<u8> = std::collections::BTreeSet::new();
    zone_ids.extend(info.names.keys().copied());
    for (zid, _) in status.zones() {
        zone_ids.insert(*zid);
    }

    let mut zones = BTreeMap::new();
    for zid in zone_ids {
        let name = info
            .names
            .get(&zid)
            .cloned()
            .unwrap_or_else(|| format!("Zone {}", zid));
        let ac_id = info.ac_for_zone(zid);
        let live = status.zones().get(zid);
        zones.insert(zid, build_zone_view(zid, name, ac_id, live));
    }

    Snapshot {
        connected,
        console: info.console.clone(),
        acs,
        zones,
    }
}

fn build_ac_view(id: u8, cap: &AcCap, live: Option<&st::AcStatus>) -> AcView {
    AcView {
        id,
        name: cap.name.clone(),
        zone_start_index: cap.zone_start_index,
        zone_count: cap.zone_count,
        supported_modes: cap.supported_modes.clone(),
        supported_fan_speeds: cap.supported_fan_speeds.clone(),
        setpoint_cool: cap.setpoint_cool,
        setpoint_heat: cap.setpoint_heat,
        status: live.map(build_ac_status_view),
    }
}

fn build_ac_status_view(a: &st::AcStatus) -> AcStatusView {
    let flags: Vec<&'static str> = a.flags.iter_names().map(|(n, _)| n).collect();

    // Pre-compute the +/- setpoint strings. `Temperature` has no numeric
    // accessor, so we parse its Display string, step by 1.0 C, and clamp to
    // the protocol's valid setpoint range.
    let (setpoint_str, setpoint_down, setpoint_up) = match a.setpoint.and_then(temp_to_f32) {
        Some(cur) => {
            let down = fmt_temp(clamp_setpoint(cur - 1.0));
            let up = fmt_temp(clamp_setpoint(cur + 1.0));
            (Some(fmt_temp(cur)), Some(down), Some(up))
        }
        None => (None, None, None),
    };

    AcStatusView {
        power: a.power.map(ac_power_str),
        mode: a.mode.map(ac_mode_str),
        fan_speed: a.fan_speed.map(|(s, _)| fan_speed_short(s)),
        fan_intelligent_auto: a.fan_speed.map(|(_, ia)| ia).unwrap_or(false),
        setpoint: a.setpoint,
        temperature: a.temperature,
        flags,
        error: a.error,
        setpoint_str,
        setpoint_down,
        setpoint_up,
    }
}

fn build_zone_view(id: u8, name: String, ac_id: Option<u8>, live: Option<&st::ZoneStatus>) -> ZoneView {
    let Some(z) = live else {
        // No live status yet: render an idle, sensor-less, off zone.
        return ZoneView {
            id,
            name,
            ac_id,
            power: ZonePowerView::Off,
            has_sensor: false,
            control_mode: ControlModeView::Unknown,
            airflow_pct: 0,
            setpoint: None,
            sensor: None,
            flags: Vec::new(),
        };
    };

    let power = match z.power {
        st::ZonePower::Off => ZonePowerView::Off,
        st::ZonePower::On => ZonePowerView::On,
        st::ZonePower::Turbo => ZonePowerView::Turbo,
    };

    let (control_mode, airflow_pct, setpoint) = match z.control {
        st::ZoneControl::Airflow(pct) => (ControlModeView::Airflow, pct, None),
        st::ZoneControl::Temperature(pct, t) => (ControlModeView::Temperature, pct, Some(t)),
    };

    let (has_sensor, sensor) = match z.sensor_reading {
        st::ZoneSensorReading::NoSensor => (false, None),
        st::ZoneSensorReading::NotAvailable => (true, Some(SensorView::NotAvailable)),
        st::ZoneSensorReading::Temperature(t) => {
            (true, Some(SensorView::Temperature(t)))
        }
    };

    let flags: Vec<&'static str> = z.flags.iter_names().map(|(n, _)| n).collect();

    ZoneView {
        id,
        name,
        ac_id,
        power,
        has_sensor,
        control_mode,
        airflow_pct,
        setpoint,
        sensor,
        flags,
    }
}

/// Parse a setpoint form value ("23.0" / "23") into a `Temperature`, rejecting
/// anything outside the protocol's valid 10.0 - 25.0 C range.
pub fn parse_setpoint(s: &str) -> Result<Temperature, String> {
    let x: f32 = s.trim().parse().map_err(|_| format!("not a number: {s:?}"))?;
    if !(10.0..=25.0).contains(&x) {
        return Err(format!("setpoint must be 10.0 - 25.0 C (got {x})"));
    }
    Ok(Temperature::from_float(x))
}

/// Parse an airflow percentage ("0" - "100").
pub fn parse_airflow(s: &str) -> Result<u8, String> {
    let x: i32 = s.trim().parse().map_err(|_| format!("not a number: {s:?}"))?;
    if !(0..=100).contains(&x) {
        return Err(format!("airflow must be 0 - 100 (got {x})"));
    }
    Ok(x as u8)
}

// ---------------------------------------------------------------------------
// Template helper methods
// ---------------------------------------------------------------------------

impl ConsoleInfo {
    /// "192.168.1.42" or "--".
    pub fn addr_str(&self) -> String {
        self.address
            .map(|a| a.to_string())
            .unwrap_or_else(|| "--".to_string())
    }
    /// "#13" or "--".
    pub fn id_str(&self) -> String {
        self.airtouch_id
            .map(|i| format!("#{i}"))
            .unwrap_or_else(|| "--".to_string())
    }
    /// First firmware version string, or "unknown".
    pub fn fw_str(&self) -> String {
        self.versions.first().cloned().unwrap_or_else(|| "unknown".to_string())
    }
    /// "available" or "up to date".
    pub fn update_str(&self) -> &'static str {
        if self.update_available {
            "available"
        } else {
            "up to date"
        }
    }
}

impl ZoneView {
    /// CSS class for the power toggle: "off" / "on" / "turbo".
    pub fn power_class(&self) -> &'static str {
        match self.power {
            ZonePowerView::Off => "off",
            ZonePowerView::On => "on",
            ZonePowerView::Turbo => "turbo",
        }
    }

    /// True if the zone is on (On or Turbo).
    pub fn is_on(&self) -> bool {
        !matches!(self.power, ZonePowerView::Off)
    }

    /// True if currently in airflow (or unknown) control mode.
    pub fn is_airflow(&self) -> bool {
        matches!(self.control_mode, ControlModeView::Airflow | ControlModeView::Unknown)
    }

    /// True if currently in temperature control mode.
    pub fn is_temp(&self) -> bool {
        matches!(self.control_mode, ControlModeView::Temperature)
    }

    /// Right-side sensor reading text: "24.3 C", "sensor n/a", or "no sensor".
    pub fn sensor_display(&self) -> String {
        match &self.sensor {
            None => "no sensor".to_string(),
            Some(SensorView::NotAvailable) => "sensor n/a".to_string(),
            Some(SensorView::Temperature(t)) => format!("{} C", t),
        }
    }

    /// Stepper value text: "23.0 C" in temp mode, "65%" in airflow mode.
    pub fn value_display(&self) -> String {
        match self.control_mode {
            ControlModeView::Temperature => match self.setpoint {
                Some(t) => format!("{} C", t),
                None => "--".to_string(),
            },
            _ => format!("{}%", self.airflow_pct),
        }
    }

    /// CSS class for a status flag badge: "warn" for LowBattery/Spill,
    /// "turbo" for Turbo, "" otherwise. (Helper for the template; askama
    /// iterates the `Vec<&str>` by reference, yielding `&&str`.)
    pub fn flag_class(&self, f: &str) -> &'static str {
        match f {
            "LowBattery" | "Spill" => "warn",
            "Turbo" => "turbo",
            _ => "",
        }
    }

    /// Glyph to render for a flag in place of its text label, when one is
    /// appropriate. LowBattery uses the low-battery symbol (U+1FAAB) so
    /// the badge reads as a low-battery icon at a glance; Spill uses the
    /// droplet symbol (U+1F4A7). Other flags keep their text label
    /// (returned as None). Only HTML files use Unicode glyphs, so the
    /// symbols live in the template, not the Rust source.
    pub fn flag_glyph(&self, f: &str) -> Option<&'static str> {
        match f {
            "LowBattery" => Some("\u{1FAAB}\u{FE0E}"),
            "Spill" => Some("\u{1F4A7}\u{FE0E}"),
            _ => None,
        }
    }
}

impl AcView {
    pub fn has_status(&self) -> bool {
        self.status.is_some()
    }
    pub fn power(&self) -> Option<&'static str> {
        self.status.as_ref().and_then(|s| s.power)
    }
    pub fn mode(&self) -> Option<&'static str> {
        self.status.as_ref().and_then(|s| s.mode)
    }
    pub fn fan(&self) -> Option<&'static str> {
        self.status.as_ref().and_then(|s| s.fan_speed)
    }
    pub fn fan_int_auto(&self) -> bool {
        self.status.as_ref().map(|s| s.fan_intelligent_auto).unwrap_or(false)
    }
    pub fn power_eq(&self, s: &str) -> bool {
        self.power().is_some_and(|p| p == s)
    }
    pub fn mode_eq(&self, s: &str) -> bool {
        self.mode().is_some_and(|p| p == s)
    }
    /// True when the AC is in any Auto mode variant. The console reports Auto
    /// mode as "Auto", "AutoHeat", or "AutoCool" (its current auto decision),
    /// but the controllable mode is just `Auto` -- the heat/cool split is the
    /// console's own choice, not something we command. All three should light
    /// up the single Auto button so it reads as selected whenever Auto is in
    /// effect.
    pub fn mode_is_auto(&self) -> bool {
        matches!(self.mode(), Some("Auto") | Some("AutoHeat") | Some("AutoCool"))
    }
    pub fn fan_eq(&self, s: &str) -> bool {
        self.fan().is_some_and(|p| p == s)
    }
    pub fn power_is_away(&self) -> bool {
        matches!(self.power(), Some("AwayOff") | Some("AwayOn"))
    }
    pub fn mode_supported(&self, s: &str) -> bool {
        self.supported_modes.contains(&s)
    }
    pub fn fan_supported(&self, s: &str) -> bool {
        self.supported_fan_speeds.contains(&s)
    }

    /// Current temperature text for the setpoint row, e.g. "24.3 C".
    pub fn temp_display(&self) -> String {
        self.status
            .as_ref()
            .and_then(|s| s.temperature)
            .map(|t| format!("{} C", t))
            .unwrap_or_else(|| "--".to_string())
    }
    /// Current setpoint text, e.g. "23.0".
    pub fn setpoint_display(&self) -> String {
        self.status
            .as_ref()
            .and_then(|s| s.setpoint_str.clone())
            .unwrap_or_else(|| "--".to_string())
    }
    /// Decremented setpoint text for the `-` button's hx-vals.
    pub fn setpoint_down(&self) -> String {
        self.status
            .as_ref()
            .and_then(|s| s.setpoint_down.clone())
            .unwrap_or_default()
    }
    /// Incremented setpoint text for the `+` button's hx-vals.
    pub fn setpoint_up(&self) -> String {
        self.status
            .as_ref()
            .and_then(|s| s.setpoint_up.clone())
            .unwrap_or_default()
    }

    /// Recompute the pre-formatted setpoint stepper strings from `status.setpoint`.
    /// Called by the mock controller after mutating a setpoint; the real path
    /// computes them in `build_ac_status_view` from the crate's `AcStatus`.
    pub fn recompute_setpoint_strings(&mut self) {
        if let Some(s) = self.status.as_mut() {
            s.recompute_setpoint_strings();
        }
    }

}

impl AcStatusView {
    /// Rebuild `setpoint_str` / `setpoint_down` / `setpoint_up` from `setpoint`.
    pub fn recompute_setpoint_strings(&mut self) {
        match self.setpoint.and_then(temp_to_f32) {
            Some(cur) => {
                self.setpoint_str = Some(fmt_temp(cur));
                self.setpoint_down = Some(fmt_temp(clamp_setpoint(cur - 1.0)));
                self.setpoint_up = Some(fmt_temp(clamp_setpoint(cur + 1.0)));
            }
            None => {
                self.setpoint_str = None;
                self.setpoint_down = None;
                self.setpoint_up = None;
            }
        }
    }
}
