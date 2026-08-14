//! Simulated FB0E "EYE Tag Add" enrollment without a BLE stack.
//!
//! Mirrors `node_add_simulated.rs` (FB0D): drive the FB0E write flow
//! (parse → prepare → apply_beacon_tag_config → store) against a real
//! `ConfigApplier` on a temp dir, and assert on the resulting response slot and
//! the persisted `eye.tags[]`. The live-config / in-memory-state seed in the
//! real handler goes through process-global handles that only exist while the
//! EYE monitor is running, so it is intentionally not exercised here — the core
//! persistence (apply_beacon_tag_config) is what this test pins down.

use fiber_app::libs::ble::gatt::beacon_add::{
    self, BeaconAddRequest, BeaconAddResponse, SharedResult,
};
use fiber_app::libs::config_applier::ConfigApplier;

/// Replicates the FB0E write handler's synchronous core: validate, persist,
/// store the final result into the slot. Returns the response an FB0E read
/// would then surface.
fn simulate_fb0e_write(
    slot: &SharedResult,
    applier: &ConfigApplier,
    req: &BeaconAddRequest,
) -> BeaconAddResponse {
    let resp = match beacon_add::prepare(req) {
        Err(msg) => BeaconAddResponse {
            success: false,
            message: msg,
        },
        Ok(prepared) => {
            let result =
                applier.apply_beacon_tag_config(prepared.mac.clone(), prepared.name.clone());
            if result.success {
                BeaconAddResponse {
                    success: true,
                    message: String::new(),
                }
            } else {
                BeaconAddResponse {
                    success: false,
                    message: result
                        .error_message
                        .unwrap_or_else(|| "unknown error".to_string()),
                }
            }
        }
    };
    beacon_add::store(slot, resp.clone());
    resp
}

fn applier_on(dir: &std::path::Path) -> ConfigApplier {
    std::fs::write(
        dir.join("fiber.config.yaml"),
        "system:\n  device_label: TEST\n",
    )
    .expect("seed fiber.config.yaml");
    ConfigApplier::new(dir).expect("ConfigApplier on tempdir")
}

fn req(mac: &str, name: Option<&str>) -> BeaconAddRequest {
    BeaconAddRequest {
        mac: mac.to_string(),
        name: name.map(|s| s.to_string()),
    }
}

#[test]
fn fb0e_add_persists_tag_into_config() {
    let tmp = tempfile::tempdir().unwrap();
    let applier = applier_on(tmp.path());
    let slot = beacon_add::new_slot();

    // Lowercase MAC on the wire is normalized to uppercase before persisting.
    let resp = simulate_fb0e_write(&slot, &applier, &req("7c:d9:f4:10:00:00", Some("Lobby")));

    assert!(resp.success, "expected success, got {resp:?}");
    assert!(resp.message.is_empty());

    // FB0E read would return the same final (non-pending) result.
    let read = beacon_add::read(&slot);
    assert!(read.success);

    // The tag was persisted (uppercase) into eye.tags[] in fiber.config.yaml.
    let yaml = std::fs::read_to_string(tmp.path().join("fiber.config.yaml")).unwrap();
    assert!(
        yaml.contains("7C:D9:F4:10:00:00"),
        "config should contain the uppercased MAC:\n{yaml}"
    );
    assert!(yaml.contains("Lobby"), "config should contain the tag name");
    assert!(
        yaml.contains("eye"),
        "config should have gained an eye section"
    );
}

#[test]
fn fb0e_add_rejects_invalid_mac_and_does_not_persist() {
    let tmp = tempfile::tempdir().unwrap();
    let applier = applier_on(tmp.path());
    let slot = beacon_add::new_slot();

    let resp = simulate_fb0e_write(&slot, &applier, &req("not-a-mac", None));

    assert!(
        !resp.success,
        "invalid MAC must be rejected before persisting"
    );
    assert!(resp.message.contains("invalid MAC"));
    // Nothing was written to eye.tags[].
    let yaml = std::fs::read_to_string(tmp.path().join("fiber.config.yaml")).unwrap();
    assert!(
        !yaml.contains("tags"),
        "no tag should have been persisted:\n{yaml}"
    );
}

#[test]
fn fb0e_add_is_idempotent_no_duplicate_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let applier = applier_on(tmp.path());
    let slot = beacon_add::new_slot();

    let _ = simulate_fb0e_write(&slot, &applier, &req("AA:BB:CC:DD:EE:FF", Some("first")));
    let _ = simulate_fb0e_write(&slot, &applier, &req("aa:bb:cc:dd:ee:ff", Some("renamed")));

    let yaml = std::fs::read_to_string(tmp.path().join("fiber.config.yaml")).unwrap();
    let occurrences = yaml.matches("AA:BB:CC:DD:EE:FF").count();
    assert_eq!(
        occurrences, 1,
        "re-adding the same MAC must upsert, not duplicate:\n{yaml}"
    );
}
