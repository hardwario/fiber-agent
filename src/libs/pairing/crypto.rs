//! Cryptographic operations for pairing protocol
//!
//! Provides AES-256-GCM encryption with PBKDF2 key derivation for
//! encrypting the admin private key during pairing.

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Key, Nonce,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use pbkdf2::pbkdf2_hmac;
use rand::RngCore;
use sha2::Sha256;
use std::fmt;

/// PBKDF2 iteration count (OWASP 2023 recommendation)
const PBKDF2_ITERATIONS: u32 = 480_000;

/// Salt length in bytes
const SALT_LENGTH: usize = 16;

/// Nonce length for AES-256-GCM
const NONCE_LENGTH: usize = 12;

/// Encryption error types
#[derive(Debug)]
pub enum CryptoError {
    EncryptionFailed,
    DecryptionFailed,
    InvalidSaltLength,
    InvalidNonceLength,
    Base64DecodeError(String),
}

impl fmt::Display for CryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CryptoError::EncryptionFailed => write!(f, "Encryption failed"),
            CryptoError::DecryptionFailed => write!(f, "Decryption failed"),
            CryptoError::InvalidSaltLength => write!(f, "Invalid salt length"),
            CryptoError::InvalidNonceLength => write!(f, "Invalid nonce length"),
            CryptoError::Base64DecodeError(e) => write!(f, "Base64 decode error: {}", e),
        }
    }
}

impl std::error::Error for CryptoError {}

/// Encrypted private key with associated data for decryption
#[derive(Debug, Clone)]
pub struct EncryptedPrivateKey {
    /// AES-256-GCM ciphertext + auth tag (48 bytes: 32 ciphertext + 16 tag)
    pub ciphertext: Vec<u8>,
    /// PBKDF2 salt (16 bytes)
    pub salt: [u8; SALT_LENGTH],
    /// AES-GCM nonce (12 bytes)
    pub nonce: [u8; NONCE_LENGTH],
}

impl EncryptedPrivateKey {
    /// Get ciphertext as base64
    pub fn ciphertext_base64(&self) -> String {
        BASE64.encode(&self.ciphertext)
    }

    /// Get salt as base64
    pub fn salt_base64(&self) -> String {
        BASE64.encode(self.salt)
    }

    /// Get nonce as base64
    pub fn nonce_base64(&self) -> String {
        BASE64.encode(self.nonce)
    }

    /// Create from base64-encoded components
    pub fn from_base64(
        ciphertext_b64: &str,
        salt_b64: &str,
        nonce_b64: &str,
    ) -> Result<Self, CryptoError> {
        let ciphertext = BASE64
            .decode(ciphertext_b64)
            .map_err(|e| CryptoError::Base64DecodeError(e.to_string()))?;

        let salt_vec = BASE64
            .decode(salt_b64)
            .map_err(|e| CryptoError::Base64DecodeError(e.to_string()))?;

        let nonce_vec = BASE64
            .decode(nonce_b64)
            .map_err(|e| CryptoError::Base64DecodeError(e.to_string()))?;

        if salt_vec.len() != SALT_LENGTH {
            return Err(CryptoError::InvalidSaltLength);
        }

        if nonce_vec.len() != NONCE_LENGTH {
            return Err(CryptoError::InvalidNonceLength);
        }

        let mut salt = [0u8; SALT_LENGTH];
        let mut nonce = [0u8; NONCE_LENGTH];
        salt.copy_from_slice(&salt_vec);
        nonce.copy_from_slice(&nonce_vec);

        Ok(Self {
            ciphertext,
            salt,
            nonce,
        })
    }
}

/// Derive AES-256 key from pairing code using PBKDF2-HMAC-SHA256
pub fn derive_key(pairing_code: &str, salt: &[u8]) -> [u8; 32] {
    let mut key = [0u8; 32];
    pbkdf2_hmac::<Sha256>(pairing_code.as_bytes(), salt, PBKDF2_ITERATIONS, &mut key);
    key
}

