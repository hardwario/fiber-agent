//! Pairing response decryption. mqtt.md "Flows" §"Pairing":
//!
//! ```text
//! portal ── {prefix}/{h}/pair/request  ──▶ device
//! device ── {prefix}/{h}/pair/response (QoS 2) ──▶ portal
//! ```
//!
//! The operator's 6-character pairing code is never transmitted — it is
//! the KDF password used to decrypt the `encrypted_private_key` block of
//! the response (`{"ciphertext", "salt", "nonce"}`, all base64) into the
//! raw 32-byte Ed25519 seed for the admin certificate minted during
//! pairing.
//!
//! # Local (server) pairing
//!
//! When the portal pairs the hub it runs *on*, there is no second machine
//! and nobody to read the LCD to: the request carries `"local": true` and
//! the device encrypts under the fixed [`LOCAL_PAIRING_PASSWORD`] instead
//! of the displayed code. That password is public by construction, so it
//! is not a secret — the protection is that the broker is loopback-only
//! plaintext anonymous (the device itself is the trust boundary) and that
//! the operator still has to open the physical pairing window on the LCD
//! before any request is answered. Remote client hubs paired through the
//! same portal keep the typed code.
//!
//! Every parameter here is cross-checked against the device's actual
//! implementation, `application/src/libs/pairing/crypto.rs`, and they
//! agree with mqtt.md exactly — **no discrepancy found**:
//!
//! | Parameter | mqtt.md | Device (`pairing/crypto.rs`) |
//! |---|---|---|
//! | KDF | PBKDF2-HMAC-SHA256 | `pbkdf2_hmac::<Sha256>` (`:118`) |
//! | Iterations | 480,000 | `PBKDF2_ITERATIONS: u32 = 480_000` (`:17`) |
//! | Key length | 32 bytes | `[u8; 32]` (`:116`) |
//! | Cipher | AES-256-GCM | `Aes256Gcm` (`:141`, `:165`) |
//! | AAD | none | `.decrypt(nonce, ciphertext.as_ref())` — no AAD arg (`:167`) |
//! | Plaintext length | exactly 32 bytes (Ed25519 seed) | checked at `:171-173` |
//! | Salt length | 16 bytes | `SALT_LENGTH: usize = 16` (`:20`) |
//! | Nonce length | 12 bytes | `NONCE_LENGTH: usize = 12` (`:23`) |
//!
//! "Derived pubkey vs `ca_public_key` mismatch is logged, not rejected"
//! (mqtt.md) — [`derive_public_hex`] only computes the pubkey; comparing
//! it against the response's `ca_public_key` and deciding whether a
//! mismatch is fatal is the caller's policy call, not this function's.

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Key, Nonce,
};
use ed25519_dalek::SigningKey;
use pbkdf2::pbkdf2_hmac;
use sha2::Sha256;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;

/// PBKDF2 iteration count — matches `pairing/crypto.rs:17` exactly.
const PBKDF2_ITERATIONS: u32 = 480_000;

/// PBKDF2 password used instead of the typed pairing code when the portal
/// pairs the hub it runs on (`"local": true` in `pair/request`).
///
/// Hand-mirrored — this crate and the device agent share no code, the same
/// way `PBKDF2_ITERATIONS` is mirrored. Changing it means changing **all
/// three** copies in lockstep, each guarded by a literal tripwire test:
///
/// - here,
/// - `control-core/src/libs/pairing/crypto.rs::LOCAL_PAIRING_PASSWORD`,
/// - `portal/dev/mock/mock_publisher.py::LOCAL_PAIRING_PASSWORD`.
///
/// The trailing `-v1` is the version handle: a future change to the local
/// scheme picks a new suffix rather than silently rotating this string.
pub const LOCAL_PAIRING_PASSWORD: &str = "proximos-local-pairing-v1";

const SALT_LENGTH: usize = 16;
const NONCE_LENGTH: usize = 12;

#[derive(Debug, thiserror::Error)]
pub enum PairingError {
    #[error("invalid base64 in pairing response: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("salt must be exactly {SALT_LENGTH} bytes, got {0}")]
    SaltLength(usize),
    #[error("nonce must be exactly {NONCE_LENGTH} bytes, got {0}")]
    NonceLength(usize),
    #[error("AES-256-GCM decryption failed (wrong pairing code or corrupted response)")]
    DecryptionFailed,
    #[error("decrypted plaintext must be exactly 32 bytes (Ed25519 seed), got {0}")]
    PlaintextLength(usize),
}

/// Decrypt the `encrypted_private_key` block of a `pair/response` payload
/// into the raw 32-byte Ed25519 seed, using the operator's pairing code
/// as the PBKDF2 password — or [`LOCAL_PAIRING_PASSWORD`] when the request
/// was flagged `local`. See the module doc for the exact KDF/cipher
/// parameters, cross-checked against the device.
pub fn decrypt_pair_response(
    pairing_code: &str,
    salt_b64: &str,
    nonce_b64: &str,
    ciphertext_b64: &str,
) -> Result<[u8; 32], PairingError> {
    let salt = BASE64.decode(salt_b64)?;
    if salt.len() != SALT_LENGTH {
        return Err(PairingError::SaltLength(salt.len()));
    }

    let nonce_bytes = BASE64.decode(nonce_b64)?;
    if nonce_bytes.len() != NONCE_LENGTH {
        return Err(PairingError::NonceLength(nonce_bytes.len()));
    }

    let ciphertext = BASE64.decode(ciphertext_b64)?;

    let mut key = [0u8; 32];
    pbkdf2_hmac::<Sha256>(pairing_code.as_bytes(), &salt, PBKDF2_ITERATIONS, &mut key);

    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    let nonce = Nonce::from_slice(&nonce_bytes);
    let plaintext = cipher
        .decrypt(nonce, ciphertext.as_ref())
        .map_err(|_| PairingError::DecryptionFailed)?;

    let plaintext_len = plaintext.len();
    let seed: [u8; 32] = plaintext
        .try_into()
        .map_err(|_| PairingError::PlaintextLength(plaintext_len))?;
    Ok(seed)
}

