//! Admin certificate generation
//!
//! Creates Ed25519 certificates for admin users, signed by the device CA.

use super::ca_key::DeviceCaKey;
use super::messages::AdminCertificate;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use chrono::{Duration, Utc};
use serde_json::json;

/// Certificate validity period (1 year)
const CERTIFICATE_VALIDITY_DAYS: i64 = 365;

/// Create an admin certificate signed by the device CA
///
/// The certificate grants full admin permissions ("*") and is valid for 1 year.
pub fn create_admin_certificate(
    admin_username: &str,
    admin_public_key: &[u8; 32],
    ca_key: &DeviceCaKey,
) -> AdminCertificate {
    let now = Utc::now();
    let expires = now + Duration::days(CERTIFICATE_VALIDITY_DAYS);

    let ca_id = ca_key.ca_id();

    // Create certificate without signature first
    let mut cert = AdminCertificate {
        signer_id: admin_username.to_string(),
        public_key_ed25519: hex::encode(admin_public_key),
        permissions: vec!["*".to_string()],
        issued_at: now.to_rfc3339(),
        expires_at: expires.to_rfc3339(),
        issuer: ca_id,
        certificate_signature: String::new(),
    };

    // Create canonical message for signing (sorted keys, no signature field)
    let sign_data = build_canonical_message(&cert);

    // Sign with CA key
    let signature = ca_key.sign(sign_data.as_bytes());
    cert.certificate_signature = BASE64.encode(signature.to_bytes());

    cert
}

/// Build canonical JSON message for signing
///
/// Fields are sorted alphabetically and the signature field is excluded.
fn build_canonical_message(cert: &AdminCertificate) -> String {
    // Use serde_json to ensure consistent serialization
    // Fields must be in alphabetical order for deterministic signing
    let canonical = json!({
        "expires_at": cert.expires_at,
        "issued_at": cert.issued_at,
        "issuer": cert.issuer,
        "permissions": cert.permissions,
        "public_key_ed25519": cert.public_key_ed25519,
        "signer_id": cert.signer_id,
    });

    // Serialize without pretty-printing (compact JSON)
    serde_json::to_string(&canonical).expect("JSON serialization should not fail")
}

/// Verify a certificate signature
///
/// Used for testing and validation.
#[cfg(test)]
pub fn verify_certificate_signature(
    cert: &AdminCertificate,
    ca_public_key: &ed25519_dalek::VerifyingKey,
) -> bool {
    use ed25519_dalek::{Signature, Verifier};

    let canonical = build_canonical_message(cert);

    let sig_bytes = match BASE64.decode(&cert.certificate_signature) {
        Ok(b) => b,
        Err(_) => return false,
    };

    let signature = match Signature::from_slice(&sig_bytes) {
        Ok(s) => s,
        Err(_) => return false,
    };

    ca_public_key.verify(canonical.as_bytes(), &signature).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_create_certificate() {
        let temp_dir = TempDir::new().unwrap();
        let ca_key = DeviceCaKey::load_or_generate(temp_dir.path(), "test-device").unwrap();

        let admin_public_key = [0xABu8; 32];
        let cert = create_admin_certificate("admin@example.com", &admin_public_key, &ca_key);

        assert_eq!(cert.signer_id, "admin@example.com");
        assert_eq!(cert.public_key_ed25519, hex::encode(admin_public_key));
        assert_eq!(cert.permissions, vec!["*".to_string()]);
        assert_eq!(cert.issuer, ca_key.ca_id());
        assert!(!cert.certificate_signature.is_empty());
    }

    #[test]
    fn test_certificate_signature_valid() {
        let temp_dir = TempDir::new().unwrap();
        let ca_key = DeviceCaKey::load_or_generate(temp_dir.path(), "test-device").unwrap();

        let admin_public_key = [0xCDu8; 32];
        let cert = create_admin_certificate("user@test.com", &admin_public_key, &ca_key);

        // Verify signature is valid
        assert!(verify_certificate_signature(&cert, &ca_key.verifying_key()));
    }

    #[test]
    fn test_certificate_tamper_detection() {
        let temp_dir = TempDir::new().unwrap();
        let ca_key = DeviceCaKey::load_or_generate(temp_dir.path(), "test-device").unwrap();

        let admin_public_key = [0xEFu8; 32];
        let mut cert = create_admin_certificate("admin@test.com", &admin_public_key, &ca_key);

        // Tamper with the certificate
        cert.signer_id = "hacker@evil.com".to_string();

        // Signature should no longer be valid
        assert!(!verify_certificate_signature(&cert, &ca_key.verifying_key()));
    }

    #[test]
    fn test_canonical_message_deterministic() {
        let cert = AdminCertificate {
            signer_id: "test@example.com".to_string(),
            public_key_ed25519: "abcd1234".to_string(),
            permissions: vec!["*".to_string()],
            issued_at: "2024-01-01T00:00:00Z".to_string(),
            expires_at: "2025-01-01T00:00:00Z".to_string(),
            issuer: "ca@test.local".to_string(),
            certificate_signature: "should_be_ignored".to_string(),
        };

        let msg1 = build_canonical_message(&cert);
        let msg2 = build_canonical_message(&cert);

        // Should produce identical output
        assert_eq!(msg1, msg2);

        // Signature should NOT be in the message
        assert!(!msg1.contains("certificate_signature"));
        assert!(!msg1.contains("should_be_ignored"));
    }

    #[test]
    fn test_certificate_validity_dates() {
        let temp_dir = TempDir::new().unwrap();
        let ca_key = DeviceCaKey::load_or_generate(temp_dir.path(), "test-device").unwrap();

        let admin_public_key = [0x12u8; 32];
        let cert = create_admin_certificate("admin@test.com", &admin_public_key, &ca_key);

        // Parse dates
        let issued = chrono::DateTime::parse_from_rfc3339(&cert.issued_at).unwrap();
        let expires = chrono::DateTime::parse_from_rfc3339(&cert.expires_at).unwrap();

        // Expires should be ~365 days after issued
        let diff = expires.signed_duration_since(issued);
        assert_eq!(diff.num_days(), CERTIFICATE_VALIDITY_DAYS);
    }
}
