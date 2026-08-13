//! Teltonika EYE Sensor (BTSMP1) BLE integration.
//!
//! Two channels (see `docs`/upstream issue fiber-v2/application#4):
//! - **Read** — passive consumption of the sensor's BLE *advertising* data
//!   ([`advertising`]); no connection required.
//! - **Provision** — one-time GATT configuration over a plain (unencrypted)
//!   connection: unlock with the PIN, write the profile, persist to flash
//!   ([`provisioning`]).
//!
//! The [`monitor::BeaconMonitor`] owns a dedicated BlueZ session that scans for
//! configured tags, parses their advertising, auto-provisions a tag on first
//! sight, and feeds readings into the telemetry pipeline — mirroring the
//! structure of the `lorawan` module.

pub mod advertising;
pub mod config;
pub mod en12830;
pub mod monitor;
pub mod provisioning;
pub mod state;

pub use config::{BeaconConfig, BeaconTagConfig};

pub use advertising::{parse_manufacturer_value, BeaconReading, TELTONIKA_COMPANY_ID};
pub use monitor::{BeaconHandle, BeaconMonitor};
pub use provisioning::{BeaconProfile, ProvisionError};
pub use state::{BeaconSensorState, BeaconTagState, SharedBeaconState};
