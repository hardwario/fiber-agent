// Power management module for FIBER Medical Thermometer
// Handles battery monitoring, power status tracking, and LED control

pub mod status;
pub mod controller;
pub mod monitor;

// Re-export public types
pub use status::{PowerStatus, SharedPowerStatus};
pub use controller::PowerController;
pub use monitor::PowerMonitor;
