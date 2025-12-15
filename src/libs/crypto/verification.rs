//! Ed25519 signature verification with CA-based certificate chain validation

use super::ca_registry::CARegistry;
use super::certificate::UserCertificate;
use super::error::CryptoError;
use super::nonce::NonceTracker;
use base64::{engine::general_purpose, Engine as _};
use ed25519_dalek::{Signature, Verifier};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Result of successful signature verification
#[derive(Debug, Clone)]
pub struct VerificationResult {
    /// Signer ID (from certificate)
    pub signer_id: String,

    /// Signer full name (from certificate)
    pub signer_name: String,

    /// Signer role (from certificate)
    pub signer_role: String,

    /// Permissions (from certificate)
    pub permissions: Vec<String>,

    /// Certificate issuer (CA)
    pub issuer: String,

    /// Timestamp from command
    pub timestamp: i64,

    /// Nonce from command
    pub nonce: String,

    /// Signature algorithm
    pub algorithm: String,
}

/// Ed25519 signature verifier with CA-based certificate chain validation
pub struct SignatureVerifier {
    /// CA registry (trusted certificate authorities)
    ca_registry: Arc<Mutex<CARegistry>>,

    /// Nonce tracker for replay protection
    nonce_tracker: Arc<Mutex<NonceTracker>>,

    /// Maximum timestamp drift in seconds (±)
    max_timestamp_drift_sec: i64,
}

impl SignatureVerifier {
    /// Create a new signature verifier
    pub fn new(
        ca_registry: Arc<Mutex<CARegistry>>,
        nonce_tracker: Arc<Mutex<NonceTracker>>,
        max_timestamp_drift_sec: i64,
    ) -> Self {
        Self {
            ca_registry,
            nonce_tracker,
            max_timestamp_drift_sec,
        }
    }

    /// Verify a signed command with certificate chain validation
    ///
    /// Verification steps:
    /// 1. Verify certificate is signed by a trusted CA
    /// 2. Check certificate is not expired
    /// 3. Verify command signature with user's public key from certificate
    /// 4. Check timestamp is within allowed drift
    /// 5. Check nonce hasn't been used (replay protection)
    /// 6. Check permission from certificate (if required)
    pub fn verify_signed_command(
        &self,
        message: &str,
        signature_base64: &str,
        certificate: &UserCertificate,
        timestamp: i64,
        nonce: &str,
        required_permission: Option<&str>,
    ) -> Result<VerificationResult, CryptoError> {
        // 1. Verify certificate is signed by a trusted CA
        self.verify_certificate(certificate)?;

        // 2. Check certificate is not expired
        if certificate.is_expired() {
            return Err(CryptoError::SignerExpired {
                signer_id: certificate.signer_id.clone(),
                expired_at: certificate.expires_at.clone(),
            });
        }

        // 3. Get user's public key from certificate
        let user_public_key = certificate.get_verifying_key()?;

        // 4. Validate timestamp (±5 minutes by default)
        let current_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let time_diff = (current_time - timestamp).abs();
        if time_diff > self.max_timestamp_drift_sec {
            return Err(CryptoError::TimestampOutOfRange {
                timestamp,
                drift_sec: self.max_timestamp_drift_sec,
            });
        }

        // 5. Check nonce hasn't been used (replay protection)
        let mut nonce_tracker = self.nonce_tracker.lock().unwrap();
        if nonce_tracker.is_nonce_used(nonce)? {
            return Err(CryptoError::NonceAlreadyUsed(nonce.to_string()));
        }

        // 6. Check permission if required
        if let Some(permission) = required_permission {
            if !certificate.has_permission(permission) {
                return Err(CryptoError::PermissionDenied {
                    signer_id: certificate.signer_id.clone(),
                    required_permission: permission.to_string(),
                });
            }
        }

        // 7. Decode and verify command signature
        let signature_bytes = general_purpose::STANDARD
            .decode(signature_base64)
            .map_err(|e| {
                CryptoError::InvalidSignature(format!("Failed to decode base64 signature: {}", e))
            })?;

        if signature_bytes.len() != 64 {
            return Err(CryptoError::InvalidSignature(format!(
                "Ed25519 signature must be 64 bytes, got {}",
                signature_bytes.len()
            )));
        }

        let signature = Signature::from_bytes(
            signature_bytes
                .as_slice()
                .try_into()
                .map_err(|_| CryptoError::InvalidSignature("Failed to convert to array".into()))?,
        );

        // Verify command signature with user's public key
        user_public_key
            .verify(message.as_bytes(), &signature)
            .map_err(|e| {
                CryptoError::SignatureVerificationFailed(format!(
                    "Command signature verification failed: {}",
                    e
                ))
            })?;

        // 8. Record nonce to prevent replay
        nonce_tracker.record_nonce(nonce, &certificate.signer_id, timestamp)?;

        // 9. Return verification result
        Ok(VerificationResult {
            signer_id: certificate.signer_id.clone(),
            signer_name: certificate.full_name.clone(),
            signer_role: certificate.role.clone(),
            permissions: certificate.permissions.clone(),
            issuer: certificate.issuer.clone(),
            timestamp,
            nonce: nonce.to_string(),
            algorithm: "Ed25519".to_string(),
        })
    }

