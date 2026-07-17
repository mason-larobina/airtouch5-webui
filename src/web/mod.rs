//! Router builder and module wiring.

pub mod error;
pub mod handlers;
pub mod sse;
pub mod state;

use axum::routing::{get, post};
use axum::Router;
use tower::ServiceBuilder;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

use crate::manager::ManagerHandle;
use state::AppState;

/// Build the full axum router.
pub fn build_router(manager: ManagerHandle) -> Router {
    let state = AppState { manager };

    // Static vendor assets (htmx + sse extension) served with a long-immutable
    // cache. The versioned filenames make this safe.
    let vendor = ServiceBuilder::new()
        .layer(SetResponseHeaderLayer::if_not_present(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("public, max-age=31536000, immutable"),
        ))
        .service(ServeDir::new("static/vendor"));

    Router::new()
        // Pages
        .route("/", get(handlers::pages::index))
        .route("/partials/system", get(handlers::pages::partial_system))
        .route("/partials/acs", get(handlers::pages::partial_acs))
        .route("/partials/acs/{id}", get(handlers::pages::partial_ac))
        .route("/partials/zones", get(handlers::pages::partial_zones))
        .route("/partials/zones/{id}", get(handlers::pages::partial_zone))
        .route("/refresh", post(handlers::pages::refresh))
        // SSE
        .route("/events", get(sse::sse_events))
        // Zone controls
        .route("/zone/{id}/power", post(handlers::zone::power))
        .route("/zone/{id}/control-type", post(handlers::zone::control_type))
        .route("/zone/{id}/step", post(handlers::zone::step))
        .route("/zone/{id}/airflow", post(handlers::zone::airflow))
        .route("/zone/{id}/setpoint", post(handlers::zone::setpoint))
        // AC controls
        .route("/ac/{id}/power", post(handlers::ac::power))
        .route("/ac/{id}/mode", post(handlers::ac::mode))
        .route("/ac/{id}/fan", post(handlers::ac::fan))
        .route("/ac/{id}/setpoint", post(handlers::ac::setpoint))
        // Vendor assets (versioned, immutable cache)
        .nest_service("/vendor", vendor)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|req: &axum::extract::Request| {
                    let method = req.method();
                    let uri = req.uri();
                    tracing::debug_span!("request", %method, %uri)
                }),
        )
        .with_state(state)
}
