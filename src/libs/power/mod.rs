// Power management module for FIBER Medical Thermometer
// Handles battery monitoring, power status tracking, and LED control

pub mod controller;
pub mod monitor;
pub mod status;

// Re-export public types
pub use controller::PowerController;
pub use monitor::PowerMonitor;
pub use status::{PowerStatus, SharedPowerStatus};
