//! Cryptography module for Ed25519 signature verification
//!
//! This module provides:
//! - Ed25519 signature verification for signed MQTT commands
//! - CA-based certificate chain validation
//! - Nonce tracking for replay attack prevention
//! - EU MDR 2017/745 compliance features

pub mod ca_registry;
pub mod certificate;
pub mod error;
pub mod nonce;
pub mod verification;

pub use ca_registry::{CARegistry, CertificateAuthority};
pub use certificate::{CertificateVerificationResult, UserCertificate};
pub use error::CryptoError;
pub use nonce::NonceTracker;
pub use verification::{SignatureVerifier, VerificationResult};
