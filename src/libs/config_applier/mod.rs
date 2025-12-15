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

pub use applier::{ConfigApplier, ApplyResult};
pub use validation::ConfigValidator;
