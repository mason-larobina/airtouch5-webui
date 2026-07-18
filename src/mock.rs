//! In-memory mock AirTouch 5 controller for e2e tests and the `airtouch5-controller-webui-mock`
//! binary (manual UI dev without a console).
//!
//! The mock implements the same `ManagerHandle` contract the real connection
//! manager does: it owns a `Snapshot` (our own, fully-constructible type), drives
//! a `tokio::sync::watch::Sender<Snapshot>`, and answers `Command`s from the web
//! layer by mutating the snapshot and re-publishing. No `airtouch5::AirTouch5`
//! handle, no wire protocol. The router/handlers/templates/SSE code is unchanged.
//!
//! `spawn_mock_controller` returns a `(ManagerHandle, MockController)` pair. The
//! `MockController` lets tests inject arbitrary live changes (as if someone
//! adjusted a zone at the wall console) to exercise the SSE dirty-diff path.

use airtouch5::types::control::{
    AcMode, AcPower as AcPowerCmd, FanSpeed as FanSpeedCmd, ZoneControlType, ZoneControlValue,
    ZonePower as ZonePowerCmd,
};
use airtouch5::types::Temperature;
use tokio::sync::{mpsc, watch};

use crate::manager::command::{AcControlReq, Command, ZoneControlReq};
use crate::manager::snapshot::{
    self, AcStatusView, AcView, ControlModeView, SensorView, Snapshot, ZonePowerView, ZoneView,
    clamp_setpoint, temp_to_f32,
};
use crate::manager::ManagerHandle;

/// Spawn the mock controller, returning the handle the web layer binds to plus a
/// `MockController` for injecting live changes (tests only).
pub fn spawn_mock_controller(initial: Snapshot) -> (ManagerHandle, MockController) {
    let (snapshot_tx, snapshot_rx) = watch::channel(initial.clone());
    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(64);
    let (mutate_tx, mutate_rx) = mpsc::channel::<Mutation>(64);

    let handle = tokio::spawn(mock_loop(initial, snapshot_tx, cmd_rx, mutate_rx));
    drop(handle);

    (
        ManagerHandle {
            snapshot_rx,
            cmd_tx,
        },
        MockController {
            mutate_tx,
        },
    )
}

/// A boxed mutation the mock task applies to its owned `Snapshot`.
type Mutation = Box<dyn FnOnce(&mut Snapshot) + Send + 'static>;

/// Handle for injecting live changes into the mock controller (e.g. simulating
/// someone turning a zone off at the wall console). Cheaply cloneable.
#[derive(Clone)]
pub struct MockController {
    mutate_tx: mpsc::Sender<Mutation>,
}

impl MockController {
    /// Inject a mutation applied to the snapshot, then re-publish. Used by tests
    /// to drive the SSE dirty-diff path.
    pub async fn mutate<F>(&self, f: F)
    where
        F: FnOnce(&mut Snapshot) + Send + 'static,
    {
        let _ = self.mutate_tx.send(Box::new(f)).await;
    }

    /// Non-async variant for fire-and-forget injections from a spawned task
    /// (tests that don't want to await the mutation themselves).
    pub fn try_mutate<F>(&self, f: F)
    where
        F: FnOnce(&mut Snapshot) + Send + 'static,
    {
        let _ = self.mutate_tx.try_send(Box::new(f));
    }
}

