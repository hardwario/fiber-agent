//! Network configuration and connectivity module for FIBER
//!
//! Handles Bluetooth and WiFi configuration for the Raspberry Pi CM4.
//! This module provides QR code generation for easy device setup and network status detection.

pub mod qrcode_generator;
pub mod status;

pub use qrcode_generator::QrCodeGenerator;
pub use status::{NetworkStatus, get_network_status};
