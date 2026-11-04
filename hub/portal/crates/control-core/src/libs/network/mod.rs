//! Network configuration and connectivity module for FIBER
//!
//! Handles Bluetooth and WiFi configuration for the Raspberry Pi CM4.
//! This module provides QR code generation for easy device setup and network status detection.

// MOVED (control-ble integration): these four came from `libs/ble/gatt/`. They are the surviving
// `nmcli` exec surface (CP-1 kept it; the BLE terminal and the shell recovery ladder are gone), and
// they belong to the network module rather than to a GATT server this process no longer hosts.
pub mod lan;
pub mod net_error;
pub mod provisioning_session;
pub mod qrcode_generator;
pub mod status;
pub mod time_sync;
pub mod wifi;

pub use provisioning_session::{
    new_shared_provisioning_session, touch_shared, ProvisioningSession, SharedProvisioningSession,
    DEFAULT_SESSION_DURATION, IDLE_TIMEOUT,
};
pub use qrcode_generator::QrCodeGenerator;
pub use status::{get_network_status, NetworkStatus};
