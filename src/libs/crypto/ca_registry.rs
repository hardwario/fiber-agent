//! Certificate Authority registry for CA-based trust model
//!
//! The device trusts one or more Certificate Authorities (CAs).
//! User certificates signed by a trusted CA are accepted.

use super::error::CryptoError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// A trusted Certificate Authority
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CertificateAuthority {
    /// Unique CA identifier
    pub ca_id: String,

    /// CA's Ed25519 public key (hex-encoded, 64 hex characters = 32 bytes)
    pub ca_public_key_ed25519: String,

    /// When this CA was added to the trust store (RFC3339)
    pub trusted_since: String,

    /// Whether this CA is currently enabled
    pub enabled: bool,

    /// Optional description
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl CertificateAuthority {
    /// Get the CA's public key as bytes
    pub fn get_public_key_bytes(&self) -> Result<[u8; 32], CryptoError> {
        let key_bytes = hex::decode(&self.ca_public_key_ed25519).map_err(|e| {
            CryptoError::InvalidPublicKey(format!("Failed to decode CA public key hex: {}", e))
        })?;

        if key_bytes.len() != 32 {
            return Err(CryptoError::InvalidPublicKey(format!(
                "CA public key must be 32 bytes, got {}",
                key_bytes.len()
            )));
        }

        key_bytes
            .as_slice()
            .try_into()
            .map_err(|_| CryptoError::InvalidPublicKey("Failed to convert to array".into()))
    }
}

/// Registry file format (authorized_signers.yaml)
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CARegistryFile {
    /// File format version (should be 2 for CA model)
    version: u32,

    /// List of trusted Certificate Authorities
    certificate_authorities: Vec<CertificateAuthority>,
}

/// CA Registry with hot-reload support
pub struct CARegistry {
    /// Map of ca_id -> CertificateAuthority
    authorities: HashMap<String, CertificateAuthority>,

    /// Path to the registry file
    config_path: PathBuf,

    /// Last modification time of the config file
    last_modified: Option<std::time::SystemTime>,
}

impl CARegistry {
    /// Load CA registry from file
    pub fn load_from_file(path: &Path) -> Result<Self, CryptoError> {
        let config_path = path.to_path_buf();

        // Check if file exists
        if !config_path.exists() {
            eprintln!(
                "[CARegistry] Warning: Config file does not exist: {}",
                config_path.display()
            );
            // Return empty registry
            return Ok(Self {
                authorities: HashMap::new(),
                config_path,
                last_modified: None,
            });
        }

        let last_modified = fs::metadata(&config_path)
            .ok()
            .and_then(|m| m.modified().ok());

        let contents = fs::read_to_string(&config_path).map_err(|e| {
            CryptoError::RegistryLoadError(format!(
                "Failed to read {}: {}",
                config_path.display(),
                e
            ))
        })?;

        let registry_file: CARegistryFile = serde_yaml::from_str(&contents).map_err(|e| {
            CryptoError::RegistryLoadError(format!(
                "Failed to parse YAML from {}: {}",
                config_path.display(),
                e
            ))
        })?;

        // Verify version
        if registry_file.version != 2 {
            return Err(CryptoError::RegistryLoadError(format!(
                "Unsupported registry version: {} (expected 2 for CA model)",
                registry_file.version
            )));
        }

        // Build hashmap
        let mut authorities = HashMap::new();
        for ca in registry_file.certificate_authorities {
            authorities.insert(ca.ca_id.clone(), ca);
        }

        eprintln!(
            "[CARegistry] Loaded {} trusted CAs from {}",
            authorities.len(),
            config_path.display()
        );

        Ok(Self {
            authorities,
            config_path,
            last_modified,
        })
    }

    /// Check if registry file has been modified and reload if needed
    pub fn reload_if_modified(&mut self) -> Result<bool, CryptoError> {
        let current_modified = fs::metadata(&self.config_path)
            .ok()
            .and_then(|m| m.modified().ok());

        if current_modified != self.last_modified {
            eprintln!(
                "[CARegistry] Config file modified, reloading: {}",
                self.config_path.display()
            );

            let new_registry = Self::load_from_file(&self.config_path)?;
            self.authorities = new_registry.authorities;
            self.last_modified = current_modified;

            return Ok(true);
        }

        Ok(false)
    }

    /// Get CA by ID
    pub fn get_ca(&self, ca_id: &str) -> Option<&CertificateAuthority> {
        self.authorities.get(ca_id)
    }

    /// Get CA by ID if it's enabled
    pub fn get_enabled_ca(&self, ca_id: &str) -> Option<&CertificateAuthority> {
        self.authorities.get(ca_id).filter(|ca| ca.enabled)
    }

    /// Get CA public key by ID (if CA is enabled)
    pub fn get_ca_public_key(&self, ca_id: &str) -> Option<&str> {
        self.get_enabled_ca(ca_id)
            .map(|ca| ca.ca_public_key_ed25519.as_str())
    }

    /// Find CA by public key (for when issuer is not specified)
    pub fn find_ca_by_public_key(&self, public_key_hex: &str) -> Option<&CertificateAuthority> {
        self.authorities
            .values()
            .find(|ca| ca.enabled && ca.ca_public_key_ed25519 == public_key_hex)
    }

    /// Get all enabled CAs (for trying all when verifying)
    pub fn get_all_enabled_cas(&self) -> Vec<&CertificateAuthority> {
        self.authorities.values().filter(|ca| ca.enabled).collect()
    }

    /// Get number of registered CAs
    pub fn len(&self) -> usize {
        self.authorities.len()
    }

    /// Check if registry is empty
    pub fn is_empty(&self) -> bool {
        self.authorities.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn create_test_registry_file() -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"version: 2
certificate_authorities:
  - ca_id: "test-ca@fiber.com"
    ca_public_key_ed25519: "{}"
    trusted_since: "2024-01-01T00:00:00Z"
    enabled: true
    description: "Test CA"
"#,
            "a".repeat(64)
        )
        .unwrap();
        file
    }

    #[test]
    fn test_load_registry() {
        let file = create_test_registry_file();
        let registry = CARegistry::load_from_file(file.path()).unwrap();

        assert_eq!(registry.len(), 1);
        assert!(registry.get_ca("test-ca@fiber.com").is_some());
    }

    #[test]
    fn test_enabled_ca() {
        let file = create_test_registry_file();
        let registry = CARegistry::load_from_file(file.path()).unwrap();

        assert!(registry.get_enabled_ca("test-ca@fiber.com").is_some());
    }

    #[test]
    fn test_missing_ca() {
        let file = create_test_registry_file();
        let registry = CARegistry::load_from_file(file.path()).unwrap();

        assert!(registry.get_ca("nonexistent@fiber.com").is_none());
    }
}
