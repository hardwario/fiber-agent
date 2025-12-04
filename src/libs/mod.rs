// Library modules for FIBER Medical Thermometer application logic

pub mod alarms;
pub mod power;
pub mod config;
pub mod accelerometer;
pub mod sensors;
pub mod buzzer;
pub mod leds;
pub mod logging;
pub mod display;
pub mod network;
pub mod storage;

// Re-export key types for convenience
pub use alarms::{AlarmController, AlarmState, AlarmThreshold};
pub use power::{PowerStatus, SharedPowerStatus};
pub use config::Config;
pub use accelerometer::AccelerometerMonitor;
pub use sensors::SensorMonitor;
pub use buzzer::BuzzerController;
pub use leds::{LedMonitor, SharedLedState};
pub use display::DisplayMonitor;
pub use network::QrCodeGenerator;
pub use storage::{StorageHandle, StorageThread};
