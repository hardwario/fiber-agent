//! Shared GATT-server state and hostname helper.

use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use bluer::gatt::local::CharacteristicNotifier;
use tokio::sync::Mutex;

use crate::libs::config_applier::ConfigApplier;
use crate::libs::network::SharedProvisioningSession;

use super::sticker::SharedResult as StickerResultSlot;
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
    /// Handles for the FB0D sticker-add path (mirror the MQTT add). `storage`
    /// and `lorawan_configs` exist before the BLE monitor starts; the LoRaWAN
    /// shared state is created later, so it is delivered through a slot the
    /// main thread fills once the LoRaWAN monitor is up (same as MQTT's
    /// `set_lorawan_state`).
    pub storage: Option<crate::libs::storage::StorageHandle>,
    pub lorawan_configs: Option<crate::libs::lorawan::SharedLoRaWANSensorConfigs>,
    pub lorawan_state_slot:
        std::sync::Arc<std::sync::Mutex<Option<crate::libs::lorawan::SharedLoRaWANState>>>,
    pub terminal_notifier: Option<Arc<Mutex<CharacteristicNotifier>>>,
    pub shell_process: Option<Arc<Mutex<ShellProcess>>>,
    /// Result of the most recent FB0D enrollment, scoped to this GATT-server
    /// instance. Cleared on BLE disconnect so one client cannot read another
    /// client's pending or completed result.
    pub sticker_result: StickerResultSlot,
    /// Handle to the background enrollment task (if any). Aborted on
    /// disconnect so a slow add cannot keep running and overwrite the slot
    /// after the originating peer is gone.
    pub sticker_task: Option<tokio::task::JoinHandle<()>>,
}

impl ServiceState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provisioning_session: SharedProvisioningSession,
        hostname: String,
        mac_address: String,
        config_applier: Option<Arc<ConfigApplier>>,
        storage: Option<crate::libs::storage::StorageHandle>,
        lorawan_configs: Option<crate::libs::lorawan::SharedLoRaWANSensorConfigs>,
        lorawan_state_slot: std::sync::Arc<
            std::sync::Mutex<Option<crate::libs::lorawan::SharedLoRaWANState>>,
        >,
    ) -> Self {
        Self {
            authenticated: AtomicBool::new(false),
            provisioning_session,
            hostname,
            mac_address,
            config_applier,
            storage,
            lorawan_configs,
            lorawan_state_slot,
            terminal_notifier: None,
            shell_process: None,
            sticker_result: super::sticker::new_slot(),
            sticker_task: None,
        }
    }

    /// Bundle the handles the FB0D sticker-add path needs. The LoRaWAN shared
    /// state is read from its slot at call time (it may still be empty early in
    /// boot before the LoRaWAN monitor fills it).
    pub fn sticker_deps(&self) -> crate::libs::lorawan::StickerAddDeps {
        crate::libs::lorawan::StickerAddDeps {
            config_applier: self.config_applier.clone(),
            storage: self.storage.clone(),
            lorawan_configs: self.lorawan_configs.clone(),
            lorawan_state: self.lorawan_state_slot.lock().ok().and_then(|g| g.clone()),
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
