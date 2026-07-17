//! Zone control handlers: `POST /zone/:id/*`.

use axum::extract::{Form, Path, State};
use axum::response::Html;

use airtouch5::types::control::{ZoneControlType, ZoneControlValue, ZonePower};

use crate::manager::command::{Command, ZoneControlReq};
use crate::manager::snapshot::{parse_airflow, parse_setpoint};
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
