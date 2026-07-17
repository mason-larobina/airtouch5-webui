//! AC control handlers: `POST /ac/:id/*` (phase 2).

use axum::extract::{Form, Path, State};
use axum::response::Html;

use airtouch5::types::control::{AcMode, AcPower, FanSpeed};

use crate::manager::command::{AcControlReq, Command};
use crate::manager::snapshot::parse_setpoint;
use crate::templates;
use crate::web::error::AppError;
use crate::web::state::AppState;

/// `POST /ac/:id/power` -- `power = on | off | away | sleep | toggle`.
pub async fn power(
    State(state): State<AppState>,
    Path(id): Path<u8>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Html<String>, AppError> {
    let p = field(&form, "power");
    let ac = match p.as_str() {
        "on" => AcPower::On,
        "off" => AcPower::Off,
        "away" => AcPower::Away,
        "sleep" => AcPower::Sleep,
        "toggle" => AcPower::Toggle,
        other => return Err(AppError::msg(format!("unknown power: {other:?}"))),
    };
    send_ac(state.manager.clone(), id, AcControlReq::Power(ac)).await?;
    render_current_ac(&state.manager, id)
}

/// `POST /ac/:id/mode` -- `mode = auto | heat | dry | fan | cool`.
pub async fn mode(
    State(state): State<AppState>,
    Path(id): Path<u8>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Html<String>, AppError> {
    let m = field(&form, "mode");
    let am = match m.as_str() {
        "auto" => AcMode::Auto,
        "heat" => AcMode::Heat,
        "dry" => AcMode::Dry,
        "fan" => AcMode::Fan,
        "cool" => AcMode::Cool,
        other => return Err(AppError::msg(format!("unknown mode: {other:?}"))),
    };
    send_ac(state.manager.clone(), id, AcControlReq::Mode(am)).await?;
    render_current_ac(&state.manager, id)
}

/// `POST /ac/:id/fan` -- `fan = auto | quiet | low | medium | high | powerful | turbo | intelligentauto`.
pub async fn fan(
    State(state): State<AppState>,
    Path(id): Path<u8>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Html<String>, AppError> {
    let f = field(&form, "fan");
    let fs = match f.as_str() {
        "auto" => FanSpeed::Auto,
        "quiet" => FanSpeed::Quiet,
        "low" => FanSpeed::Low,
        "medium" => FanSpeed::Medium,
        "high" => FanSpeed::High,
        "powerful" => FanSpeed::Powerful,
        "turbo" => FanSpeed::Turbo,
        "intelligentauto" => FanSpeed::IntelligentAuto,
        other => return Err(AppError::msg(format!("unknown fan: {other:?}"))),
    };
    send_ac(state.manager.clone(), id, AcControlReq::FanSpeed(fs)).await?;
    render_current_ac(&state.manager, id)
}

/// `POST /ac/:id/setpoint` -- `temp = <float>`.
pub async fn setpoint(
    State(state): State<AppState>,
    Path(id): Path<u8>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Html<String>, AppError> {
    let t = parse_setpoint(&field(&form, "temp"))?;
    send_ac(state.manager.clone(), id, AcControlReq::Setpoint(t)).await?;
    render_current_ac(&state.manager, id)
}

async fn send_ac(
    manager: crate::manager::ManagerHandle,
    id: u8,
    req: AcControlReq,
) -> Result<(), AppError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    manager
        .cmd_tx
        .send(Command::ControlAc { id, req, reply: tx })
        .await
        .map_err(|_| AppError::msg("manager stopped"))?;
    rx.await
        .map_err(|_| AppError::msg("manager dropped reply"))?
        .map_err(AppError::msg)
}

fn render_current_ac(
    manager: &crate::manager::ManagerHandle,
    id: u8,
) -> Result<Html<String>, AppError> {
    let snap = manager.snapshot_rx.borrow().clone();
    let ac = snap
        .acs
        .get(&id)
        .ok_or_else(|| AppError::msg(format!("ac {id} not found")))?;
    Ok(Html(templates::render_ac(ac)))
}

fn field(form: &[(String, String)], key: &str) -> String {
    form.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}
