// Configuration applier for atomic YAML updates
//
// This module handles applying configuration changes with:
// - Atomic file updates (write to temp, then rename)
// - Automatic backup before changes
// - Validation before applying
// - Rollback on failure
// - EU MDR audit compliance

pub mod applier;
pub mod validation;

pub use applier::{ApplyResult, ConfigApplier};
pub use validation::ConfigValidator;

use std::sync::{Arc, OnceLock};

/// Process-wide handle to the running applier.
///
/// The EYE monitor adopts a discovered tag by writing it into `eye.tags`, and
/// that write must go through the applier so it keeps the backup, validation and
/// audit trail every other config change gets. The monitor is spawned before the
/// MQTT thread that constructs the applier and has no reference to it, so rather
/// than thread one through `EyeMonitor::new` (and every call site between), the
/// applier registers itself here once — mirroring `eye_state_handle` /
/// `eye_config_handle`, which exist for exactly this reason.
static CONFIG_APPLIER: OnceLock<Arc<ConfigApplier>> = OnceLock::new();

/// Register the applier (called once, by the MQTT monitor that builds it).
pub fn register_config_applier(applier: Arc<ConfigApplier>) {
    let _ = CONFIG_APPLIER.set(applier);
}

/// The registered applier, or `None` before the MQTT monitor has started.
pub fn config_applier_handle() -> Option<Arc<ConfigApplier>> {
    CONFIG_APPLIER.get().cloned()
}
