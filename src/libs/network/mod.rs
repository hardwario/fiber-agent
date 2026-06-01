//! Network configuration and connectivity module for FIBER
//!
//! Handles Bluetooth and WiFi configuration for the Raspberry Pi CM4.
//! This module provides QR code generation for easy device setup and network status detection.

pub mod provisioning_session;
pub mod qrcode_generator;
pub mod status;

pub use provisioning_session::{
    new_shared_provisioning_session, ProvisioningSession, SharedProvisioningSession,
    DEFAULT_SESSION_DURATION,
};
pub use qrcode_generator::QrCodeGenerator;
pub use status::{NetworkStatus, get_network_status};
