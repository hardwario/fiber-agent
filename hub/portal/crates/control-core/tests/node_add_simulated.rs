//! Simulated FB0D Node-Add — the full enrolment path with no BLE adapter and no ChirpStack.
//!
//! FB0D is served by `control-ble` now, so the parse / pending-guard / response-buffer half of this
//! test moved into the crate (`tests/gatt_handlers.rs`). What stays here is the half that is still
//! this crate's: `NodeEnroll` over `libs::lorawan`'s node-add path, and what it leaves behind in
//! `fiber.config.yaml`, the shared configs list and the shared state.
//!
//! The dependencies are real but temporary:
//!   - `ConfigApplier` on a tempdir (a real YAML write, no `/data` needed)
//!   - in-memory `lorawan_configs` + `lorawan_state`
//!   - `storage: None` (the epoch bump is skipped, as on a no-storage boot)
//!   - ChirpStack offline → provisioning fails, which the add path treats as best-effort and
//!     continues to the config save. That is the behaviour being pinned: an installer standing in
//!     front of a Hub whose broker is unreachable must still be able to enrol a Node.

use std::sync::Arc;

use control_ble::host::{NodeAddRequest, NodeAddResponse, NodeEnroll};
use fiber_app::libs::ble_host::node_enroll::LorawanNodeEnroll;
use fiber_app::libs::lorawan::{
    create_shared_lorawan_sensor_configs, create_shared_lorawan_state, NodeAddDeps,
};
use fiber_app::ConfigApplier;

fn deps_on(
    dir: &std::path::Path,
) -> (
    NodeAddDeps,
    fiber_app::libs::lorawan::SharedLoRaWANSensorConfigs,
    fiber_app::libs::lorawan::SharedLoRaWANState,
) {
    // `apply_lorawan_sensor_config` edits an existing fiber.config.yaml (the lorawan.sensors array
    // lives there). On a device it always exists; seed a minimal valid one.
    std::fs::write(
        dir.join("fiber.config.yaml"),
        "system:\n  device_label: TEST\n",
    )
    .expect("seed fiber.config.yaml");
    let applier = ConfigApplier::new(dir).expect("ConfigApplier on tempdir");
    let configs = create_shared_lorawan_sensor_configs(vec![]);
    let state = create_shared_lorawan_state(false);
    let deps = NodeAddDeps {
        config_applier: Some(Arc::new(applier)),
        storage: None,
        lorawan_configs: Some(configs.clone()),
        lorawan_state: Some(state.clone()),
    };
    (deps, configs, state)
}

fn req(deveui: &str) -> NodeAddRequest {
    NodeAddRequest {
        deveui: deveui.to_string(),
        joineui: "8899aabbccddeeff".to_string(),
        appkey: "00112233445566778899AABBCCDDEEFF".to_string(),
        name: "Fridge 1".to_string(),
        serial_number: "SN-001".to_string(),
        profile_id: None,
    }
}

/// The add path never reads the config file — it writes through the applier — so the path handed to
/// the seam here is only what FB11's list would resolve against. `node_remove_simulated.rs` is where
/// that half is exercised.
fn enrol(deps: &NodeAddDeps, req: &NodeAddRequest) -> NodeAddResponse {
    LorawanNodeEnroll::new(deps.clone(), std::env::temp_dir().join("fiber.config.yaml")).add(req)
}

#[test]
fn fb0d_add_persists_config_and_state_without_chirpstack() {
    let tmp = tempfile::tempdir().unwrap();
    let (deps, configs, state) = deps_on(tmp.path());

    let resp = enrol(&deps, &req("0011223344556677"));

    // ChirpStack is offline, but the config save still succeeds → success.
    assert!(
        resp.success,
        "expected success via config save, got {resp:?}"
    );
    assert_eq!(resp.deveui, "0011223344556677");
    assert!(!resp.pending, "the final answer is not pending");

    // The Node is now in the in-memory configs list…
    assert!(
        configs
            .read()
            .unwrap()
            .iter()
            .any(|c| c.dev_eui == "0011223344556677"),
        "lorawan_configs should contain the new Node"
    );
    // …and an optimistic stub is in shared state, so it shows before the first uplink.
    assert!(
        state
            .read()
            .unwrap()
            .sensors
            .contains_key("0011223344556677"),
        "lorawan_state should hold the Node stub"
    );
    // And the dev_eui was persisted into fiber.config.yaml (lorawan.sensors).
    let yaml = std::fs::read_to_string(tmp.path().join("fiber.config.yaml")).unwrap();
    assert!(
        yaml.contains("0011223344556677"),
        "fiber.config.yaml should contain the Node dev_eui after the add"
    );
}

#[test]
fn fb0d_add_rejects_an_invalid_appkey_and_does_not_persist() {
    let tmp = tempfile::tempdir().unwrap();
    let (deps, configs, state) = deps_on(tmp.path());

    let mut bad = req("1122334455667788");
    bad.appkey = "deadbeef".to_string(); // too short

    let resp = enrol(&deps, &bad);

    assert!(
        !resp.success,
        "an invalid appkey must be rejected before any provisioning"
    );
    assert!(!resp.pending);
    assert_eq!(
        resp.deveui, "1122334455667788",
        "and the response says which"
    );
    // Nothing persisted.
    assert!(configs.read().unwrap().is_empty());
    assert!(state.read().unwrap().sensors.is_empty());
}

#[test]
fn fb0d_add_is_idempotent_no_duplicate_config_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let (deps, configs, _state) = deps_on(tmp.path());

    let _ = enrol(&deps, &req("aabbccddeeff0011"));
    let _ = enrol(&deps, &req("aabbccddeeff0011"));

    let count = configs
        .read()
        .unwrap()
        .iter()
        .filter(|c| c.dev_eui == "aabbccddeeff0011")
        .count();
    assert_eq!(
        count, 1,
        "re-adding the same dev_eui must not duplicate the config entry"
    );
}

#[test]
fn a_hub_with_no_config_applier_refuses_by_name_rather_than_generically() {
    // v1's add path returned a hard error here. The phone shows this string to an installer, so it
    // has to identify the Hub as the problem — "failed" would send them back to the Node's QR code.
    let resp = enrol(&NodeAddDeps::default(), &req("0011223344556677"));
    assert!(!resp.success);
    assert!(
        resp.message.contains("config applier"),
        "the refusal must name what is missing: {}",
        resp.message
    );
}
