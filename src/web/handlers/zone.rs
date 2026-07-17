//! Zone control handlers: `POST /zone/:id/*`.

use axum::extract::{Form, Path, State};
use axum::response::Html;

use airtouch5::types::control::{ZoneControlType, ZoneControlValue, ZonePower};

use crate::manager::command::{Command, ZoneControlReq};
use crate::manager::snapshot::{parse_airflow, parse_setpoint, BulkModeView};
use crate::templates;
use crate::web::error::AppError;
use crate::web::state::AppState;

/// `POST /zone/:id/power` -- form field `power = on | off | turbo | toggle`.
pub async fn power(
    State(state): State<AppState>,
    Path(id): Path<u8>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Html<String>, AppError> {
    let p = field(&form, "power");
    let zpower = match p.as_str() {
        "on" => ZonePower::On,
        "off" => ZonePower::Off,
        "turbo" => ZonePower::Turbo,
        "toggle" => ZonePower::Toggle,
        other => return Err(AppError::msg(format!("unknown power: {other:?}"))),
    };
    send_zone(state.manager.clone(), id, ZoneControlReq::Power(zpower)).await?;
    render_current_zone(&state.manager, id)
}

/// `POST /zone/:id/control-type` -- form field `type = airflow | temperature`.
pub async fn control_type(
    State(state): State<AppState>,
    Path(id): Path<u8>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Html<String>, AppError> {
    let t = field(&form, "type");
    let ct = match t.as_str() {
        "airflow" => ZoneControlType::Airflow,
        "temperature" => ZoneControlType::Temperature,
        other => return Err(AppError::msg(format!("unknown control type: {other:?}"))),
    };
    send_zone(state.manager.clone(), id, ZoneControlReq::SetControlType(ct)).await?;
    render_current_zone(&state.manager, id)
}

/// `POST /zone/:id/step` -- form field `dir = up | down`.
pub async fn step(
    State(state): State<AppState>,
    Path(id): Path<u8>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Html<String>, AppError> {
    let dir = field(&form, "dir");
    let val = match dir.as_str() {
        "up" => ZoneControlValue::Increment,
        "down" => ZoneControlValue::Decrement,
        other => return Err(AppError::msg(format!("unknown dir: {other:?}"))),
    };
    send_zone(state.manager.clone(), id, ZoneControlReq::StepValue(val)).await?;
    render_current_zone(&state.manager, id)
}

/// `POST /zone/:id/airflow` -- form field `pct = 0..100`.
pub async fn airflow(
    State(state): State<AppState>,
    Path(id): Path<u8>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Html<String>, AppError> {
    let pct = parse_airflow(&field(&form, "pct"))?;
    send_zone(state.manager.clone(), id, ZoneControlReq::SetAirflow(pct)).await?;
    render_current_zone(&state.manager, id)
}

/// `POST /zone/:id/setpoint` -- form field `temp = 10.0..25.0`.
pub async fn setpoint(
    State(state): State<AppState>,
    Path(id): Path<u8>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Html<String>, AppError> {
    let t = parse_setpoint(&field(&form, "temp"))?;
    send_zone(state.manager.clone(), id, ZoneControlReq::SetTemperature(t)).await?;
    render_current_zone(&state.manager, id)
}

/// `POST /zones/control-type` -- form field `type = airflow | temperature`.
/// Switches every zone's control mode. Temperature is applied only to zones
/// that have a sensor (sensorless zones cannot be temperature-controlled and
/// are left untouched); airflow is applied to every zone. Re-renders the whole
/// zones partial so the bulk bar reflects the newly selected mode.
pub async fn set_all_control_type(
    State(state): State<AppState>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Html<String>, AppError> {
    let t = field(&form, "type");
    let ct = match t.as_str() {
        "airflow" => ZoneControlType::Airflow,
        "temperature" => ZoneControlType::Temperature,
        other => return Err(AppError::msg(format!("unknown control type: {other:?}"))),
    };

    let snap = state.manager.snapshot_rx.borrow().clone();
    let manager = state.manager.clone();
    for (&id, zone) in &snap.zones {
        // Skip sensorless zones when switching to temperature: the protocol
        // rejects it and the per-zone UI disables the Temp button for them.
        if matches!(ct, ZoneControlType::Temperature) && !zone.has_sensor {
            continue;
        }
        send_zone(manager.clone(), id, ZoneControlReq::SetControlType(ct)).await?;
    }

    let bulk_mode = match ct {
        ZoneControlType::Airflow => BulkModeView::Airflow,
        ZoneControlType::Temperature => BulkModeView::Temperature,
        _ => state.manager.snapshot_rx.borrow().bulk_mode(),
    };
    let snap = state.manager.snapshot_rx.borrow().clone();
    Ok(Html(templates::render_zones_with_bulk(&snap, bulk_mode)))
}

/// `POST /zones/preset` -- form fields `mode = airflow | temperature` and
/// `value` (an airflow percentage `0..100` or a temperature setpoint
/// `10.0..25.0`). Sets every zone to that value in the given mode. Temperature
/// is applied only to sensor-equipped zones (and forces temperature mode for
/// them); airflow is applied to every zone. Re-renders the whole zones partial
/// keeping the requested mode active on the bulk bar.
pub async fn set_all_preset(
    State(state): State<AppState>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Html<String>, AppError> {
    let mode = field(&form, "mode");
    let value = field(&form, "value");

    let snap = state.manager.snapshot_rx.borrow().clone();
    let manager = state.manager.clone();
    let bulk_mode = match mode.as_str() {
        "airflow" => {
            let pct = parse_airflow(&value)?;
            for &id in snap.zones.keys() {
                send_zone(manager.clone(), id, ZoneControlReq::SetAirflow(pct)).await?;
            }
            BulkModeView::Airflow
        }
        "temperature" => {
            let t = parse_setpoint(&value)?;
            for (&id, zone) in &snap.zones {
                if !zone.has_sensor {
                    continue;
                }
                send_zone(manager.clone(), id, ZoneControlReq::SetTemperature(t)).await?;
            }
            BulkModeView::Temperature
        }
        other => return Err(AppError::msg(format!("unknown mode: {other:?}"))),
    };

    let snap = state.manager.snapshot_rx.borrow().clone();
    Ok(Html(templates::render_zones_with_bulk(&snap, bulk_mode)))
}

async fn send_zone(
    manager: crate::manager::ManagerHandle,
    id: u8,
    req: ZoneControlReq,
) -> Result<(), AppError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    manager
        .cmd_tx
        .send(Command::ControlZone { id, req, reply: tx })
        .await
        .map_err(|_| AppError::msg("manager stopped"))?;
    rx.await
        .map_err(|_| AppError::msg("manager dropped reply"))?
        .map_err(AppError::msg)
}

fn render_current_zone(
    manager: &crate::manager::ManagerHandle,
    id: u8,
) -> Result<Html<String>, AppError> {
    let snap = manager.snapshot_rx.borrow().clone();
    let zone = snap
        .zones
        .get(&id)
        .ok_or_else(|| AppError::msg(format!("zone {id} not found")))?;
    Ok(Html(templates::render_zone(zone)))
}

fn field(form: &[(String, String)], key: &str) -> String {
    form.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}