/// Build a representative initial snapshot (one AC, six zones) mirroring the
/// static mockup. Handy for the `airtouch5-controller-webui-mock` binary and as a test fixture.
pub fn sample_snapshot() -> Snapshot {
    let console = snapshot::ConsoleInfo {
        name: "LivingRoom-AT5".to_string(),
        address: Some("192.168.1.42".parse().expect("valid addr")),
        airtouch_id: Some(13),
        console_id: Some("AT5A-104823".to_string()),
        versions: vec!["v5.3.2".to_string()],
        update_available: true,
        ac_count: 1,
        zone_count: 6,
    };

    let mut ac_status = AcStatusView {
        power: Some("On"),
        mode: Some("Cool"),
        fan_speed: Some("Med"),
        fan_intelligent_auto: false,
        setpoint: Some(Temperature::from_float(23.0)),
        temperature: Some(Temperature::from_float(24.3)),
        flags: Vec::new(),
        error: None,
        setpoint_str: None,
        setpoint_down: None,
        setpoint_up: None,
    };
    ac_status.recompute_setpoint_strings();
    let ac = AcView {
        id: 0,
        name: "Whole House".to_string(),
        zone_start_index: 0,
        zone_count: 6,
        supported_modes: vec!["Auto", "Heat", "Dry", "Fan", "Cool"],
        supported_fan_speeds: vec![
            "Auto", "Quiet", "Low", "Medium", "High", "Powerful", "Turbo",
        ],
        setpoint_cool: (16, 30),
        setpoint_heat: (16, 30),
        status: Some(ac_status),
    };

    let zones: Vec<ZoneView> = vec![
        ZoneView {
            id: 0,
            name: "Living Room".into(),
            ac_id: Some(0),
            power: ZonePowerView::On,
            has_sensor: true,
            control_mode: ControlModeView::Temperature,
            airflow_pct: 0,
            setpoint: Some(Temperature::from_float(23.0)),
            sensor: Some(SensorView::Temperature(Temperature::from_float(24.3))),
            flags: Vec::new(),
        },
        ZoneView {
            id: 1,
            name: "Bedroom".into(),
            ac_id: Some(0),
            power: ZonePowerView::Off,
            has_sensor: false,
            control_mode: ControlModeView::Airflow,
            airflow_pct: 20,
            setpoint: None,
            sensor: None,
            flags: Vec::new(),
        },
        ZoneView {
            id: 2,
            name: "Kitchen".into(),
            ac_id: Some(0),
            power: ZonePowerView::On,
            has_sensor: true,
            control_mode: ControlModeView::Airflow,
            airflow_pct: 80,
            setpoint: None,
            sensor: Some(SensorView::Temperature(Temperature::from_float(25.1))),
            flags: Vec::new(),
        },
        ZoneView {
            id: 3,
            name: "Study".into(),
            ac_id: Some(0),
            power: ZonePowerView::Turbo,
            has_sensor: true,
            control_mode: ControlModeView::Temperature,
            airflow_pct: 0,
            setpoint: Some(Temperature::from_float(21.0)),
            sensor: Some(SensorView::Temperature(Temperature::from_float(22.7))),
            flags: vec!["LowBattery"],
        },
        ZoneView {
            id: 6,
            name: "Garage".into(),
            ac_id: Some(0),
            power: ZonePowerView::Off,
            has_sensor: false,
            control_mode: ControlModeView::Airflow,
            airflow_pct: 0,
            setpoint: None,
            sensor: None,
            flags: Vec::new(),
        },
        ZoneView {
            id: 7,
            name: "Bathroom".into(),
            ac_id: Some(0),
            power: ZonePowerView::On,
            has_sensor: true,
            control_mode: ControlModeView::Airflow,
            airflow_pct: 45,
            setpoint: None,
            sensor: Some(SensorView::NotAvailable),
            flags: vec!["LowBattery", "Spill"],
        },
    ];

    let mut zone_map = std::collections::BTreeMap::new();
    for z in zones {
        zone_map.insert(z.id, z);
    }

    Snapshot {
        connected: true,
        console,
        acs: [(0u8, ac)].into_iter().collect(),
        zones: zone_map,
    }
}

// ---------------------------------------------------------------------------
// Mock task
// ---------------------------------------------------------------------------

