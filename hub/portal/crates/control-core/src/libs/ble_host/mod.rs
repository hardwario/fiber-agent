//! The agent's implementations of `control_ble::host::*`.
//!
//! `control-ble` owns the Hub's Bluetooth: the scan loop, the BTSMP1 decode, the EN12830 archive,
//! the FB00 GATT server. It owns none of the Hub's *state* — no database, no YAML, no MQTT client,
//! no `nmcli` — because a crate that reached for those could not be tested without them and could
//! not be reused by the portal later. Everything it needs from the surrounding process arrives
//! through nine traits, and this module is the agent's answer to all nine.
//!
//! | Trait | Here | Over |
//! |---|---|---|
//! | `ConfigStore` | [`config_store::ApplierConfigStore`] | `ConfigApplier` — backup, audit, live reload |
//! | `Publisher` | [`publisher::MqttPublisher`] | the existing `MqttMessage` channel |
//! | `ReadingSink` | [`reading_sink::StorageReadingSink`] | the storage thread (decision #13, variant A) |
//! | `SubsystemControl` | [`subsystem::DeferredSubsystem`] | the crate's own `CentralHandle` |
//! | `StandbyGate` | [`subsystem::PowerStandby`] | `libs::power::standby` |
//! | `NetworkOps` | [`network::NmcliNetwork`] | `libs::network::{wifi,lan}` |
//! | `SystemClock` | [`clock::SystemClockImpl`] | `libs::network::time_sync` |
//! | `DeviceInfo` | [`device_info::AgentDeviceInfo`] | hostname, build version, `/proc/uptime` |
//! | `NodeEnroll` | [`node_enroll::LorawanNodeEnroll`] | the Node registry and `libs::radio_nodes` on a radio-mode Hub (#118), `libs::lorawan`'s node add/remove paths otherwise |
//! | `RadioModeControl` | [`radio_mode::ApplierRadioMode`] | `libs::radio_mode`, the signed `radio-set-mode`'s apply path |
//! | `ClusterJoin` | [`cluster_join::ApplierClusterJoin`] | `libs::cluster_join`, `proximosctl control.radio create --role client`'s apply path |
//!
//! **What is deliberately absent.** No implementation here reimplements a v1 behaviour: every one
//! delegates to the subsystem that already owned it, which is what makes a BLE-originated change
//! and an MQTT-originated change indistinguishable at the store layer (AC 4). Where a delegate did
//! not exist in a usable shape, the imported file gained a method rather than this module gaining a
//! second implementation — `ConfigApplier::apply_beacon_tag_config_and_enable` and
//! `StorageHandle::max_beacon_reading_ts` are the two, both listed in the README's edit ledger.

pub mod clock;
pub mod cluster_join;
pub mod config_store;
pub mod device_info;
pub mod display;
pub mod export_destination;
pub mod network;
pub mod node_enroll;
pub mod portal_pairing;
pub mod publisher;
pub mod radio_mode;
pub mod reading_sink;
pub mod registry;
pub mod setup_state;
pub mod subsystem;

use std::path::Path;
use std::sync::Arc;

use control_ble::host::Host;

use crate::libs::config_applier::ConfigApplier;
use crate::libs::lorawan::NodeAddDeps;
use crate::libs::mqtt::messages::MqttMessage;
use crate::libs::storage::StorageHandle;

pub use registry::{
    begin_phone_session, central, end_phone_session, host, install, peripheral, seams, BleSeams,
};
pub use subsystem::DeferredSubsystem;

/// Whether `mac` is a Beacon address this Hub will accept.
///
/// Delegates to the crate's own parser so v2 has exactly one MAC grammar. v1 had two — a regex in
/// `libs/beacon/state.rs` and a separate check in the GATT handler — and they disagreed about
/// lowercase input, which is the sort of difference that shows up as "the phone can add a Beacon
/// the portal cannot".
pub fn is_valid_mac(mac: &str) -> bool {
    control_ble::mac::MacAddr::parse(mac).is_ok()
}

/// Everything `main.rs` needs to hand the crate, assembled in one place.
///
/// The subsystem is returned alongside the `Host` because it is the one seam that cannot be
/// satisfied yet: it points at a runtime that `tasks::spawn` has not created. Call
/// [`DeferredSubsystem::fill`] with the handle the spawn returns, immediately, on the same thread.
pub struct BleHost {
    pub host: Host,
    pub subsystem: Arc<DeferredSubsystem>,
}

