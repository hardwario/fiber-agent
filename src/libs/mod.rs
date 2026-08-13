// Library modules for FIBER Medical Thermometer application logic

pub mod accelerometer;
pub mod alarms;
pub mod authorization;
pub mod ble;
pub mod buzzer;
pub mod config;
pub mod config_applier;
pub mod config_migrations;
pub mod control;
pub mod crypto;
pub mod display;
pub mod eye;
pub mod leds;
pub mod logging;
pub mod lorawan;
pub mod mqtt;
pub mod mqtt_export;
pub mod network;
pub mod pairing;
pub mod power;
pub mod sensors;
pub mod storage;
pub mod system_control;

// Re-export key types for convenience
pub use accelerometer::AccelerometerMonitor;
pub use alarms::{AlarmController, AlarmState, AlarmThreshold};
pub use buzzer::BuzzerController;
pub use config::Config;
pub use display::DisplayMonitor;
pub use leds::{LedMonitor, SharedLedState};
pub use lorawan::{LoRaWANHandle, LoRaWANMonitor};
pub use mqtt::{MqttHandle, MqttMonitor};
pub use network::QrCodeGenerator;
pub use pairing::{PairingHandle, PairingMonitor};
pub use power::{PowerStatus, SharedPowerStatus};
pub use sensors::SensorMonitor;
pub use storage::{StorageHandle, StorageThread};
