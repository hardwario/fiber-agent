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
pub mod mqtt;
pub mod crypto;
pub mod authorization;
pub mod config_applier;
pub mod pairing;
pub mod ble;

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
pub use mqtt::{MqttHandle, MqttMonitor};
pub use pairing::{PairingMonitor, PairingHandle};
