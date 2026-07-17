//! Page + partial handlers: `GET /` and `GET /partials/*`.

use axum::extract::{Path, State};
use axum::response::Html;

use crate::manager::command::Command;
use crate::templates;
use crate::web::error::AppError;
use crate::web::state::AppState;

/// `GET /` -- full page shell.
pub async fn index(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let snap = state.manager.snapshot_rx.borrow().clone();
    let cfg = state.automation.get();
    Ok(Html(templates::render_index(&snap, &cfg)))
}

/// `GET /partials/system`.
pub async fn partial_system(State(state): State<AppState>) -> Html<String> {
    let snap = state.manager.snapshot_rx.borrow().clone();
    Html(templates::render_system(&snap))
}

/// `GET /partials/acs`.
pub async fn partial_acs(State(state): State<AppState>) -> Html<String> {
    let snap = state.manager.snapshot_rx.borrow().clone();
    Html(templates::render_acs(&snap))
}

/// `GET /partials/ac/:id`.
pub async fn partial_ac(
    State(state): State<AppState>,
    Path(id): Path<u8>,
) -> Result<Html<String>, AppError> {
    let snap = state.manager.snapshot_rx.borrow().clone();
    let ac = snap
        .acs
        .get(&id)
        .ok_or_else(|| AppError::msg(format!("ac {id} not found")))?;
    Ok(Html(templates::render_ac(ac)))
}

/// `GET /partials/zones`.
pub async fn partial_zones(State(state): State<AppState>) -> Html<String> {
    let snap = state.manager.snapshot_rx.borrow().clone();
    Html(templates::render_zones(&snap))
}

/// `GET /partials/automation` -- the automation programs configuration card.
pub async fn partial_automation(State(state): State<AppState>) -> Html<String> {
    let cfg = state.automation.get();
    Html(templates::render_automation(&cfg))
}

/// `GET /partials/zone/:id`.
pub async fn partial_zone(
    State(state): State<AppState>,
    Path(id): Path<u8>,
) -> Result<Html<String>, AppError> {
    let snap = state.manager.snapshot_rx.borrow().clone();
    let zone = snap
        .zones
        .get(&id)
        .ok_or_else(|| AppError::msg(format!("zone {id} not found")))?;
    Ok(Html(templates::render_zone(zone)))
}

/// `POST /refresh` -- re-pull full status, then re-render the system bar.
pub async fn refresh(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    send_refresh(&state.manager).await?;
    let snap = state.manager.snapshot_rx.borrow().clone();
    Ok(Html(templates::render_system(&snap)))
}

/// Send the `Refresh` command and await its reply.
pub async fn send_refresh(manager: &crate::manager::ManagerHandle) -> Result<(), AppError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    manager
        .cmd_tx
        .send(Command::Refresh { reply: tx })
        .await
        .map_err(|_| AppError::msg("manager stopped"))?;
    rx.await
        .map_err(|_| AppError::msg("manager dropped reply"))?
        .map_err(AppError::msg)
}
