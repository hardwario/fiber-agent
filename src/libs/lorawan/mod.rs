//! LoRaWAN gateway integration module
//!
//! Bridges ChirpStack MQTT uplinks from HARDWARIO STICKER sensors
//! into the FIBER MQTT topic hierarchy.

pub mod chirpstack;
pub mod cluster;
pub mod detector;
pub mod monitor;
pub mod provisioning;
pub mod registry;
pub mod state;
pub mod sticker_add;
pub mod sticker_alarm;
pub mod sticker_command;
pub mod sticker_config;
pub mod sticker_payload;
pub mod sticker_proto;
pub mod sticker_reassembly;
pub mod sticker_response;

pub use sticker_add::{add_lorawan_sticker, StickerAddDeps};

pub use detector::detect_gateway;
pub use monitor::{LoRaWANHandle, LoRaWANMonitor};
pub use state::{
    create_shared_lorawan_sensor_configs, create_shared_lorawan_state, LoRaWANSensorState,
    LoRaWANState, SharedFieldThresholdDefaults, SharedLoRaWANSensorConfigs, SharedLoRaWANState,
};
