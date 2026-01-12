//! MQTT message types for pairing protocol
//!
//! Defines the JSON structures for pairing requests and responses.

use serde::{Deserialize, Serialize};

/// Incoming pairing request from viewer backend
#[derive(Debug, Clone, Deserialize)]
pub struct PairingRequest {
    /// UUID for tracking this request
    pub request_id: String,
    /// Unix timestamp (seconds)
    pub timestamp: i64,
    /// Username for the admin certificate
    pub admin_username: String,
}

/// Admin certificate signed by device CA
#[derive(Debug, Clone, Serialize, Default)]
pub struct AdminCertificate {
    /// Unique identifier for this admin (username)
    pub signer_id: String,
    /// Admin's Ed25519 public key (hex encoded)
    pub public_key_ed25519: String,
    /// Permissions granted (["*"] for full admin)
    pub permissions: Vec<String>,
    /// Certificate validity start (ISO8601/RFC3339)
    pub issued_at: String,
    /// Certificate expiration (ISO8601/RFC3339)
    pub expires_at: String,
    /// CA identifier that signed this cert
    pub issuer: String,
    /// Ed25519 signature of certificate (Base64)
    pub certificate_signature: String,
}

/// Encrypted private key in response
#[derive(Debug, Clone, Serialize, Default)]
pub struct EncryptedKeyResponse {
    /// Base64-encoded ciphertext (32 bytes + 16 byte auth tag)
    pub ciphertext: String,
    /// Base64-encoded PBKDF2 salt (16 bytes)
    pub salt: String,
    /// Base64-encoded AES-GCM nonce (12 bytes)
    pub nonce: String,
}

/// Successful pairing response
#[derive(Debug, Clone, Serialize)]
pub struct PairingResponse {
    /// Original request ID
    pub request_id: String,
    /// Always true for success
    pub success: bool,
    /// Device CA public key (hex-encoded Ed25519)
    pub ca_public_key: String,
    /// Device CA identifier
    pub ca_id: String,
    /// Admin certificate signed by device CA
    pub admin_certificate: AdminCertificate,
    /// Encrypted admin private key
    pub encrypted_private_key: EncryptedKeyResponse,
}

impl PairingResponse {
    /// Create a new successful pairing response
    pub fn new(
        request_id: String,
        ca_public_key: String,
        ca_id: String,
        admin_certificate: AdminCertificate,
        encrypted_private_key: EncryptedKeyResponse,
    ) -> Self {
        Self {
            request_id,
            success: true,
            ca_public_key,
            ca_id,
            admin_certificate,
            encrypted_private_key,
        }
    }
}

/// Error pairing response
#[derive(Debug, Clone, Serialize)]
pub struct PairingError {
    /// Original request ID
    pub request_id: String,
    /// Always false for errors
    pub success: bool,
    /// Error message
    pub error: String,
}

impl PairingError {
    /// Create a new error response
    pub fn new(request_id: String, error: impl Into<String>) -> Self {
        Self {
            request_id,
            success: false,
            error: error.into(),
        }
    }

    /// Error: Not in pairing mode
    pub fn not_in_pairing_mode(request_id: String) -> Self {
        Self::new(request_id, "Not in pairing mode")
    }

    /// Error: Pairing code expired
    pub fn code_expired(request_id: String) -> Self {
        Self::new(request_id, "Pairing code expired")
    }

    /// Error: Pairing already in progress
    pub fn already_processing(request_id: String) -> Self {
        Self::new(request_id, "Pairing already in progress")
    }

    /// Error: Invalid request format
    pub fn invalid_request(request_id: String, details: &str) -> Self {
        Self::new(request_id, format!("Invalid request: {}", details))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_pairing_request() {
        let json = r#"{
            "request_id": "550e8400-e29b-41d4-a716-446655440000",
            "timestamp": 1704067200,
            "admin_username": "admin@fiber.local"
        }"#;

        let request: PairingRequest = serde_json::from_str(json).unwrap();

        assert_eq!(request.request_id, "550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(request.timestamp, 1704067200);
        assert_eq!(request.admin_username, "admin@fiber.local");
    }

    #[test]
    fn test_serialize_pairing_response() {
        let response = PairingResponse {
            request_id: "test-123".to_string(),
            success: true,
            ca_public_key: "abcd1234".to_string(),
            ca_id: "fiber-test-ca@fiber.local".to_string(),
            admin_certificate: AdminCertificate {
                signer_id: "admin@test.com".to_string(),
                public_key_ed25519: "deadbeef".to_string(),
                permissions: vec!["*".to_string()],
                issued_at: "2024-01-01T00:00:00Z".to_string(),
                expires_at: "2025-01-01T00:00:00Z".to_string(),
                issuer: "fiber-test-ca@fiber.local".to_string(),
                certificate_signature: "c2lnbmF0dXJl".to_string(),
            },
            encrypted_private_key: EncryptedKeyResponse {
                ciphertext: "Y2lwaGVydGV4dA==".to_string(),
                salt: "c2FsdA==".to_string(),
                nonce: "bm9uY2U=".to_string(),
            },
        };

        let json = serde_json::to_string(&response).unwrap();

        assert!(json.contains("\"success\":true"));
        assert!(json.contains("\"ca_public_key\":\"abcd1234\""));
        assert!(json.contains("\"signer_id\":\"admin@test.com\""));
    }

    #[test]
    fn test_serialize_pairing_error() {
        let error = PairingError::code_expired("req-456".to_string());

        let json = serde_json::to_string(&error).unwrap();

        assert!(json.contains("\"success\":false"));
        assert!(json.contains("\"error\":\"Pairing code expired\""));
        assert!(json.contains("\"request_id\":\"req-456\""));
    }
}
