//! Device Certificate Authority key management
//!
//! Manages the device's Ed25519 CA keypair for signing admin certificates.
//! The key is generated on first boot and persisted to the filesystem.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// CA key filename
const CA_KEY_FILENAME: &str = "device_ca.key";

/// Error types for CA key operations
#[derive(Debug)]
pub enum CaKeyError {
    IoError(io::Error),
    InvalidKeyFormat(String),
    HexDecodeError(String),
    SigningError(String),
}

impl std::fmt::Display for CaKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaKeyError::IoError(e) => write!(f, "IO error: {}", e),
            CaKeyError::InvalidKeyFormat(s) => write!(f, "Invalid key format: {}", s),
            CaKeyError::HexDecodeError(s) => write!(f, "Hex decode error: {}", s),
            CaKeyError::SigningError(s) => write!(f, "Signing error: {}", s),
        }
    }
}

impl std::error::Error for CaKeyError {}

impl From<io::Error> for CaKeyError {
    fn from(e: io::Error) -> Self {
        CaKeyError::IoError(e)
    }
}

/// Device Certificate Authority key manager
pub struct DeviceCaKey {
    /// Ed25519 signing key
    signing_key: SigningKey,
    /// Path to key file
    key_path: PathBuf,
    /// Hostname for CA ID
    hostname: String,
}

impl DeviceCaKey {
    /// Load existing CA key or generate a new one
    ///
    /// The key is stored at `{config_dir}/device_ca.key` as hex-encoded 32-byte seed.
    pub fn load_or_generate(config_dir: &Path, hostname: &str) -> Result<Self, CaKeyError> {
        let key_path = config_dir.join(CA_KEY_FILENAME);

        let signing_key = if key_path.exists() {
            Self::load_key(&key_path)?
        } else {
            let key = Self::generate_key();
            Self::save_key(&key_path, &key)?;
            eprintln!(
                "[DeviceCaKey] Generated new CA key at {}",
                key_path.display()
            );
            key
        };

        Ok(Self {
            signing_key,
            key_path,
            hostname: hostname.to_string(),
        })
    }

    /// Generate a new Ed25519 signing key
    fn generate_key() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    /// Load key from file (hex-encoded 32-byte seed)
    fn load_key(path: &Path) -> Result<SigningKey, CaKeyError> {
        let hex_content = fs::read_to_string(path)?;
        let hex_trimmed = hex_content.trim();

        let seed_bytes = hex::decode(hex_trimmed)
            .map_err(|e| CaKeyError::HexDecodeError(e.to_string()))?;

        if seed_bytes.len() != 32 {
            return Err(CaKeyError::InvalidKeyFormat(format!(
                "Expected 32 bytes, got {}",
                seed_bytes.len()
            )));
        }

        let mut seed = [0u8; 32];
        seed.copy_from_slice(&seed_bytes);

        Ok(SigningKey::from_bytes(&seed))
    }

    /// Save key to file (hex-encoded 32-byte seed)
    fn save_key(path: &Path, key: &SigningKey) -> Result<(), CaKeyError> {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let hex_seed = hex::encode(key.to_bytes());
        fs::write(path, hex_seed)?;

        // Set restrictive permissions (owner read/write only)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(path)?.permissions();
            perms.set_mode(0o600);
            fs::set_permissions(path, perms)?;
        }

        Ok(())
    }

    /// Get the CA's public key as hex-encoded string
    pub fn public_key_hex(&self) -> String {
        hex::encode(self.signing_key.verifying_key().to_bytes())
    }

    /// Get the CA's public key bytes
    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }

    /// Get the CA's private key bytes (for pairing export)
    pub fn private_key_bytes(&self) -> [u8; 32] {
        self.signing_key.to_bytes()
    }

    /// Get the CA's verifying key
    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }

    /// Get the unique CA identifier
    ///
    /// Format: `fiber-{hostname}-ca@fiber.local`
    pub fn ca_id(&self) -> String {
        format!("fiber-{}-ca@fiber.local", self.hostname.to_lowercase())
    }

    /// Sign a message with the CA's private key
    pub fn sign(&self, message: &[u8]) -> Signature {
        self.signing_key.sign(message)
    }

    /// Get path to the key file
    pub fn key_path(&self) -> &Path {
        &self.key_path
    }

    /// Generate a new admin keypair and return (signing_key, public_key_bytes)
    pub fn generate_admin_keypair() -> (SigningKey, [u8; 32]) {
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = signing_key.verifying_key().to_bytes();
        (signing_key, public_key)
    }

    /// Load existing CA key from a specific file path
    ///
    /// This is used to load the CA key for authorization without needing to generate.
    /// The hostname is read from /etc/hostname.
    pub fn load_existing(key_path: &Path) -> Result<Self, CaKeyError> {
        let signing_key = Self::load_key(key_path)?;

        // Read hostname from /etc/hostname
        let hostname = fs::read_to_string("/etc/hostname")
            .map(|h| h.trim().to_uppercase())
            .unwrap_or_else(|_| "UNKNOWN".to_string());

        Ok(Self {
            signing_key,
            key_path: key_path.to_path_buf(),
            hostname,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_generate_and_load() {
        let temp_dir = TempDir::new().unwrap();
        let config_dir = temp_dir.path();

        // Generate new key
        let ca1 = DeviceCaKey::load_or_generate(config_dir, "test-device").unwrap();
        let pubkey1 = ca1.public_key_hex();

        // Load existing key
        let ca2 = DeviceCaKey::load_or_generate(config_dir, "test-device").unwrap();
        let pubkey2 = ca2.public_key_hex();

        // Should be the same key
        assert_eq!(pubkey1, pubkey2);
    }

    #[test]
    fn test_public_key_format() {
        let temp_dir = TempDir::new().unwrap();
        let ca = DeviceCaKey::load_or_generate(temp_dir.path(), "test").unwrap();

        let pubkey_hex = ca.public_key_hex();

        // Ed25519 public key is 32 bytes = 64 hex chars
        assert_eq!(pubkey_hex.len(), 64);

        // Should be valid hex
        assert!(hex::decode(&pubkey_hex).is_ok());
    }

    #[test]
    fn test_ca_id_format() {
        let temp_dir = TempDir::new().unwrap();
        let ca = DeviceCaKey::load_or_generate(temp_dir.path(), "FIBER-001").unwrap();

        let ca_id = ca.ca_id();

        assert_eq!(ca_id, "fiber-fiber-001-ca@fiber.local");
    }

    #[test]
    fn test_sign_and_verify() {
        let temp_dir = TempDir::new().unwrap();
        let ca = DeviceCaKey::load_or_generate(temp_dir.path(), "test").unwrap();

        let message = b"test message";
        let signature = ca.sign(message);

        // Verify with public key
        use ed25519_dalek::Verifier;
        assert!(ca.verifying_key().verify(message, &signature).is_ok());
    }

    #[test]
    fn test_generate_admin_keypair() {
        let (signing_key, public_key) = DeviceCaKey::generate_admin_keypair();

        // Verify public key matches signing key
        assert_eq!(signing_key.verifying_key().to_bytes(), public_key);

        // Keys should be 32 bytes
        assert_eq!(public_key.len(), 32);
    }
}
