//! Shared GATT-server state and hostname helper.

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

/// Read /etc/hostname (uppercase). Falls back to "FIBER-DEVICE".
pub fn get_hostname() -> String {
    Command::new("hostname")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_uppercase())
        .unwrap_or_else(|_| "FIBER-DEVICE".to_string())
}
