// Authorization manager for signed configuration commands
//
// This module implements the EU MDR-compliant challenge-response protocol:
// 1. ConfigRequest arrives → verify signature → create challenge
// 2. Challenge published to MQTT with preview of changes
// 3. ConfigConfirm arrives → verify signature → apply or reject
// 4. Response published with success/error status
// 5. All steps logged to audit trail

pub mod level;
pub mod manager;
pub mod state;

/// P1's acceptance tests: the fixture-driven canonical strings and the
/// level/single-shot behaviour (plan/portal/mqtt/design.md §14 P1).
#[cfg(test)]
mod v2_tests;

pub use level::{Envelope, Level};
pub use manager::AuthorizationManager;
pub use state::{ChallengeState, PendingChallenge};
