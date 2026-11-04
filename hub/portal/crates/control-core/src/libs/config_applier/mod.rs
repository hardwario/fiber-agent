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
/// Exists so any subsystem spawned before the MQTT thread that constructs the
/// applier — and with no reference to it — can still route a config write
/// through the applier (backup, validation, audit trail) without threading a
/// reference through every call site between. Mirrors `beacon_state_handle` /
/// `beacon_config_handle`, which exist for the same reason.
///
/// The beacon monitor no longer uses this: its own scan loop is read-only with
/// respect to `fiber.config.yaml` (the discovery pass only lists candidates,
/// never adopts one — see `auto_discover` in `beacon/config.rs`), and every
/// write to `eye.tags` now comes from an operator action dispatched on the MQTT
/// thread, which already holds a direct reference to the applier.
static CONFIG_APPLIER: OnceLock<Arc<ConfigApplier>> = OnceLock::new();

/// Register the applier (called once, by the MQTT monitor that builds it).
pub fn register_config_applier(applier: Arc<ConfigApplier>) {
    let _ = CONFIG_APPLIER.set(applier);
}

/// The registered applier, or `None` before the MQTT monitor has started.
pub fn config_applier_handle() -> Option<Arc<ConfigApplier>> {
    CONFIG_APPLIER.get().cloned()
}
