//! Simulated FB18 — the export-destination write, with no BLE adapter and no MQTT broker.
//! GitLab #59, `plan/control/ble/peripheral.md` §6.4.
//!
//! The twin of `setup_state_simulated.rs`, carrying the same assertion `plan/control/ble/plan.md`
//! §9.6 states for every mutation:
//!
//! > The BLE surface's effects and the signed command's effects must be identical, asserted **at
//! > the store layer**, not the API layer. Two surfaces that agree today because someone wrote them
//! > carefully is not the same as two surfaces that cannot disagree.
//!
//! `ble_and_signed_writes_leave_the_same_config_behind` is that test. It is close to a tautology,
//! because both paths reach `ConfigApplier::apply_export_destination` — which is the point. It
//! fails the day someone writes a second way to write `mqtt.export.destinations[]`.

use std::sync::Arc;

use control_ble::host::{ExportDestination, ExportDestinationRequest, Origin};
use fiber_app::libs::ble_host::export_destination::ApplierExportDestination;
use fiber_app::ConfigApplier;

/// A Hub seeded as the image ships one: `mqtt.export` off, with the anonymous `local` destination
/// the shipped config's own comment describes.
struct Hub {
    applier: Arc<ConfigApplier>,
    dir: tempfile::TempDir,
}

impl Hub {
    fn new() -> Hub {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("fiber.config.yaml"),
            "system:\n  device_label: TEST\nmqtt:\n  export:\n    enabled: false\n    \
             destinations:\n      - broker_id: local\n        enabled: true\n        host: \
             localhost\n        port: 1883\n",
        )
        .expect("seed fiber.config.yaml");
        let applier = Arc::new(ConfigApplier::new(dir.path()).expect("ConfigApplier on tempdir"));
        Hub { applier, dir }
    }

    fn seam(&self) -> ApplierExportDestination {
        ApplierExportDestination::new(self.applier.clone())
    }

    fn config(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("fiber.config.yaml")).expect("read back")
    }
}

fn request(host: &str, username: &str, password: &str) -> ExportDestinationRequest {
    serde_json::from_value(serde_json::json!({
        "host": host, "username": username, "password": password,
    }))
    .expect("an FB18 request")
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

    // The BLE path: FB18's handler calls exactly this.
    assert!(
        via_ble
            .seam()
            .set(Origin::Ble, &request("hub-1.local", "fiber", "s3cret"))
            .success
    );

    // The signed path: `MqttCommand::SetExportDestination`'s dispatch arm calls exactly this.
    // `enabled` defaults to true on both surfaces, so the two inputs really are the same input.
    assert!(
        via_signed
            .applier
            .apply_export_destination("hub-1.local", "fiber", "s3cret", true)
            .success
    );

    assert_eq!(
        via_ble.config(),
        via_signed.config(),
        "FB18 and the signed export-destination-set must be indistinguishable on disk"
    );
}

#[test]
fn both_surfaces_refuse_the_same_half_filled_credential_and_write_nothing() {
    // The rule lives in the applier, so it cannot hold on one surface and not the other. A blank
    // username silently means *anonymous* downstream, which is the Hub connecting as nobody to a
    // broker the operator believed it had authenticated to.
    let via_ble = Hub::new();
    let via_signed = Hub::new();
    let untouched = via_ble.config();

    let refused_ble = via_ble
        .seam()
        .set(Origin::Ble, &request("hub-1.local", "", "s3cret"));
    assert!(!refused_ble.success);

    let refused_signed =
        via_signed
            .applier
            .apply_export_destination("hub-1.local", "", "s3cret", true);
    assert!(!refused_signed.success);

    assert_eq!(
        refused_ble.message,
        refused_signed.error_message.expect("a reason"),
        "one rule, one wording — the app and the portal must not be told different things"
    );
    assert_eq!(via_ble.config(), untouched, "nothing was written");
    assert_eq!(via_signed.config(), untouched);
}

#[test]
fn the_destination_survives_a_reopen_which_is_what_surviving_a_restart_means_here() {
    // #59's acceptance criterion. `mqtt.export` has no live reload, so "applies at the next
    // restart" is only true if the *file* carries it — which is what a second applier over the
    // same directory reads.
    let hub = Hub::new();
    assert!(
        hub.seam()
            .set(Origin::Ble, &request("hub-1.local", "fiber", "s3cret"))
            .success
    );

    let after_restart =
        std::fs::read_to_string(hub.dir.path().join("fiber.config.yaml")).expect("read back");
    let doc: serde_yaml::Value = serde_yaml::from_str(&after_restart).expect("parse");
    let destinations = doc["mqtt"]["export"]["destinations"]
        .as_sequence()
        .expect("a list");
    assert_eq!(destinations.len(), 2, "the shipped `local` one survives");
    assert_eq!(destinations[1]["broker_id"].as_str(), Some("remote"));
    assert_eq!(destinations[1]["port"].as_u64(), Some(8883));
    assert_eq!(
        doc["mqtt"]["export"]["enabled"].as_bool(),
        Some(true),
        "enabling a destination enables the block it lives in, in the same atomic write"
    );
}