/// Derive the hex-encoded Ed25519 public key for a decrypted seed, for
/// comparison against the pairing response's `ca_public_key` (mismatch is
/// logged, not fatal — see module doc).
pub fn derive_public_hex(seed: &[u8; 32]) -> String {
    hex::encode(SigningKey::from_bytes(seed).verifying_key().to_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encrypts with the same primitives, standing in for the device side
    /// (mirrors `pairing/crypto.rs::encrypt_private_key`), so the test
    /// exercises the real decrypt path without needing a live device.
    fn encrypt_for_test(seed: &[u8; 32], pairing_code: &str) -> (String, String, String) {
        use rand::RngCore;

        let mut salt = [0u8; SALT_LENGTH];
        let mut nonce_bytes = [0u8; NONCE_LENGTH];
        rand::thread_rng().fill_bytes(&mut salt);
        rand::thread_rng().fill_bytes(&mut nonce_bytes);

        let mut key = [0u8; 32];
        pbkdf2_hmac::<Sha256>(pairing_code.as_bytes(), &salt, PBKDF2_ITERATIONS, &mut key);

        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = cipher.encrypt(nonce, seed.as_ref()).expect("encrypt");

        (
            BASE64.encode(salt),
            BASE64.encode(nonce_bytes),
            BASE64.encode(ciphertext),
        )
    }

    #[test]
    fn decrypt_roundtrip() {
        let seed = [0xABu8; 32];
        let code = "XY7K9M";
        let (salt, nonce, ciphertext) = encrypt_for_test(&seed, code);

        let decrypted = decrypt_pair_response(code, &salt, &nonce, &ciphertext).unwrap();
        assert_eq!(decrypted, seed);
    }

    #[test]
    fn decrypt_roundtrip_with_local_password() {
        // The local (server) pairing path: the device encrypted under the
        // shared constant, nobody typed anything.
        let seed = [0x5Au8; 32];
        let (salt, nonce, ciphertext) = encrypt_for_test(&seed, LOCAL_PAIRING_PASSWORD);

        let decrypted =
            decrypt_pair_response(LOCAL_PAIRING_PASSWORD, &salt, &nonce, &ciphertext).unwrap();
        assert_eq!(decrypted, seed);
    }

    /// Tripwire: the constant is hand-mirrored into the device agent and the
    /// dev mock, so it may only change deliberately and in all three places
    /// at once (see its doc comment). A drifted copy would otherwise surface
    /// as an opaque "decryption failed" at pairing time.
    #[test]
    fn local_pairing_password_is_the_agreed_literal() {
        assert_eq!(LOCAL_PAIRING_PASSWORD, "proximos-local-pairing-v1");
    }

    #[test]
    fn a_typed_code_cannot_open_a_locally_encrypted_response() {
        let seed = [0x5Au8; 32];
        let (salt, nonce, ciphertext) = encrypt_for_test(&seed, LOCAL_PAIRING_PASSWORD);

        let result = decrypt_pair_response("ABC123", &salt, &nonce, &ciphertext);
        assert!(matches!(result, Err(PairingError::DecryptionFailed)));
    }

    #[test]
    fn wrong_pairing_code_fails() {
        let seed = [0xABu8; 32];
        let (salt, nonce, ciphertext) = encrypt_for_test(&seed, "ABC234");

        let result = decrypt_pair_response("WRONGCODE", &salt, &nonce, &ciphertext);
        assert!(matches!(result, Err(PairingError::DecryptionFailed)));
    }

    #[test]
    fn wrong_salt_length_rejected() {
        let result = decrypt_pair_response(
            "ABC234",
            &BASE64.encode([0u8; 8]),
            &BASE64.encode([0u8; NONCE_LENGTH]),
            &BASE64.encode([0u8; 48]),
        );
        assert!(matches!(result, Err(PairingError::SaltLength(8))));
    }

    #[test]
    fn wrong_nonce_length_rejected() {
        let result = decrypt_pair_response(
            "ABC234",
            &BASE64.encode([0u8; SALT_LENGTH]),
            &BASE64.encode([0u8; 4]),
            &BASE64.encode([0u8; 48]),
        );
        assert!(matches!(result, Err(PairingError::NonceLength(4))));
    }

    #[test]
    fn derive_public_hex_is_64_lowercase_hex_chars() {
        let hex_str = derive_public_hex(&[3u8; 32]);
        assert_eq!(hex_str.len(), 64);
        assert!(hex_str
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn derive_public_hex_deterministic() {
        assert_eq!(derive_public_hex(&[3u8; 32]), derive_public_hex(&[3u8; 32]));
        assert_ne!(derive_public_hex(&[3u8; 32]), derive_public_hex(&[4u8; 32]));
    }
}