    /// Verify certificate is signed by a trusted CA
    fn verify_certificate(&self, certificate: &UserCertificate) -> Result<(), CryptoError> {
        let ca_registry = self.ca_registry.lock().unwrap();

        // Try to find the CA by issuer ID first
        if let Some(ca) = ca_registry.get_enabled_ca(&certificate.issuer) {
            return certificate.verify_signature(&ca.ca_public_key_ed25519);
        }

        // If issuer not found by ID, try all enabled CAs
        // (in case certificate uses a different issuer format)
        let enabled_cas = ca_registry.get_all_enabled_cas();
        if enabled_cas.is_empty() {
            return Err(CryptoError::RegistryLoadError(
                "No trusted Certificate Authorities configured".to_string(),
            ));
        }

        for ca in enabled_cas {
            if certificate.verify_signature(&ca.ca_public_key_ed25519).is_ok() {
                return Ok(());
            }
        }

        Err(CryptoError::SignatureVerificationFailed(format!(
            "Certificate not signed by any trusted CA (issuer: {})",
            certificate.issuer
        )))
    }

    /// Reload CA registry from disk
    pub fn reload_registry(&self) -> Result<bool, CryptoError> {
        let mut ca_registry = self.ca_registry.lock().unwrap();
        ca_registry.reload_if_modified()
    }

    /// Cleanup old nonces from tracker
    pub fn cleanup_old_nonces(&self) -> Result<usize, CryptoError> {
        let mut nonce_tracker = self.nonce_tracker.lock().unwrap();
        nonce_tracker.cleanup_old_nonces()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer as _, SigningKey};

    fn create_test_certificate_and_ca() -> (SigningKey, SigningKey, UserCertificate, String) {
        // Generate CA keypair
        let ca_signing_key = SigningKey::generate(&mut rand::thread_rng());
        let ca_public_key_hex = hex::encode(ca_signing_key.verifying_key().to_bytes());

        // Generate user keypair
        let user_signing_key = SigningKey::generate(&mut rand::thread_rng());
        let user_public_key_hex = hex::encode(user_signing_key.verifying_key().to_bytes());

        // Create certificate (without signature first)
        let mut cert = UserCertificate {
            signer_id: "test@example.com".to_string(),
            full_name: "Test User".to_string(),
            role: "Physician".to_string(),
            public_key_ed25519: user_public_key_hex,
            permissions: vec!["set_threshold".to_string(), "get_status".to_string()],
            issued_at: "2024-01-01T00:00:00Z".to_string(),
            expires_at: "2099-12-31T23:59:59Z".to_string(),
            issuer: "test-ca@fiber.com".to_string(),
            certificate_signature: String::new(),
        };

        // Sign certificate with CA key
        let canonical_message = cert.build_canonical_message();
        let signature = ca_signing_key.sign(canonical_message.as_bytes());
        cert.certificate_signature = general_purpose::STANDARD.encode(signature.to_bytes());

        (ca_signing_key, user_signing_key, cert, ca_public_key_hex)
    }

    #[test]
    fn test_certificate_chain_verification() {
        let (_ca_signing_key, user_signing_key, cert, ca_public_key_hex) =
            create_test_certificate_and_ca();

        // Verify certificate signature
        assert!(cert.verify_signature(&ca_public_key_hex).is_ok());

        // Sign a test message with user key
        let message = "test command message";
        let signature = user_signing_key.sign(message.as_bytes());
        let signature_base64 = general_purpose::STANDARD.encode(signature.to_bytes());

        // Verify user signature with public key from certificate
        let user_public_key = cert.get_verifying_key().unwrap();
        let decoded_sig = general_purpose::STANDARD.decode(&signature_base64).unwrap();
        let sig = Signature::from_bytes(decoded_sig.as_slice().try_into().unwrap());
        assert!(user_public_key.verify(message.as_bytes(), &sig).is_ok());
    }

    #[test]
    fn test_permission_check() {
        let (_, _, cert, _) = create_test_certificate_and_ca();

        assert!(cert.has_permission("set_threshold"));
        assert!(cert.has_permission("get_status"));
        assert!(!cert.has_permission("restart_application"));
    }
}
