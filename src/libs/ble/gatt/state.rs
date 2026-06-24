//! Shared GATT-server state and hostname helper.

use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use bluer::gatt::local::CharacteristicNotifier;
use tokio::sync::Mutex;

use crate::libs::config_applier::ConfigApplier;
use crate::libs::network::SharedProvisioningSession;

use super::terminal::ShellProcess;

pub struct ServiceState {
    pub authenticated: AtomicBool,
    /// Live ephemeral provisioning session — `None` outside provisioning mode.
    /// Replaces the previous static `pin: String`; the BLE auth path now
    /// rejects any attempt when this is `None` or the inner session has
    /// expired. See [`crate::libs::network::ProvisioningSession`].
    pub provisioning_session: SharedProvisioningSession,
    pub hostname: String,
    pub mac_address: String,
    /// Optional handle to the config-applier so authenticated FB0A writes
    /// can mutate `system.device_label` atomically. `None` only in tests
    /// or when the applier failed to construct at boot.
    pub config_applier: Option<Arc<ConfigApplier>>,
    pub terminal_notifier: Option<Arc<Mutex<CharacteristicNotifier>>>,
    pub shell_process: Option<Arc<Mutex<ShellProcess>>>,
}

impl ServiceState {
    pub fn new(
        provisioning_session: SharedProvisioningSession,
        hostname: String,
        mac_address: String,
        config_applier: Option<Arc<ConfigApplier>>,
    ) -> Self {
        Self {
            authenticated: AtomicBool::new(false),
            provisioning_session,
            hostname,
            mac_address,
            config_applier,
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
