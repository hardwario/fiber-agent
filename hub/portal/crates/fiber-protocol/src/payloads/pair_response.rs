//! `{prefix}/{host}/pair/response` — QoS 2, not retained. mqtt.md "Flows"
//! §"Pairing": the device's reply to a `pair/request`, correlated by
//! `request_id`.
//!
//! `encrypted_private_key` is only present when `success` is `true`;
//! decrypting it into the raw Ed25519 seed is [`crate::pairing::decrypt_pair_response`]'s
//! job, not this struct's — it stays untyped-but-shaped here (base64
//! strings) so a failed pairing (`success: false`, `error: Some(_)`,
//! `encrypted_private_key: None`) still deserializes cleanly.

use serde::{Deserialize, Serialize};

/// The base64 `{ciphertext, salt, nonce}` block mqtt.md documents —
/// see [`crate::pairing::decrypt_pair_response`] for the exact KDF/cipher
/// parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedPrivateKey {
    pub ciphertext: String,
    pub salt: String,
    pub nonce: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairResponseMsg {
    pub request_id: String,
    pub success: bool,
    pub encrypted_private_key: Option<EncryptedPrivateKey>,
    pub ca_public_key: Option<String>,
    pub ca_id: Option<String>,
    pub admin_certificate: Option<serde_json::Value>,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_success_response() {
        let json = r#"{
            "request_id": "req-1", "success": true,
            "encrypted_private_key": {"ciphertext": "Y2lw", "salt": "c2Fs", "nonce": "bm9u"},
            "ca_public_key": "aa", "ca_id": "ca-1",
            "admin_certificate": {"signer_id": "admin"}, "error": null
        }"#;
        let msg: PairResponseMsg = serde_json::from_str(json).unwrap();
        assert!(msg.success);
        assert_eq!(msg.encrypted_private_key.unwrap().salt, "c2Fs");
        assert_eq!(msg.ca_id.as_deref(), Some("ca-1"));
    }

    #[test]
    fn deserializes_failure_response() {
        let json = r#"{
            "request_id": "req-1", "success": false,
            "encrypted_private_key": null, "ca_public_key": null, "ca_id": null,
            "admin_certificate": null, "error": "wrong pairing code"
        }"#;
        let msg: PairResponseMsg = serde_json::from_str(json).unwrap();
        assert!(!msg.success);
        assert!(msg.encrypted_private_key.is_none());
        assert_eq!(msg.error.as_deref(), Some("wrong pairing code"));
    }
}
