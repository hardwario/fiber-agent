//! Shared GATT-server state and PIN/hostname helpers.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use bluer::gatt::local::CharacteristicNotifier;
use tokio::sync::Mutex;

use super::terminal::ShellProcess;

pub struct ServiceState {
    pub authenticated: AtomicBool,
    pub pin: String,
    pub hostname: String,
    pub mac_address: String,
    pub terminal_notifier: Option<Arc<Mutex<CharacteristicNotifier>>>,
    pub shell_process: Option<Arc<Mutex<ShellProcess>>>,
}

impl ServiceState {
    pub fn new(pin: String, hostname: String, mac_address: String) -> Self {
        Self {
            authenticated: AtomicBool::new(false),
            pin,
            hostname,
            mac_address,
            terminal_notifier: None,
            shell_process: None,
        }
    }
}

pub type SharedState = Arc<Mutex<ServiceState>>;

/// Load PIN from `pin_file`. If absent, write `default_pin` and return it.
/// File is created with mode 0600 to limit exposure.
pub fn load_pin(pin_file: &str, default_pin: &str) -> String {
    if Path::new(pin_file).exists() {
        if let Ok(content) = fs::read_to_string(pin_file) {
            let pin = content.trim().to_string();
            if !pin.is_empty() {
                eprintln!("[ble::state] PIN loaded from {}", pin_file);
                return pin;
            }
        }
    }

    eprintln!("[ble::state] Using default PIN: {}", default_pin);
    if let Some(parent) = Path::new(pin_file).parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Err(e) = fs::write(pin_file, default_pin) {
        eprintln!("[ble::state] Warning: failed to persist PIN file: {}", e);
    } else {
        // Tighten permissions on Unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(metadata) = fs::metadata(pin_file) {
                let mut perms = metadata.permissions();
                perms.set_mode(0o600);
                let _ = fs::set_permissions(pin_file, perms);
            }
        }
    }
    default_pin.to_string()
}

/// Write the MAC address to a file for any external consumer (e.g. the
/// QR generator was reading /data/ble/mac.txt during the Yocto era).
/// Phase 1: still write it for compatibility. Phase 3: file becomes optional.
pub fn write_mac_to_file(path: &str, mac: &str) {
    if let Some(parent) = Path::new(path).parent() {
        let _ = fs::create_dir_all(parent);
    }
    match fs::write(path, mac) {
        Ok(_) => eprintln!("[ble::state] MAC address written to {}", path),
        Err(e) => eprintln!("[ble::state] Warning: could not write MAC: {}", e),
    }
}

/// Read /etc/hostname (uppercase). Falls back to "FIBER-DEVICE".
pub fn get_hostname() -> String {
    Command::new("hostname")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_uppercase())
        .unwrap_or_else(|_| "FIBER-DEVICE".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    #[test]
    fn load_pin_creates_file_with_default_when_missing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("pin.txt");
        let pin = load_pin(path.to_str().unwrap(), "654321");
        assert_eq!(pin, "654321");
        assert_eq!(fs::read_to_string(&path).unwrap(), "654321");
    }

    #[test]
    fn load_pin_returns_existing_value() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("pin.txt");
        fs::write(&path, "999999\n").unwrap();
        let pin = load_pin(path.to_str().unwrap(), "111111");
        assert_eq!(pin, "999999");
    }

    #[test]
    fn load_pin_falls_back_when_file_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("pin.txt");
        fs::write(&path, "   \n").unwrap();
        let pin = load_pin(path.to_str().unwrap(), "111111");
        assert_eq!(pin, "111111");
    }

    #[test]
    fn load_pin_sets_0600_permissions_when_creating() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("pin.txt");
        let _ = load_pin(path.to_str().unwrap(), "777777");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "newly-created PIN file must be 0600");
    }
}
