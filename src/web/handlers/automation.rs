//! Automation program configuration handlers: `POST /automation/*`.
//!
//! These mutate the shared [`AutomationStore`] (persisting to disk if a path
//! is configured) and re-render the `#automation` partial so the UI reflects
//! the new enable/parameter state immediately. The automation engine reads
//! the same store on its next tick, so a toggle takes effect within one tick
//! interval.

use axum::extract::{Form, State};
use axum::response::Html;

use crate::templates;
use crate::web::error::AppError;
use crate::web::state::AppState;

/// `POST /automation/setpoint-off/toggle` -- `enabled = true | false`.
pub async fn toggle_setpoint_off(
    State(state): State<AppState>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Html<String>, AppError> {
    let enabled = parse_bool(&field(&form, "enabled"))?;
    state
        .automation
        .set_setpoint_off_enabled(enabled)
        .map_err(AppError::msg)?;
    Ok(render(&state))
}

/// `POST /automation/setpoint-off/hold` -- `mins = 15 | 30 | 60`.
pub async fn set_setpoint_off_hold(
    State(state): State<AppState>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Html<String>, AppError> {
    let mins = parse_mins(&field(&form, "mins"))?;
    state
        .automation
        .set_setpoint_off_hold(mins)
        .map_err(AppError::msg)?;
    Ok(render(&state))
}

/// `POST /automation/idle-off/toggle` -- `enabled = true | false`.
pub async fn toggle_idle_off(
    State(state): State<AppState>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Html<String>, AppError> {
    let enabled = parse_bool(&field(&form, "enabled"))?;
    state
        .automation
        .set_idle_off_enabled(enabled)
        .map_err(AppError::msg)?;
    Ok(render(&state))
}

/// `POST /automation/idle-off/timeout` -- `mins = 15 | 30 | 60 | 120`.
pub async fn set_idle_off_timeout(
    State(state): State<AppState>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Html<String>, AppError> {
    let mins = parse_mins(&field(&form, "mins"))?;
    state
        .automation
        .set_idle_off_timeout(mins)
        .map_err(AppError::msg)?;
    Ok(render(&state))
}

fn render(state: &AppState) -> Html<String> {
    let cfg = state.automation.get();
    Html(templates::render_automation(&cfg))
}

fn parse_bool(s: &str) -> Result<bool, AppError> {
    match s.trim() {
        "true" | "1" | "on" => Ok(true),
        "false" | "0" | "off" => Ok(false),
        other => Err(AppError::msg(format!(
            "enabled must be true/false, got {other:?}"
        ))),
    }
}

fn parse_mins(s: &str) -> Result<u64, AppError> {
    s.trim()
        .parse::<u64>()
        .map_err(|_| AppError::msg(format!("mins must be a number, got {s:?}")))
}

fn field(form: &[(String, String)], key: &str) -> String {
    form.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}
