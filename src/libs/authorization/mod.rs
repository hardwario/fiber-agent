// Authorization manager for signed configuration commands
//
// This module implements the EU MDR-compliant challenge-response protocol:
// 1. ConfigRequest arrives → verify signature → create challenge
// 2. Challenge published to MQTT with preview of changes
// 3. ConfigConfirm arrives → verify signature → apply or reject
// 4. Response published with success/error status
// 5. All steps logged to audit trail

pub mod manager;
pub mod state;

pub use manager::AuthorizationManager;
pub use state::{ChallengeState, PendingChallenge};
