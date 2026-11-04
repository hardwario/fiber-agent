//! Simulated FB16/FB17 — the provisioned/role read and the setup-complete write, with no BLE
//! adapter and no MQTT broker. GitLab #58, `plan/control/ble/peripheral.md` §6.3.
//!
//! It carries the assertion `plan/control/ble/plan.md` §9.6 states for every mutation:
//!
//! > The BLE surface's effects and the signed command's effects must be identical, asserted **at
//! > the store layer**, not the API layer. Two surfaces that agree today because someone wrote them
//! > carefully is not the same as two surfaces that cannot disagree.
//!
//! `ble_and_signed_writes_leave_the_same_config_behind` is that test. It is close to a tautology,
//! because both paths reach `ConfigApplier::apply_setup_complete` — which is the point. It fails
//! the day someone writes a second way to set `setup:`.

use std::sync::Arc;

use control_ble::host::{HubRole, Origin, SetupState, SetupStatus};
use fiber_app::libs::ble_host::setup_state::ApplierSetupState;
use fiber_app::libs::config::SetupRole;
use fiber_app::ConfigApplier;

/// A Hub seeded exactly as one out of the box: a config with no `setup:` section at all.
struct Hub {
    applier: Arc<ConfigApplier>,
    dir: tempfile::TempDir,
}

impl Hub {
    fn new() -> Hub {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("fiber.config.yaml"),
            "system:\n  device_label: TEST\neye:\n  enabled: true\n  tags: []\n",
        )
        .expect("seed fiber.config.yaml");
        let applier = Arc::new(
            ConfigApplier::new(dir.path())
                .expect("ConfigApplier on tempdir")
                .with_cluster_dir(dir.path().join("cluster")),
        );
        Hub { applier, dir }
    }

    /// The role marker a cluster join leaves (`plan/control/cluster.md` §3): the one state in
    /// which `client` is a role this Hub may record.
    fn joined(self) -> Hub {
        std::fs::create_dir_all(self.dir.path().join("cluster")).expect("cluster dir");
        std::fs::write(self.dir.path().join("cluster/role"), "client").expect("marker");
        self
    }

    fn seam(&self) -> ApplierSetupState {
        ApplierSetupState::new(self.applier.clone(), self.dir.path())
    }

    fn config(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("fiber.config.yaml")).expect("read back")
    }
}

#[test]
fn ble_and_signed_writes_leave_the_same_config_behind() {
    let via_ble = Hub::new();
    let via_signed = Hub::new();
    assert_eq!(
        via_ble.config(),
        via_signed.config(),
        "the two Hubs must start identical or the comparison below proves nothing"
    );

    // The BLE path: FB17's handler calls exactly this.
    assert!(
        via_ble
            .seam()
            .mark_complete(Origin::Ble, HubRole::Server)
            .success
    );

    // The signed path: `MqttCommand::SetupComplete`'s dispatch arm calls exactly this.
    assert!(
        via_signed
            .applier
            .apply_setup_complete(SetupRole::Server)
            .success
    );

    assert_eq!(
        via_ble.config(),
        via_signed.config(),
        "FB17 and the signed setup-complete must be indistinguishable on disk"
    );
}

#[test]
fn a_hub_out_of_the_box_reads_as_unprovisioned_and_says_so_after_the_write() {
    let hub = Hub::new().joined();
    let seam = hub.seam();
    assert_eq!(
        seam.read().expect("readable"),
        SetupStatus::default(),
        "a config with no setup: section is a genuine 'nobody set this up', not a failure"
    );

    assert!(seam.mark_complete(Origin::Ble, HubRole::Client).success);
    assert_eq!(
        seam.read().expect("readable"),
        SetupStatus {
            provisioned: true,
            role: Some(HubRole::Client),
        }
    );
}