/// Build the bundle from the agent's live handles.
#[allow(clippy::too_many_arguments)]
pub fn build(
    applier: Arc<ConfigApplier>,
    config_dir: &Path,
    mqtt_tx: crossbeam::channel::Sender<MqttMessage>,
    storage: StorageHandle,
    hostname: String,
    mac: String,
    node_deps: NodeAddDeps,
) -> BleHost {
    let subsystem = DeferredSubsystem::new();
    let applier_for_setup = applier.clone();
    let applier_for_export = applier.clone();
    let applier_for_radio = applier.clone();
    let applier_for_cluster = applier.clone();
    let mqtt_tx_for_pairing = mqtt_tx.clone();
    let host = Host {
        // Read through the cluster view (decision #42): a Client's registry and fleet keys are
        // the Server's, and every Hub dials only the Beacons it is home for.
        config: Arc::new(control_ble::central::cluster::ClusterConfigStore::new(
            Arc::new(config_store::ApplierConfigStore::new(
                applier.clone(),
                config_dir,
            )),
            crate::libs::beacon_cluster::membership().clone(),
        )),
        publisher: Arc::new(publisher::MqttPublisher::new(mqtt_tx)),
        readings: Arc::new(reading_sink::StorageReadingSink::new(storage)),
        subsystem: subsystem.clone(),
        standby: Arc::new(subsystem::PowerStandby),
        network: Arc::new(network::NmcliNetwork),
        clock: Arc::new(clock::SystemClockImpl),
        device: Arc::new(device_info::AgentDeviceInfo::new(
            hostname, mac, applier, config_dir,
        )),
        nodes: Arc::new(node_enroll::LorawanNodeEnroll::new(
            node_deps,
            config_dir.join("fiber.config.yaml"),
        )),
        setup: Arc::new(setup_state::ApplierSetupState::new(
            applier_for_setup,
            config_dir,
        )),
        export: Arc::new(export_destination::ApplierExportDestination::new(
            applier_for_export,
        )),
        pairing: Arc::new(portal_pairing::MqttPortalPairing::new(mqtt_tx_for_pairing)),
        radio: Arc::new(radio_mode::ApplierRadioMode::new(applier_for_radio)),
        cluster: Arc::new(cluster_join::ApplierClusterJoin::new(applier_for_cluster)),
    };
    BleHost { host, subsystem }
}

#[cfg(test)]
mod tests {
    use super::*;
    use control_ble::host::{ConfigStore, DeviceInfo, NodeEnroll, StandbyGate, SubsystemControl};

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let base = std::env::temp_dir().join(format!(
                "ble-host-mod-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&base).expect("temp dir");
            std::fs::write(
                base.join("fiber.config.yaml"),
                "system:\n  device_label: bench\neye:\n  enabled: true\n  tags: []\n",
            )
            .expect("seed");
            TempDir(base)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn built(dir: &Path) -> BleHost {
        let applier = Arc::new(ConfigApplier::new(dir).expect("applier"));
        let (tx, rx) = crossbeam::channel::unbounded();
        std::mem::forget(rx); // keep the channel open for the lifetime of the test
        let thread = crate::libs::storage::StorageThread::spawn(
            dir.join("fiber.db").to_str().expect("path"),
            1,
        )
        .expect("storage");
        build(
            applier,
            dir,
            tx,
            thread.0,
            "test-hub".to_string(),
            "B8:27:EB:11:22:33".to_string(),
            NodeAddDeps::default(),
        )
    }

    #[test]
    fn the_bundle_wires_every_seam_to_something_real() {
        let dir = TempDir::new("bundle");
        let b = built(&dir.0);
        // Each of these would panic or return a placeholder if a seam were left unimplemented.
        assert_eq!(b.host.device.hostname(), "test-hub");
        assert_eq!(b.host.device.label(), "bench");
        assert!(!b.host.device.version().is_empty());
        assert!(!b.host.device.uptime().is_empty());
        assert!(b.host.config.load().expect("eye section").enabled);
        assert!(b.host.clock.now_unix() > 1_700_000_000);
        assert!(!b.host.standby.in_standby());
        // FB11 answers on this bundle: the seeded config has no `lorawan:` section at all, which is
        // an empty list rather than a failure — a Beacon-only site holds no Nodes.
        assert!(b
            .host
            .nodes
            .list()
            .expect("the node list is answerable")
            .is_empty());
    }

    #[test]
    fn the_subsystem_starts_unfilled_and_reports_it_honestly() {
        // Between `build` and `tasks::spawn` the central handle does not exist. That window must
        // report "not scanning" rather than claim a loop that is not there.
        let dir = TempDir::new("deferred");
        let b = built(&dir.0);
        assert!(!b.subsystem.is_scanning());
        assert!(b.host.subsystem.ensure_scanning().is_err());
        assert!(b.subsystem.handle().is_none());
    }
}
