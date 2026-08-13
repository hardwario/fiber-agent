//! Shared GATT-server state and hostname helper.

use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use bluer::gatt::local::CharacteristicNotifier;
use bluer::Address;
use tokio::sync::Mutex;

use crate::libs::config_applier::ConfigApplier;
use crate::libs::network::SharedProvisioningSession;

use super::eye_tag_add::SharedResult as EyeTagResultSlot;
use super::sticker::SharedResult as StickerResultSlot;
use super::terminal::ShellProcess;

pub struct ServiceState {
    /// The peer currently authenticated over FB01, if any. `None` means no one
    /// is authenticated. Scoped to a single address (rather than a bare bool)
    /// so a second peer's traffic can never be served under a different
    /// peer's session — see [`ServiceState::is_authenticated_for`].
    pub authenticated_peer: Option<Address>,
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
    /// Result of the most recent FB0E EYE-tag-add write, scoped to this
    /// GATT-server instance. Cleared on BLE disconnect. Enrollment is a
    /// synchronous local YAML write, so (unlike FB0D) there is no task handle.
    pub eye_tag_result: EyeTagResultSlot,
    /// True while an FB09 `apply_lan_config` task is running. A concurrent
    /// FB09 write is rejected so a spammy peer cannot saturate the
    /// blocking-thread pool or trigger a NetworkManager modify+up race
    /// that leaves the eth profile half-applied.
    pub lan_apply_in_flight: Arc<AtomicBool>,
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
            authenticated_peer: None,
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
            eye_tag_result: super::eye_tag_add::new_slot(),
            lan_apply_in_flight: Arc::new(AtomicBool::new(false)),
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

    /// Whether `addr` is the peer that authenticated over FB01. Every
    /// auth-gated characteristic must check this against its own request's
    /// `device_address` rather than a bare global flag, so one peer's session
    /// can never be used to serve another peer's request.
    pub fn is_authenticated_for(&self, addr: Address) -> bool {
        self.authenticated_peer == Some(addr)
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

#[cfg(test)]
mod auth_scoping_tests {
    use super::*;
    use crate::libs::network::new_shared_provisioning_session;

    const PEER_A: Address = Address::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x01]);
    const PEER_B: Address = Address::new([0x7C, 0xD9, 0xF4, 0x13, 0x10, 0xDE]);

    fn new_state() -> ServiceState {
        ServiceState::new(
            new_shared_provisioning_session(),
            "FIBER-TEST".to_string(),
            "00:00:00:00:00:00".to_string(),
            None,
            None,
            None,
            Arc::new(std::sync::Mutex::new(None)),
        )
    }

    #[test]
    fn a_fresh_state_is_not_authenticated_for_anyone() {
        let state = new_state();
        assert!(!state.is_authenticated_for(PEER_A));
        assert!(!state.is_authenticated_for(PEER_B));
    }

    #[test]
    fn a_second_peer_cannot_ride_the_first_peers_session() {
        let mut state = new_state();
        state.authenticated_peer = Some(PEER_A);
        assert!(state.is_authenticated_for(PEER_A));
        assert!(!state.is_authenticated_for(PEER_B));
    }

    #[test]
    fn clearing_the_peer_de_authenticates_everyone() {
        let mut state = new_state();
        state.authenticated_peer = Some(PEER_A);
        state.authenticated_peer = None;
        assert!(!state.is_authenticated_for(PEER_A));
    }
}
