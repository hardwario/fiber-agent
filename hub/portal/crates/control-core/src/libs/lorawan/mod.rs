//! LoRaWAN gateway integration module
//!
//! Bridges ChirpStack MQTT uplinks from HARDWARIO NODE sensors
//! into the FIBER MQTT topic hierarchy.

pub mod chirpstack;
pub mod concentratord;
pub mod detector;
pub mod external_gateway;
pub mod monitor;
pub mod node_add;
pub mod node_alarm;
pub mod node_command;
pub mod node_config;
pub mod node_payload;
pub mod node_proto;
pub mod node_reassembly;
pub mod node_remove;
pub mod node_response;
pub mod provisioning;
pub mod registry;
pub mod state;

pub use external_gateway::{add_external_gateway, remove_external_gateway};
pub use node_add::{add_lorawan_node, NodeAddDeps};
pub use node_remove::remove_lorawan_node;

pub use detector::detect_gateway;
pub use monitor::{LoRaWANHandle, LoRaWANMonitor};
pub use state::{
    create_shared_lorawan_sensor_configs, create_shared_lorawan_state, LoRaWANSensorState,
    LoRaWANState, SharedFieldThresholdDefaults, SharedLoRaWANSensorConfigs, SharedLoRaWANState,
};