async fn mock_loop(
    mut snap: Snapshot,
    snapshot_tx: watch::Sender<Snapshot>,
    mut cmd_rx: mpsc::Receiver<Command>,
    mut mutate_rx: mpsc::Receiver<Mutation>,
) {
    loop {
        tokio::select! {
            // A web-layer command: apply it, reply, and re-publish.
            Some(cmd) = cmd_rx.recv() => {
                handle_command(&mut snap, cmd);
                let _ = snapshot_tx.send(snap.clone());
            }
            // A test-injected live change: apply and re-publish.
            Some(f) = mutate_rx.recv() => {
                f(&mut snap);
                let _ = snapshot_tx.send(snap.clone());
            }
            // Both command channels closed (manager + controller handles
            // dropped, e.g. at shutdown): exit instead of panicking.
            else => break,
        }
    }
}

fn handle_command(snap: &mut Snapshot, cmd: Command) {
    match cmd {
        Command::Refresh { reply } => {
            let _ = reply.send(Ok(()));
        }
        Command::ControlZone { id, req, reply } => {
            let res = apply_zone(snap, id, req);
            let _ = reply.send(res);
        }
        Command::ControlAc { id, req, reply } => {
            let res = apply_ac(snap, id, req);
            let _ = reply.send(res);
        }
    }
}

fn apply_zone(snap: &mut Snapshot, id: u8, req: ZoneControlReq) -> Result<(), String> {
    let zone = snap
        .zones
        .get_mut(&id)
        .ok_or_else(|| format!("zone {id} not found"))?;

    match req {
        ZoneControlReq::Power(p) => match p {
            ZonePowerCmd::Toggle => {
                zone.power = if matches!(zone.power, ZonePowerView::Off) {
                    ZonePowerView::On
                } else {
                    ZonePowerView::Off
                };
            }
            ZonePowerCmd::Off => zone.power = ZonePowerView::Off,
            ZonePowerCmd::On => zone.power = ZonePowerView::On,
            ZonePowerCmd::Turbo => zone.power = ZonePowerView::Turbo,
        },
        ZoneControlReq::SetControlType(t) => match t {
            ZoneControlType::Airflow => zone.control_mode = ControlModeView::Airflow,
            ZoneControlType::Temperature => {
                if !zone.has_sensor {
                    return Err("zone has no sensor; cannot temperature-control".into());
                }
                zone.control_mode = ControlModeView::Temperature;
                if zone.setpoint.is_none() {
                    zone.setpoint = Some(Temperature::from_float(20.0));
                }
            }
            ZoneControlType::Toggle => {
                if !zone.has_sensor {
                    zone.control_mode = ControlModeView::Airflow;
                } else {
                    zone.control_mode = if matches!(zone.control_mode, ControlModeView::Temperature)
                    {
                        ControlModeView::Airflow
                    } else {
                        ControlModeView::Temperature
                    };
                    if zone.setpoint.is_none() {
                        zone.setpoint = Some(Temperature::from_float(20.0));
                    }
                }
            }
        },
        ZoneControlReq::StepValue(v) => {
            // The real AirTouch 5 console treats a relative Increment/Decrement
            // as "the user wants to interact" and powers an OFF zone ON before
            // applying the step. We mirror that here so the mock stays faithful
            // and a handler that (incorrectly) sends StepValue to an off zone
            // shows up as the zone turning on. The web handlers avoid this by
            // sending absolute SetAirflow/SetTemperature values instead.
            if matches!(zone.power, ZonePowerView::Off) {
                zone.power = ZonePowerView::On;
            }
            match v {
                ZoneControlValue::Increment => step_zone(zone, true),
                ZoneControlValue::Decrement => step_zone(zone, false),
                ZoneControlValue::Airflow(pct) => {
                    zone.airflow_pct = pct;
                    zone.control_mode = ControlModeView::Airflow;
                }
                ZoneControlValue::Temperature(t) => {
                    if !zone.has_sensor {
                        return Err("zone has no sensor; cannot temperature-control".into());
                    }
                    zone.setpoint = Some(t);
                    zone.control_mode = ControlModeView::Temperature;
                }
            }
        }
        ZoneControlReq::SetAirflow(pct) => {
            zone.airflow_pct = pct;
            zone.control_mode = ControlModeView::Airflow;
        }
        ZoneControlReq::SetTemperature(t) => {
            if !zone.has_sensor {
                return Err("zone has no sensor; cannot temperature-control".into());
            }
            zone.setpoint = Some(t);
            zone.control_mode = ControlModeView::Temperature;
        }
    }
    Ok(())
}

