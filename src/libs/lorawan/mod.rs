//! LoRaWAN gateway integration module
//!
//! Bridges ChirpStack MQTT uplinks from HARDWARIO STICKER sensors
//! into the FIBER MQTT topic hierarchy.

pub mod chirpstack;
pub mod detector;
pub mod monitor;
pub mod provisioning;
pub mod state;

pub use monitor::{LoRaWANMonitor, LoRaWANHandle};
pub use state::{LoRaWANState, LoRaWANSensorState, SharedLoRaWANState, create_shared_lorawan_state};
pub use detector::detect_gateway;
