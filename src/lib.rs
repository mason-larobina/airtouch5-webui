//! aircon: a small web server wrapping the [`airtouch5`] crate.
//!
//! Discovers the AirTouch 5 console on the LAN, exposes its state, and lets you
//! control AC units and zones from a browser UI built with htmx, live-updated
//! over SSE (Server-Sent Events). See `ARCHITECTURE.md` for the full
//! architecture.
//!
//! # Library layout
//!
//! - [`manager`] -- the connection actor: owns the `AirTouch5` handle, applies
//!   `Command`s, watches live status, publishes a `Snapshot` via a `watch`
//!   channel. [`manager::ManagerHandle`] is the cheaply-cloneable handle the
//!   web layer renders from and sends controls to.
//! - [`mock`] -- an in-memory controller implementing the same `ManagerHandle`
//!   contract, for e2e tests and the `aircon-mock` binary (manual UI dev
//!   without a console).
//! - [`web`] -- the axum router; [`web::build_router`] is agnostic to whether a
//!   real manager or a mock controller produced the `ManagerHandle`.
//! - [`config`], [`airtouch`], [`templates`] -- support modules.
//!
//! The two binaries (`aircon`, real; `aircon-mock`, mock) are thin entrypoints
//! that parse CLI args, spawn the appropriate controller, and call [`serve`].

pub mod airtouch;
pub mod config;
pub mod manager;
pub mod mock;
pub mod templates;
pub mod web;

use std::net::SocketAddr;
use std::time::Duration;

pub use manager::ManagerHandle;

/// Convenience for the binaries: build the router, bind, and serve, shutting
/// down on the first trigger (Ctrl-C / SIGTERM / optional `--timeout`
/// deadline).
///
/// Shutdown is *immediate*, not graceful: the serve future is dropped, which
/// closes the listener and any in-flight connections. axum's graceful shutdown
/// would instead wait for in-flight requests to finish, and with SSE streams
/// (held open for the life of the page) that effectively never happens -- so a
/// plain Ctrl-C would hang.
pub async fn serve(manager: ManagerHandle, bind: SocketAddr, timeout: Option<Duration>) {
    let app = web::build_router(manager);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {bind}: {e}"));
    tracing::info!("serving on http://{bind}");
    if let Some(t) = timeout {
        tracing::info!("auto-shutdown after {t:?}");
    }
    tokio::select! {
        res = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()) => res.expect("server error"),
        _ = shutdown_signal(timeout) => {
            tracing::info!("shutting down now (closing in-flight requests, e.g. SSE)");
        }
    }
}

/// Await a shutdown trigger: Ctrl-C, SIGTERM, or the `--timeout` deadline (if
/// set). Whichever fires first wins.
pub async fn shutdown_signal(timeout: Option<Duration>) {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    let deadline = async {
        match timeout {
            Some(t) => tokio::time::sleep(t).await,
            None => std::future::pending::<()>().await,
        }
    };

    tokio::select! {
        _ = ctrl_c => tracing::info!("shutdown signal received (Ctrl-C)"),
        _ = terminate => tracing::info!("shutdown signal received (SIGTERM)"),
        _ = deadline => tracing::info!("shutdown: --timeout deadline reached"),
    }
}
