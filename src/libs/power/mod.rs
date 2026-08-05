// Power management module for FIBER Medical Thermometer
// Handles battery monitoring, power status tracking, and LED control

pub mod controller;
pub mod link;
pub mod monitor;
pub mod standby;
pub mod status;

// Re-export public types
pub use controller::PowerController;
pub use monitor::PowerMonitor;
pub use standby::{BootDecision, StandbyMarker};
pub use status::{PowerStatus, SharedPowerStatus};
