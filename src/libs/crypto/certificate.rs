//! User certificate handling for CA-based trust model
//!
//! Users present certificates signed by a trusted CA. The device verifies:
//! 1. Certificate signature is valid (signed by CA)
//! 2. Certificate is not expired
//! 3. User's command signature is valid (signed by user's key from certificate)
//! 4. User has required permission (from certificate)

use super::error::CryptoError;
use base64::{engine::general_purpose, Engine as _};
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// User certificate issued by CA
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserCertificate {
    /// Unique signer identifier (email address)
    pub signer_id: String,

    /// Full name of the signer
    pub full_name: String,

    /// Role (Physician, Pharmacist, Engineer, etc.)
    pub role: String,

    /// User's Ed25519 public key (hex-encoded, 64 hex characters = 32 bytes)
    pub public_key_ed25519: String,

    /// Permissions granted to this user
    pub permissions: Vec<String>,

    /// When this certificate was issued (RFC3339)
    pub issued_at: String,

    /// When this certificate expires (RFC3339)
    pub expires_at: String,

    /// CA that issued this certificate
    pub issuer: String,

    /// CA's signature over all above fields (base64-encoded Ed25519)
    pub certificate_signature: String,
}

impl UserCertificate {
    /// Build canonical message for certificate signing/verification
    /// This is the message that the CA signs when issuing the certificate
    pub fn build_canonical_message(&self) -> String {
        // Deterministic JSON serialization (alphabetically sorted keys)
        let msg = json!({
            "expires_at": self.expires_at,
            "full_name": self.full_name,
            "issued_at": self.issued_at,
            "issuer": self.issuer,
            "permissions": self.permissions,
            "public_key_ed25519": self.public_key_ed25519,
            "role": self.role,
            "signer_id": self.signer_id,
        });

        serde_json::to_string(&msg).unwrap()
    }

    /// Check if certificate has expired
    pub fn is_expired(&self) -> bool {
        if let Ok(expiry) = DateTime::parse_from_rfc3339(&self.expires_at) {
            return Utc::now() > expiry.with_timezone(&Utc);
        }
        // If we can't parse the date, consider it expired for safety
        true
    }

    /// Check if certificate has a specific permission
    pub fn has_permission(&self, permission: &str) -> bool {
        self.permissions.iter().any(|p| p == permission)
    }

    /// Verify certificate signature against CA public key
    pub fn verify_signature(&self, ca_public_key_hex: &str) -> Result<(), CryptoError> {
        // 1. Decode CA public key from hex
        let ca_key_bytes = hex::decode(ca_public_key_hex).map_err(|e| {
            CryptoError::InvalidPublicKey(format!("Failed to decode CA public key hex: {}", e))
        })?;

        if ca_key_bytes.len() != 32 {
            return Err(CryptoError::InvalidPublicKey(format!(
                "CA public key must be 32 bytes, got {}",
                ca_key_bytes.len()
            )));
        }

        let ca_public_key = VerifyingKey::from_bytes(
            ca_key_bytes
                .as_slice()
                .try_into()
                .map_err(|_| CryptoError::InvalidPublicKey("Failed to convert to array".into()))?,
        )
        .map_err(|e| CryptoError::InvalidPublicKey(format!("Invalid CA Ed25519 public key: {}", e)))?;

        // 2. Decode certificate signature from base64
        let signature_bytes = general_purpose::STANDARD
            .decode(&self.certificate_signature)
            .map_err(|e| {
                CryptoError::InvalidSignature(format!(
                    "Failed to decode certificate signature base64: {}",
                    e
                ))
            })?;

        if signature_bytes.len() != 64 {
            return Err(CryptoError::InvalidSignature(format!(
                "Certificate signature must be 64 bytes, got {}",
                signature_bytes.len()
            )));
        }

        let signature = Signature::from_bytes(
            signature_bytes
                .as_slice()
                .try_into()
                .map_err(|_| CryptoError::InvalidSignature("Failed to convert to array".into()))?,
        );

        // 3. Build canonical message and verify
        let canonical_message = self.build_canonical_message();

        ca_public_key
            .verify(canonical_message.as_bytes(), &signature)
            .map_err(|e| {
                CryptoError::SignatureVerificationFailed(format!(
                    "Certificate signature verification failed: {}",
                    e
                ))
            })?;

        Ok(())
    }

