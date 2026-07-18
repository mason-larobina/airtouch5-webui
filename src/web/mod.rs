//! Router builder and module wiring.

pub mod error;
pub mod handlers;
pub mod log;
pub mod sse;
pub mod static_assets;
pub mod state;
pub mod theme;

use axum::Router;
use axum::routing::{get, post};
use tower_http::trace::TraceLayer;

use crate::automation::AutomationStore;
use crate::manager::ManagerHandle;
use state::AppState;

/// Build the full axum router.
pub fn build_router(manager: ManagerHandle, automation: AutomationStore) -> Router {
    let state = AppState {
        manager,
        automation,
    };

    Router::new()
        // Pages
        .route("/", get(handlers::pages::index))
        .route("/partials/system", get(handlers::pages::partial_system))
        .route("/partials/acs", get(handlers::pages::partial_acs))
        .route("/partials/acs/{id}", get(handlers::pages::partial_ac))
        .route("/partials/zones", get(handlers::pages::partial_zones))
        .route("/partials/zones/{id}", get(handlers::pages::partial_zone))
        .route("/refresh", post(handlers::pages::refresh))
        .route("/theme", post(handlers::pages::set_theme))
        // SSE
        .route("/events", get(sse::sse_events))
        // Zone controls
        .route("/zone/{id}/power", post(handlers::zone::power))
        .route(
            "/zone/{id}/control-type",
            post(handlers::zone::control_type),
        )
        .route(
            "/zone/{id}/control-type/toggle",
            post(handlers::zone::toggle_control_type),
        )
        .route("/zone/{id}/step", post(handlers::zone::step))
        .route("/zone/{id}/airflow", post(handlers::zone::airflow))
        .route("/zone/{id}/setpoint", post(handlers::zone::setpoint))
        // Bulk zone controls (apply to every zone)
        .route("/zones/power", post(handlers::zone::set_all_power))
        .route("/zones/preset", post(handlers::zone::set_all_preset))
        // Automation programs (configure + enable/disable)
        .route(
            "/partials/automation",
            get(handlers::pages::partial_automation),
        )
        .route(
            "/automation/setpoint-off/toggle",
            post(handlers::automation::toggle_setpoint_off),
        )
        .route(
            "/automation/setpoint-off/hold",
            post(handlers::automation::set_setpoint_off_hold),
        )
        .route(
            "/automation/idle-off/toggle",
            post(handlers::automation::toggle_idle_off),
        )
        .route(
            "/automation/idle-off/timeout",
            post(handlers::automation::set_idle_off_timeout),
        )
        // AC controls
        .route("/ac/{id}/power", post(handlers::ac::power))
        .route("/ac/{id}/mode", post(handlers::ac::mode))
        .route("/ac/{id}/fan", post(handlers::ac::fan))
        .route("/ac/{id}/setpoint", post(handlers::ac::setpoint))
        // Static assets (embedded in the binary at compile time via
        // include_bytes!; see src/web/static_assets.rs). Vendor files are
        // versioned and cached long-immutable.
        .route("/vendor/{file}", get(static_assets::vendor))
        .route("/css/{file}", get(static_assets::css))
        .route("/icons/{file}", get(static_assets::icons))
        // Interaction logging: control actions at info (ip + action + result),
        // everything else at debug. Applied as the outermost layer so its
        // elapsed time covers the whole request.
        .layer(axum::middleware::from_fn(log::request_log))
        .layer(
            TraceLayer::new_for_http().make_span_with(|req: &axum::extract::Request| {
                let method = req.method();
                let uri = req.uri();
                tracing::debug_span!("request", %method, %uri)
            }),
        )
        .with_state(state)
}