/// Step a zone's value: +/- 5% in airflow mode, +/- 1.0 C (clamped 10-25) in
/// temperature mode.
fn step_zone(zone: &mut ZoneView, up: bool) {
    match zone.control_mode {
        ControlModeView::Airflow | ControlModeView::Unknown => {
            let n = zone.airflow_pct as i16 + if up { 5 } else { -5 };
            zone.airflow_pct = n.clamp(0, 100) as u8;
        }
        ControlModeView::Temperature => {
            let cur = zone
                .setpoint
                .and_then(temp_to_f32)
                .unwrap_or(20.0);
            let n = if up { cur + 1.0 } else { cur - 1.0 };
            zone.setpoint = Some(Temperature::from_float(clamp_setpoint(n)));
        }
    }
}

fn apply_ac(snap: &mut Snapshot, id: u8, req: AcControlReq) -> Result<(), String> {
    let ac = snap
        .acs
        .get_mut(&id)
        .ok_or_else(|| format!("ac {id} not found"))?;

    match req {
        AcControlReq::Power(p) => {
            let status = ensure_status(ac);
            match p {
                AcPowerCmd::Toggle => {
                    let active = matches!(status.power, Some("On") | Some("Sleep") | Some("AwayOff") | Some("AwayOn"));
                    status.power = Some(if active { "Off" } else { "On" });
                }
                AcPowerCmd::Off => status.power = Some("Off"),
                AcPowerCmd::On => status.power = Some("On"),
                AcPowerCmd::Away => status.power = Some("AwayOff"),
                AcPowerCmd::Sleep => status.power = Some("Sleep"),
            }
        }
        AcControlReq::Mode(m) => {
            let status = ensure_status(ac);
            status.mode = Some(match m {
                AcMode::Auto => "Auto",
                AcMode::Heat => "Heat",
                AcMode::Dry => "Dry",
                AcMode::Fan => "Fan",
                AcMode::Cool => "Cool",
            });
        }
        AcControlReq::FanSpeed(f) => {
            let status = ensure_status(ac);
            let (label, int_auto) = control_fan_to_view(&f);
            status.fan_speed = Some(label);
            status.fan_intelligent_auto = int_auto;
        }
        AcControlReq::Setpoint(t) => {
            let status = ensure_status(ac);
            status.setpoint = Some(t);
            status.recompute_setpoint_strings();
        }
    }
    Ok(())
}

/// Get the AC's live status, creating a default if it has never reported.
fn ensure_status(ac: &mut AcView) -> &mut AcStatusView {
    ac.status.get_or_insert_with(AcStatusView::default)
}

/// Map a control `FanSpeed` to the short status label + IntelligentAuto flag.
fn control_fan_to_view(f: &FanSpeedCmd) -> (&'static str, bool) {
    match f {
        FanSpeedCmd::Auto => ("Auto", false),
        FanSpeedCmd::Quiet => ("Quiet", false),
        FanSpeedCmd::Low => ("Low", false),
        FanSpeedCmd::Medium => ("Med", false),
        FanSpeedCmd::High => ("High", false),
        FanSpeedCmd::Powerful => ("Power", false),
        FanSpeedCmd::Turbo => ("Turbo", false),
        FanSpeedCmd::IntelligentAuto => ("Auto", true),
    }
}
