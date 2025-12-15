//! Cryptography error types

use std::fmt;

/// Errors that can occur during cryptographic operations
#[derive(Debug, Clone)]
pub enum CryptoError {
    /// Signature verification failed
    SignatureVerificationFailed(String),

    /// Invalid public key format
    InvalidPublicKey(String),

    /// Invalid signature format
    InvalidSignature(String),

    /// Signer not found in registry
    SignerNotFound(String),

    /// Signer is disabled
    SignerDisabled(String),

    /// Permission denied for this operation
    PermissionDenied { signer_id: String, required_permission: String },

    /// Timestamp is outside valid range
    TimestampOutOfRange { timestamp: i64, drift_sec: i64 },

    /// Nonce has already been used (replay attack)
    NonceAlreadyUsed(String),

    /// Failed to load signer registry
    RegistryLoadError(String),

    /// Failed to access nonce database
    NonceDatabaseError(String),

    /// Signer certificate has expired
    SignerExpired { signer_id: String, expired_at: String },

    /// Invalid configuration
    InvalidConfiguration(String),
}

impl fmt::Display for CryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CryptoError::SignatureVerificationFailed(msg) => {
                write!(f, "Signature verification failed: {}", msg)
            }
            CryptoError::InvalidPublicKey(msg) => {
                write!(f, "Invalid public key: {}", msg)
            }
            CryptoError::InvalidSignature(msg) => {
                write!(f, "Invalid signature format: {}", msg)
            }
            CryptoError::SignerNotFound(signer_id) => {
                write!(f, "Signer not found in registry: {}", signer_id)
            }
            CryptoError::SignerDisabled(signer_id) => {
                write!(f, "Signer is disabled: {}", signer_id)
            }
            CryptoError::PermissionDenied { signer_id, required_permission } => {
                write!(
                    f,
                    "Permission denied: signer '{}' lacks permission '{}'",
                    signer_id, required_permission
                )
            }
            CryptoError::TimestampOutOfRange { timestamp, drift_sec } => {
                write!(
                    f,
                    "Timestamp {} is outside valid range (±{}s)",
                    timestamp, drift_sec
                )
            }
            CryptoError::NonceAlreadyUsed(nonce) => {
                write!(f, "Nonce has already been used (replay attack): {}", nonce)
            }
            CryptoError::RegistryLoadError(msg) => {
                write!(f, "Failed to load signer registry: {}", msg)
            }
            CryptoError::NonceDatabaseError(msg) => {
                write!(f, "Nonce database error: {}", msg)
            }
            CryptoError::SignerExpired { signer_id, expired_at } => {
                write!(f, "Signer '{}' certificate expired at {}", signer_id, expired_at)
            }
            CryptoError::InvalidConfiguration(msg) => {
                write!(f, "Invalid configuration: {}", msg)
            }
        }
    }
}

impl std::error::Error for CryptoError {}
