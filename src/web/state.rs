//! Shared application state for the axum router.

use crate::manager::ManagerHandle;

/// Axum state: a cheaply-cloneable manager handle (a `watch::Receiver` +
/// `mpsc::Sender`).
#[derive(Clone)]
pub struct AppState {
    pub manager: ManagerHandle,
}
