//! Teltonika EYE Sensor (BTSMP1) BLE integration.
//!
//! Two channels (see `docs`/upstream issue fiber-v2/application#4):
//! - **Read** — passive consumption of the sensor's BLE *advertising* data
//!   ([`advertising`]); no connection required.
//! - **Provision** — one-time GATT configuration over a plain (unencrypted)
//!   connection: unlock with the PIN, write the profile, persist to flash
//!   ([`provisioning`]).
//!
//! The [`monitor::EyeMonitor`] owns a dedicated BlueZ session that scans for
//! configured tags, parses their advertising, auto-provisions a tag on first
//! sight, and feeds readings into the telemetry pipeline — mirroring the
//! structure of the `lorawan` module.

pub mod advertising;
pub mod config;
pub mod en12830;
pub mod provisioning;
pub mod state;
pub mod monitor;

pub use config::{EyeConfig, EyeTagConfig};

pub use advertising::{parse_manufacturer_value, EyeReading, TELTONIKA_COMPANY_ID};
pub use monitor::{EyeMonitor, EyeHandle};
pub use provisioning::{EyeProfile, ProvisionError};
pub use state::{EyeSensorState, EyeTagState, SharedEyeState};
