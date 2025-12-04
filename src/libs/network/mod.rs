//! Network configuration and connectivity module for FIBER
//!
//! Handles Bluetooth and WiFi configuration for the Raspberry Pi CM4.
//! This module provides QR code generation for easy device setup.

pub mod qrcode_generator;

pub use qrcode_generator::QrCodeGenerator;
