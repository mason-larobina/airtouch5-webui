//! Shared application state for the axum router.

use crate::automation::AutomationStore;
use crate::manager::ManagerHandle;

/// Axum state: a cheaply-cloneable manager handle (a `watch::Receiver` +
/// `mpsc::Sender`) plus the shared automation config store (read by the
/// engine, mutated by the automation UI handlers).
#[derive(Clone)]
pub struct AppState {
    pub manager: ManagerHandle,
    pub automation: AutomationStore,
}
