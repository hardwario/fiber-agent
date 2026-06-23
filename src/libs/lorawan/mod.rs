//! LoRaWAN gateway integration module
//!
//! Bridges ChirpStack MQTT uplinks from HARDWARIO STICKER sensors
//! into the FIBER MQTT topic hierarchy.

pub mod chirpstack;
pub mod detector;
pub mod monitor;
pub mod provisioning;
pub mod registry;
pub mod state;
pub mod sticker_payload;
pub mod sticker_proto;
pub mod sticker_response;

pub use monitor::{LoRaWANMonitor, LoRaWANHandle};
pub use state::{
    LoRaWANState, LoRaWANSensorState, SharedLoRaWANState, create_shared_lorawan_state,
    SharedLoRaWANSensorConfigs, create_shared_lorawan_sensor_configs,
    SharedFieldThresholdDefaults,
};
pub use detector::detect_gateway;
