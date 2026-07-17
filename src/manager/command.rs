//! Commands sent from the web layer to the connection manager.

use tokio::sync::oneshot;

use airtouch5::types::control::{AcControl, AcMode, AcPower, FanSpeed, ZoneControl};

/// A request from the web layer to the manager, with a oneshot reply channel.
#[derive(Debug)]
pub enum Command {
    /// Re-pull the full status from the console (the `[refresh]` button).
    Refresh {
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Apply a zone control. The manager folds the post-change status into the
    /// snapshot before replying so the handler renders the new state.
    ControlZone {
        id: u8,
        req: ZoneControlReq,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Apply an AC control. Same fold-before-reply semantics as `ControlZone`.
    ControlAc {
        id: u8,
        req: AcControlReq,
        reply: oneshot::Sender<Result<(), String>>,
    },
}

/// A zone control request built up by the handler layer.
///
/// Each variant maps to one or two fields of `ZoneControl` (the rest are
/// `None`). A combined `SetTemperature` carries both a `control` type and a
/// `value`, which is allowed by the protocol in a single call.
#[derive(Clone, Debug)]
pub enum ZoneControlReq {
    /// Power: on / off / turbo / toggle.
    Power(airtouch5::types::control::ZonePower),
    /// Switch control mode between airflow % and temperature setpoint.
    SetControlType(airtouch5::types::control::ZoneControlType),
    /// Step the value (increment / decrement) in the current mode.
    StepValue(airtouch5::types::control::ZoneControlValue),
    /// Set an airflow percentage directly.
    SetAirflow(u8),
    /// Set a temperature setpoint directly (also forces Temperature mode).
    SetTemperature(airtouch5::types::Temperature),
}

impl ZoneControlReq {
    /// Translate into a `ZoneControl` with the appropriate fields set.
    pub fn to_zone_control(&self) -> ZoneControl {
        match self {
            ZoneControlReq::Power(p) => ZoneControl {
                power: Some(*p),
                control: None,
                value: None,
            },
            ZoneControlReq::SetControlType(t) => ZoneControl {
                power: None,
                control: Some(*t),
                value: None,
            },
            ZoneControlReq::StepValue(v) => ZoneControl {
                power: None,
                control: None,
                value: Some(*v),
            },
            ZoneControlReq::SetAirflow(pct) => ZoneControl {
                power: None,
                control: None,
                value: Some(airtouch5::types::control::ZoneControlValue::Airflow(*pct)),
            },
            ZoneControlReq::SetTemperature(t) => ZoneControl {
                power: None,
                control: Some(airtouch5::types::control::ZoneControlType::Temperature),
                value: Some(airtouch5::types::control::ZoneControlValue::Temperature(*t)),
            },
        }
    }
}

/// An AC control request built up by the handler layer.
#[derive(Clone, Debug)]
pub enum AcControlReq {
    Power(AcPower),
    Mode(AcMode),
    FanSpeed(FanSpeed),
    Setpoint(airtouch5::types::Temperature),
}

impl AcControlReq {
    /// Translate into an `AcControl` with the appropriate fields set.
    pub fn to_ac_control(&self) -> AcControl {
        match self {
            AcControlReq::Power(p) => AcControl {
                power: Some(*p),
                mode: None,
                fan_speed: None,
                setpoint: None,
            },
            AcControlReq::Mode(m) => AcControl {
                power: None,
                mode: Some(*m),
                fan_speed: None,
                setpoint: None,
            },
            AcControlReq::FanSpeed(f) => AcControl {
                power: None,
                mode: None,
                fan_speed: Some(*f),
                setpoint: None,
            },
            AcControlReq::Setpoint(t) => AcControl {
                power: None,
                mode: None,
                fan_speed: None,
                setpoint: Some(*t),
            },
        }
    }
}