    /// Get the user's public key as bytes
    pub fn get_public_key_bytes(&self) -> Result<[u8; 32], CryptoError> {
        let key_bytes = hex::decode(&self.public_key_ed25519).map_err(|e| {
            CryptoError::InvalidPublicKey(format!("Failed to decode user public key hex: {}", e))
        })?;

        if key_bytes.len() != 32 {
            return Err(CryptoError::InvalidPublicKey(format!(
                "User public key must be 32 bytes, got {}",
                key_bytes.len()
            )));
        }

        key_bytes
            .as_slice()
            .try_into()
            .map_err(|_| CryptoError::InvalidPublicKey("Failed to convert to array".into()))
    }

    /// Get the user's verifying key
    pub fn get_verifying_key(&self) -> Result<VerifyingKey, CryptoError> {
        let key_bytes = self.get_public_key_bytes()?;
        VerifyingKey::from_bytes(&key_bytes)
            .map_err(|e| CryptoError::InvalidPublicKey(format!("Invalid Ed25519 public key: {}", e)))
    }
}

/// Result of successful certificate verification
#[derive(Debug, Clone)]
pub struct CertificateVerificationResult {
    /// Signer ID from certificate
    pub signer_id: String,

    /// Signer full name from certificate
    pub signer_name: String,

    /// Signer role from certificate
    pub signer_role: String,

    /// Permissions from certificate
    pub permissions: Vec<String>,

    /// Certificate issuer (CA)
    pub issuer: String,

    /// When certificate expires
    pub expires_at: String,
}

impl From<&UserCertificate> for CertificateVerificationResult {
    fn from(cert: &UserCertificate) -> Self {
        Self {
            signer_id: cert.signer_id.clone(),
            signer_name: cert.full_name.clone(),
            signer_role: cert.role.clone(),
            permissions: cert.permissions.clone(),
            issuer: cert.issuer.clone(),
            expires_at: cert.expires_at.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer as _, SigningKey};

    fn create_test_certificate() -> (SigningKey, SigningKey, UserCertificate) {
        // Generate CA keypair
        let ca_signing_key = SigningKey::generate(&mut rand::thread_rng());
        let _ca_public_key_hex = hex::encode(ca_signing_key.verifying_key().to_bytes());

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

        (ca_signing_key, user_signing_key, cert)
    }

    #[test]
    fn test_certificate_verification() {
        let (ca_signing_key, _user_signing_key, cert) = create_test_certificate();
        let ca_public_key_hex = hex::encode(ca_signing_key.verifying_key().to_bytes());

        // Should verify successfully
        assert!(cert.verify_signature(&ca_public_key_hex).is_ok());
    }

    #[test]
    fn test_certificate_wrong_ca() {
        let (_ca_signing_key, _user_signing_key, cert) = create_test_certificate();

        // Use a different CA key - should fail
        let wrong_ca = SigningKey::generate(&mut rand::thread_rng());
        let wrong_ca_hex = hex::encode(wrong_ca.verifying_key().to_bytes());

        assert!(cert.verify_signature(&wrong_ca_hex).is_err());
    }

    #[test]
    fn test_certificate_expiry() {
        let (_ca_signing_key, _user_signing_key, mut cert) = create_test_certificate();

        // Not expired
        assert!(!cert.is_expired());

        // Make it expired
        cert.expires_at = "2020-01-01T00:00:00Z".to_string();
        assert!(cert.is_expired());
    }

    #[test]
    fn test_certificate_permissions() {
        let (_ca_signing_key, _user_signing_key, cert) = create_test_certificate();

        assert!(cert.has_permission("set_threshold"));
        assert!(cert.has_permission("get_status"));
        assert!(!cert.has_permission("restart_application"));
    }
}
