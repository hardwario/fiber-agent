//! Simulated FB0D Sticker-Add path — exercises the full enrollment logic
//! WITHOUT a BLE adapter and WITHOUT a running ChirpStack.
//!
//! The bluer GATT closure in `service.rs` does: auth-gate → parse → prepare →
//! spawn_blocking(add_lorawan_sticker) → set_last_result. Everything except the
//! auth-gate and the bluer transport is plain logic, so we reproduce that
//! sequence here against real-but-temp dependencies:
//!   - ConfigApplier on a tempdir (real YAML write, no /data needed)
//!   - in-memory lorawan_configs + lorawan_state
//!   - storage = None (epoch bump is skipped, as on a no-storage boot)
//!   - ChirpStack offline → provision_sticker_otaa fails, which the add path
//!     treats as best-effort (logs and continues to the config save).

use std::sync::Arc;

use fiber_app::libs::ble::gatt::sticker::{self, StickerAddRequest, StickerAddResponse};
use fiber_app::libs::lorawan::{
    add_lorawan_sticker, create_shared_lorawan_sensor_configs, create_shared_lorawan_state,
    StickerAddDeps,
};
use fiber_app::ConfigApplier;

/// Reproduce exactly what the FB0D write closure does after the auth gate, and
/// return the response it would store. We return the response locally (rather
/// than read it back via `sticker::last_result()`) because that global is
/// shared across these parallel tests — asserting on the local value avoids a
/// cross-test race. `set_last_result` is still called for realism.
fn simulate_fb0d_write(deps: &StickerAddDeps, req: &StickerAddRequest) -> StickerAddResponse {
    let resp = match sticker::prepare(req) {
        Err(msg) => StickerAddResponse {
            success: false,
            message: msg,
            deveui: req.deveui.trim().to_lowercase(),
        },
        Ok(prepared) => {
            let dev_eui = prepared.dev_eui.clone();
            let result = add_lorawan_sticker(
                deps,
                prepared.dev_eui,
                prepared.name,
                prepared.serial_number,
                prepared.activation,
            );
            match result {
                Ok(()) => StickerAddResponse {
                    success: true,
                    message: "sticker enrolled".to_string(),
                    deveui: dev_eui,
                },
                Err(e) => StickerAddResponse { success: false, message: e, deveui: dev_eui },
            }
        }
    };
    sticker::set_last_result(resp.clone());
    resp
}

fn deps_on(dir: &std::path::Path) -> (StickerAddDeps, fiber_app::libs::lorawan::SharedLoRaWANSensorConfigs, fiber_app::libs::lorawan::SharedLoRaWANState) {
    // apply_lorawan_sensor_config edits an existing fiber.config.yaml (the
    // lorawan.sensors array lives there). On a device it always exists; for
    // the test we seed a minimal valid one.
    std::fs::write(dir.join("fiber.config.yaml"), "system:\n  device_label: TEST\n")
        .expect("seed fiber.config.yaml");
    let applier = ConfigApplier::new(dir).expect("ConfigApplier on tempdir");
    let configs = create_shared_lorawan_sensor_configs(vec![]);
    let state = create_shared_lorawan_state(false);
    let deps = StickerAddDeps {
        config_applier: Some(Arc::new(applier)),
        storage: None,
        lorawan_configs: Some(configs.clone()),
        lorawan_state: Some(state.clone()),
    };
    (deps, configs, state)
}

fn req(deveui: &str) -> StickerAddRequest {
    StickerAddRequest {
        deveui: deveui.to_string(),
        joineui: "8899aabbccddeeff".to_string(),
        appkey: "00112233445566778899AABBCCDDEEFF".to_string(),
        name: "Fridge 1".to_string(),
        serial_number: "SN-001".to_string(),
    }
}

#[test]
fn fb0d_add_persists_config_and_state_without_chirpstack() {
    let tmp = tempfile::tempdir().unwrap();
    let (deps, configs, state) = deps_on(tmp.path());

    let resp = simulate_fb0d_write(&deps, &req("0011223344556677"));

    // ChirpStack is offline, but config save still succeeds → success.
    assert!(resp.success, "expected success via config save, got {:?}", resp);
    assert_eq!(resp.deveui, "0011223344556677");
    assert_eq!(resp.message, "sticker enrolled");

    // The sticker is now in the in-memory configs list…
    assert!(
        configs.read().unwrap().iter().any(|c| c.dev_eui == "0011223344556677"),
        "lorawan_configs should contain the new sticker"
    );
    // …and an optimistic stub is in shared state (so it shows before first uplink).
    assert!(
        state.read().unwrap().sensors.contains_key("0011223344556677"),
        "lorawan_state should hold the sticker stub"
    );
    // The sticker dev_eui was persisted into fiber.config.yaml (lorawan.sensors).
    let yaml = std::fs::read_to_string(tmp.path().join("fiber.config.yaml")).unwrap();
    assert!(
        yaml.contains("0011223344556677"),
        "fiber.config.yaml should contain the sticker dev_eui after the add"
    );
}

#[test]
fn fb0d_add_rejects_invalid_appkey_and_does_not_persist() {
    let tmp = tempfile::tempdir().unwrap();
    let (deps, configs, state) = deps_on(tmp.path());

    let mut bad = req("1122334455667788");
    bad.appkey = "deadbeef".to_string(); // too short

    let resp = simulate_fb0d_write(&deps, &bad);

    assert!(!resp.success, "invalid appkey must be rejected before any provisioning");
    assert!(resp.message.contains("appkey"));
    assert_eq!(resp.deveui, "1122334455667788");
    // Nothing persisted.
    assert!(configs.read().unwrap().is_empty());
    assert!(state.read().unwrap().sensors.is_empty());
}

#[test]
fn fb0d_add_is_idempotent_no_duplicate_config_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let (deps, configs, _state) = deps_on(tmp.path());

    let _ = simulate_fb0d_write(&deps, &req("aabbccddeeff0011"));
    let _ = simulate_fb0d_write(&deps, &req("aabbccddeeff0011"));

    let count = configs
        .read()
        .unwrap()
        .iter()
        .filter(|c| c.dev_eui == "aabbccddeeff0011")
        .count();
    assert_eq!(count, 1, "re-adding the same dev_eui must not duplicate the config entry");
}
