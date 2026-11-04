//! Device Certificate Authority key management
//!
//! Manages the device's Ed25519 CA keypair for signing admin certificates.
//! The key is generated on first boot and persisted to the filesystem.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use std::fs;
use std::io::{self, Write};
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

        // An empty file is a key that never reached the disk: `save_key` used to write
        // it in place, and a power cut seconds after first boot left a 0-byte file that
        // failed every later boot and took pairing down with it. Nothing can have been
        // signed with a key nobody can read, so it is replaced like a missing one. A file
        // with content that does not parse still fails: replacing that one would silently
        // invalidate every certificate issued under it.
        let empty = fs::metadata(&key_path)
            .map(|m| m.len() == 0)
            .unwrap_or(false);
        if empty {
            eprintln!(
                "[DeviceCaKey] Warning: {} is empty - generating a new CA key",
                key_path.display()
            );
        }

        let signing_key = if key_path.exists() && !empty {
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

        let seed_bytes =
            hex::decode(hex_trimmed).map_err(|e| CaKeyError::HexDecodeError(e.to_string()))?;

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
    ///
    /// Temp file in the same directory, created 0600, synced, renamed over the key, then
    /// the directory synced: a power cut leaves the old file or the whole new key, never
    /// an empty one.
    fn save_key(path: &Path, key: &SigningKey) -> Result<(), CaKeyError> {
        let dir = path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(dir)?;

        let tmp = dir.join(format!(".{}.{}.tmp", CA_KEY_FILENAME, std::process::id()));
        let _ = fs::remove_file(&tmp);
        let result = (|| {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut f = options.open(&tmp)?;
            f.write_all(hex::encode(key.to_bytes()).as_bytes())?;
            f.sync_all()?;
            drop(f);
            fs::rename(&tmp, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result?;
        fs::File::open(dir)?.sync_all()?;

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
            .map(|h| h.trim().to_lowercase())
            .unwrap_or_else(|_| "unknown".to_string());

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
    fn an_empty_key_file_is_replaced_with_a_new_key() {
        // What a power cut right after first boot left behind: the file, no key.
        let temp_dir = TempDir::new().unwrap();
        let key_path = temp_dir.path().join(CA_KEY_FILENAME);
        fs::write(&key_path, "").unwrap();

        let ca = DeviceCaKey::load_or_generate(temp_dir.path(), "test-device").unwrap();

        let loaded = DeviceCaKey::load_existing(&key_path).unwrap();
        assert_eq!(ca.public_key_hex(), loaded.public_key_hex());
    }

    #[test]
    fn a_corrupt_key_file_is_not_replaced() {
        let temp_dir = TempDir::new().unwrap();
        let key_path = temp_dir.path().join(CA_KEY_FILENAME);
        fs::write(&key_path, "abcd").unwrap();

        assert!(DeviceCaKey::load_or_generate(temp_dir.path(), "test-device").is_err());
        assert_eq!(fs::read_to_string(&key_path).unwrap(), "abcd");
    }

    #[test]
    fn saving_leaves_only_the_key_with_owner_only_access() {
        let temp_dir = TempDir::new().unwrap();
        DeviceCaKey::load_or_generate(temp_dir.path(), "test-device").unwrap();

        let names: Vec<_> = fs::read_dir(temp_dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec![CA_KEY_FILENAME.to_string()]);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(temp_dir.path().join(CA_KEY_FILENAME))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn test_load_existing_matches_load_or_generate() {
        // The device CA is written by whichever consumer runs first
        // (load_or_generate) and read back by the other (load_existing). The MQTT
        // command verifier trusts the key it reads with load_existing, while
        // pairing signs the operator certificate with the key it opened via
        // load_or_generate. If those two ever saw different keys, every signed
        // command would be rejected as "not signed by any trusted CA" - so the
        // public key MUST be identical across the two entry points for the same
        // file.
        let temp_dir = TempDir::new().unwrap();
        let config_dir = temp_dir.path();

        let generated = DeviceCaKey::load_or_generate(config_dir, "fiber-race").unwrap();
        let loaded = DeviceCaKey::load_existing(&config_dir.join(CA_KEY_FILENAME)).unwrap();

        assert_eq!(generated.public_key_hex(), loaded.public_key_hex());
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
