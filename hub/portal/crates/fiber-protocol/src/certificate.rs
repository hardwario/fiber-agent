//! The per-signer certificate minted by the device CA and attached to
//! signed commands. mqtt.md "The signed envelope" §"Certificate
//! sub-object":
//!
//! ```jsonc
//! { "signer_id", "full_name", "role", "public_key_ed25519": "<64 hex>",
//!   "permissions": [str], "issued_at", "expires_at", "issuer": "device_ca",
//!   "certificate_signature": "<base64 over the other 8 fields, sorted-key compact JSON>" }
//! ```
//!
//! Field types and the signed pre-image are byte-exact with the device
//! agent's actual verifier: `UserCertificate` + `build_canonical_message`
//! in `application/src/libs/crypto/certificate.rs:16-80`, consumed by
//! `SignatureVerifier::verify_signed_command` step 1
//! (`application/src/libs/crypto/verification.rs:85`, `:176-206`) via the
//! CA registry lookup. This is deliberately **not** modeled on the
//! narrower `AdminCertificate` minted only during pairing bootstrap
//! (`application/src/libs/pairing/certificate.rs:19-33`), which omits
//! `full_name`/`role` because the pairing-minted admin has neither yet —
//! that certificate is out of scope here and stays inside the pairing
//! flow.
//!
//! # Fixes a real interop bug versus the previous `envelope::Certificate`
//!
//! The struct this module replaces declared `issued_at`/`expires_at` as
//! `Option<i64>` and `full_name`/`role` as `Option<String>`. The device's
//! actual verifier (`UserCertificate`, `crypto/certificate.rs:16-44`) uses
//! **RFC3339 strings, and all four fields are required** (no `Option`).
//! Concretely, `UserCertificate::is_expired()`
//! (`crypto/certificate.rs:83-89`) calls
//! `DateTime::parse_from_rfc3339(&self.expires_at)` and **treats a parse
//! failure as already expired** — so a portal that sent `expires_at` as a
//! unix-epoch integer (matching the old struct) would have every one of
//! its certificates rejected as expired before the signature is even
//! checked. `CertificatePayload` below uses `String` (RFC3339) and
//! required `full_name`/`role`, matching the device exactly.

use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;

use crate::envelope::canonical_json;

#[derive(Debug, thiserror::Error)]
pub enum CertificateError {
    #[error("invalid hex public key: {0}")]
    Hex(#[from] hex::FromHexError),
    #[error("public key must be exactly 32 bytes, got {0}")]
    KeyLength(usize),
    #[error("invalid Ed25519 public key")]
    InvalidPublicKey,
    #[error("invalid base64 signature: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("signature must be exactly 64 bytes, got {0}")]
    SignatureLength(usize),
    #[error("certificate signature does not verify")]
    InvalidSignature,
}

/// The 8 signed fields, matching `UserCertificate`
/// (`application/src/libs/crypto/certificate.rs:16-44`) field-for-field
/// and type-for-type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CertificatePayload {
    pub signer_id: String,
    pub full_name: String,
    pub role: String,
    /// 64 lowercase hex chars (32-byte Ed25519 public key).
    pub public_key_ed25519: String,
    pub permissions: Vec<String>,
    /// RFC3339 — the device parses this with
    /// `DateTime::parse_from_rfc3339` (`crypto/certificate.rs:84`).
    pub issued_at: String,
    /// RFC3339, same parser.
    pub expires_at: String,
    /// `"device_ca"` per mqtt.md, or whatever issuer id the device's
    /// `CARegistry` looks up (`crypto/verification.rs:180-186`).
    pub issuer: String,
}

/// Wire form: the payload plus the CA's base64 Ed25519 signature over it.
/// This is what rides on `SignedRequest`/`SignedConfirm.certificate`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Certificate {
    #[serde(flatten)]
    pub payload: CertificatePayload,
    pub certificate_signature: String,
}

/// The exact 8-key object `UserCertificate::build_canonical_message`
/// constructs via `BTreeMap<&str, Value>` before
/// `serde_json::to_string(&map)` (`crypto/certificate.rs:52-79`) — field
/// order doesn't matter here since [`canonical_json`] sorts keys itself,
/// but the *set* of fields (no `certificate_signature`) must match
/// exactly.
fn payload_value(payload: &CertificatePayload) -> Value {
    json!({
        "signer_id": payload.signer_id,
        "full_name": payload.full_name,
        "role": payload.role,
        "public_key_ed25519": payload.public_key_ed25519,
        "permissions": payload.permissions,
        "issued_at": payload.issued_at,
        "expires_at": payload.expires_at,
        "issuer": payload.issuer,
    })
}

/// Canonical message the CA signs / the device re-derives to verify —
/// byte-identical to `UserCertificate::build_canonical_message`
/// (`crypto/certificate.rs:49-80`): the 8 fields above, recursively
/// sorted keys, compact JSON, `certificate_signature` excluded.
pub fn canonical_certificate_message(payload: &CertificatePayload) -> String {
    canonical_json(&payload_value(payload))
}