#[test]
fn the_state_survives_a_reopen_which_is_what_surviving_a_restart_means_here() {
    // #58's second acceptance criterion. A restart is a fresh process reading the same file, which
    // is what a second `ApplierSetupState` over the same directory is.
    let hub = Hub::new();
    assert!(
        hub.seam()
            .mark_complete(Origin::Ble, HubRole::Server)
            .success
    );

    let after_restart = ApplierSetupState::new(
        Arc::new(ConfigApplier::new(hub.dir.path()).expect("a second applier, as a restart gets")),
        hub.dir.path(),
    );
    assert_eq!(
        after_restart.read().expect("readable"),
        SetupStatus {
            provisioned: true,
            role: Some(HubRole::Server),
        }
    );
}

#[test]
fn losing_the_config_file_is_how_a_factory_reset_clears_it() {
    // #58's third acceptance criterion, at the level this test can honestly assert: the wipe takes
    // everything under /data, the config file included, and `fiber-config-seed.sh` re-copies the
    // pristine template on the next boot. So "factory reset clears it" is a property of *where the
    // state lives*, not of a clearing step someone has to remember to write.
    //
    // What this cannot assert is the wipe itself: phase 2 only runs from an image that ships the
    // executor, and `factory_reset::executor_installed()` is false on a dev checkout.
    let hub = Hub::new();
    assert!(
        hub.seam()
            .mark_complete(Origin::Ble, HubRole::Server)
            .success
    );
    assert!(hub.seam().read().expect("readable").provisioned);

    std::fs::remove_file(hub.dir.path().join("fiber.config.yaml")).expect("the wipe");
    std::fs::write(
        hub.dir.path().join("fiber.config.yaml"),
        include_str!("../fiber.config.yaml"),
    )
    .expect("the seed script re-copies the shipped template");

    assert_eq!(
        hub.seam().read().expect("readable"),
        SetupStatus::default(),
        "a refurbished Hub must not claim to be configured"
    );
}

#[test]
fn both_surfaces_refuse_a_re_role_once_the_hub_is_provisioned() {
    // §9.6 again, on the refusal rather than on the write: the role lock lives in
    // `apply_setup_complete`, so it is one rule with one wording, and neither surface can be the
    // soft one. Before the 2026-09-16 review the last write simply won, which made FB19's
    // client-role gate a formality — write FB17 `server`, then read the credential.
    let via_ble = Hub::new().joined();
    let via_signed = Hub::new().joined();
    assert!(
        via_ble
            .seam()
            .mark_complete(Origin::Ble, HubRole::Client)
            .success
    );
    assert!(
        via_signed
            .applier
            .apply_setup_complete(SetupRole::Client)
            .success
    );
    let settled = via_ble.config();

    let refused_ble = via_ble.seam().mark_complete(Origin::Ble, HubRole::Server);
    assert!(!refused_ble.success, "{}", refused_ble.message);
    let refused_signed = via_signed.applier.apply_setup_complete(SetupRole::Server);
    assert!(!refused_signed.success);

    assert_eq!(
        refused_ble.message,
        refused_signed.error_message.expect("a reason"),
        "one rule, one wording — FB17's envelope and the signed command say the same thing"
    );
    assert!(
        refused_ble.message.contains("factory reset"),
        "peripheral.md §6.3's way back must reach the app: {}",
        refused_ble.message
    );
    assert_eq!(via_ble.config(), settled, "neither file moved");
    assert_eq!(via_signed.config(), settled);
}

/// cluster.md §3: `client` is refused on both surfaces unless the role marker says `client`, with
/// one wording, and nothing moves on disk.
#[test]
fn both_surfaces_refuse_the_client_role_before_the_join() {
    let via_ble = Hub::new();
    let via_signed = Hub::new();
    let before = via_ble.config();

    let refused_ble = via_ble.seam().mark_complete(Origin::Ble, HubRole::Client);
    let refused_signed = via_signed.applier.apply_setup_complete(SetupRole::Client);
    assert!(!refused_ble.success);
    assert!(!refused_signed.success);
    assert_eq!(
        refused_ble.message,
        refused_signed.error_message.expect("a reason")
    );
    assert!(refused_ble.message.contains("has not joined a Server"));
    assert_eq!(via_ble.config(), before);
    assert_eq!(via_signed.config(), before);
}