/// Encrypt an Ed25519 private key seed using AES-256-GCM
///
/// The pairing code is used as the password for PBKDF2 key derivation.
pub fn encrypt_private_key(
    private_key: &[u8; 32],
    pairing_code: &str,
) -> Result<EncryptedPrivateKey, CryptoError> {
    let mut rng = rand::thread_rng();

    // Generate random salt and nonce
    let mut salt = [0u8; SALT_LENGTH];
    let mut nonce_bytes = [0u8; NONCE_LENGTH];
    rng.fill_bytes(&mut salt);
    rng.fill_bytes(&mut nonce_bytes);

    // Derive AES key from pairing code
    let aes_key = derive_key(pairing_code, &salt);

    // Encrypt with AES-256-GCM
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&aes_key));
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, private_key.as_ref())
        .map_err(|_| CryptoError::EncryptionFailed)?;

    Ok(EncryptedPrivateKey {
        ciphertext,
        salt,
        nonce: nonce_bytes,
    })
}

/// Decrypt an Ed25519 private key seed using AES-256-GCM
///
/// Used for testing and verification.
pub fn decrypt_private_key(
    encrypted: &EncryptedPrivateKey,
    pairing_code: &str,
) -> Result<[u8; 32], CryptoError> {
    // Derive AES key from pairing code
    let aes_key = derive_key(pairing_code, &encrypted.salt);

    // Decrypt with AES-256-GCM
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&aes_key));
    let nonce = Nonce::from_slice(&encrypted.nonce);
    let plaintext = cipher
        .decrypt(nonce, encrypted.ciphertext.as_ref())
        .map_err(|_| CryptoError::DecryptionFailed)?;

    if plaintext.len() != 32 {
        return Err(CryptoError::DecryptionFailed);
    }

    let mut key = [0u8; 32];
    key.copy_from_slice(&plaintext);
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_derivation_deterministic() {
        let code = "ABC123";
        let salt = [0u8; 16];

        let key1 = derive_key(code, &salt);
        let key2 = derive_key(code, &salt);

        assert_eq!(key1, key2);
    }

    #[test]
    fn test_key_derivation_different_salts() {
        let code = "ABC123";
        let salt1 = [0u8; 16];
        let salt2 = [1u8; 16];

        let key1 = derive_key(code, &salt1);
        let key2 = derive_key(code, &salt2);

        assert_ne!(key1, key2);
    }

    #[test]
    fn test_key_derivation_different_codes() {
        let salt = [0u8; 16];

        let key1 = derive_key("ABC123", &salt);
        let key2 = derive_key("XYZ789", &salt);

        assert_ne!(key1, key2);
    }

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let private_key = [0xABu8; 32];
        let code = "XY7K9M";

        let encrypted = encrypt_private_key(&private_key, code).unwrap();
        let decrypted = decrypt_private_key(&encrypted, code).unwrap();

        assert_eq!(private_key, decrypted);
    }

    #[test]
    fn test_wrong_code_fails_decryption() {
        let private_key = [0xABu8; 32];

        let encrypted = encrypt_private_key(&private_key, "ABC123").unwrap();
        let result = decrypt_private_key(&encrypted, "WRONG1");

        assert!(result.is_err());
    }

    #[test]
    fn test_base64_roundtrip() {
        let private_key = [0xCDu8; 32];
        let code = "HJKMNP";

        let encrypted = encrypt_private_key(&private_key, code).unwrap();

        // Convert to base64 and back
        let reconstructed = EncryptedPrivateKey::from_base64(
            &encrypted.ciphertext_base64(),
            &encrypted.salt_base64(),
            &encrypted.nonce_base64(),
        )
        .unwrap();

        // Verify decryption still works
        let decrypted = decrypt_private_key(&reconstructed, code).unwrap();
        assert_eq!(private_key, decrypted);
    }

    #[test]
    fn test_ciphertext_length() {
        let private_key = [0u8; 32];
        let encrypted = encrypt_private_key(&private_key, "ABC234").unwrap();

        // 32 bytes plaintext + 16 bytes auth tag = 48 bytes
        assert_eq!(encrypted.ciphertext.len(), 48);
    }
}