/// Sign a certificate payload with the device CA's Ed25519 seed. Mirrors
/// `create_admin_certificate`'s signing step
/// (`application/src/libs/pairing/certificate.rs:38-42`), generalized to
/// the full `CertificatePayload` shape.
pub fn sign_certificate(ca_seed: &[u8; 32], payload: &CertificatePayload) -> String {
    let signing_key = SigningKey::from_bytes(ca_seed);
    let message = canonical_certificate_message(payload);
    let signature = signing_key.sign(message.as_bytes());
    BASE64.encode(signature.to_bytes())
}

/// Verify a certificate's signature against the issuing CA's hex public
/// key. Mirrors `UserCertificate::verify_signature`
/// (`crypto/certificate.rs:97-157`).
pub fn verify_certificate(
    ca_pub_hex: &str,
    certificate: &Certificate,
) -> Result<(), CertificateError> {
    let key_bytes = hex::decode(ca_pub_hex)?;
    let key_len = key_bytes.len();
    let key_arr: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| CertificateError::KeyLength(key_len))?;
    let ca_key =
        VerifyingKey::from_bytes(&key_arr).map_err(|_| CertificateError::InvalidPublicKey)?;

    let sig_bytes = BASE64.decode(&certificate.certificate_signature)?;
    let sig_len = sig_bytes.len();
    let sig_arr: [u8; 64] = sig_bytes
        .try_into()
        .map_err(|_| CertificateError::SignatureLength(sig_len))?;
    let signature = ed25519_dalek::Signature::from_bytes(&sig_arr);

    let message = canonical_certificate_message(&certificate.payload);
    ca_key
        .verify(message.as_bytes(), &signature)
        .map_err(|_| CertificateError::InvalidSignature)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_payload() -> CertificatePayload {
        CertificatePayload {
            signer_id: "admin@fiber.local".to_string(),
            full_name: "Admin User".to_string(),
            role: "admin".to_string(),
            public_key_ed25519: "aa".repeat(32),
            permissions: vec!["get_status".to_string(), "set_threshold".to_string()],
            issued_at: "2026-01-01T00:00:00Z".to_string(),
            expires_at: "2027-01-01T00:00:00Z".to_string(),
            issuer: "device_ca".to_string(),
        }
    }

    /// Hand-checked against alphabetical key order (expires_at, full_name,
    /// issued_at, issuer, permissions, public_key_ed25519, role,
    /// signer_id) and compact separators — the same shape
    /// `build_canonical_message` produces on the device.
    #[test]
    fn canonical_certificate_message_matches_hand_written_string() {
        let payload = fixture_payload();
        let expected = concat!(
            r#"{"expires_at":"2027-01-01T00:00:00Z","#,
            r#""full_name":"Admin User","#,
            r#""issued_at":"2026-01-01T00:00:00Z","#,
            r#""issuer":"device_ca","#,
            r#""permissions":["get_status","set_threshold"],"#,
            r#""public_key_ed25519":""#,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            r#"","role":"admin","signer_id":"admin@fiber.local"}"#,
        );
        assert_eq!(canonical_certificate_message(&payload), expected);
    }

    #[test]
    fn canonical_message_excludes_signature_field() {
        let payload = fixture_payload();
        let message = canonical_certificate_message(&payload);
        assert!(!message.contains("certificate_signature"));
    }

    /// Golden sign/verify round-trip with a fixed CA seed.
    #[test]
    fn sign_and_verify_roundtrip_fixed_seed() {
        let ca_seed = [7u8; 32];
        let ca_pub_hex = hex::encode(SigningKey::from_bytes(&ca_seed).verifying_key().to_bytes());

        let payload = fixture_payload();
        let signature = sign_certificate(&ca_seed, &payload);
        let certificate = Certificate {
            payload,
            certificate_signature: signature,
        };

        verify_certificate(&ca_pub_hex, &certificate).expect("valid certificate must verify");
    }

    #[test]
    fn tampered_payload_fails_verification() {
        let ca_seed = [7u8; 32];
        let ca_pub_hex = hex::encode(SigningKey::from_bytes(&ca_seed).verifying_key().to_bytes());

        let payload = fixture_payload();
        let signature = sign_certificate(&ca_seed, &payload);
        let mut certificate = Certificate {
            payload,
            certificate_signature: signature,
        };
        certificate
            .payload
            .permissions
            .push("factory_reset".to_string());

        assert!(verify_certificate(&ca_pub_hex, &certificate).is_err());
    }

    #[test]
    fn wrong_ca_fails_verification() {
        let ca_seed = [7u8; 32];
        let other_ca_pub_hex = hex::encode(
            SigningKey::from_bytes(&[9u8; 32])
                .verifying_key()
                .to_bytes(),
        );

        let payload = fixture_payload();
        let signature = sign_certificate(&ca_seed, &payload);
        let certificate = Certificate {
            payload,
            certificate_signature: signature,
        };

        assert!(verify_certificate(&other_ca_pub_hex, &certificate).is_err());
    }
}
