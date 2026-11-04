//! Simulated external-gateway add/remove — the legacy path with no ChirpStack, on a Hub with no
//! rendered radio config.
//!
//! The two MQTT arms (`AddExternalGateway`, `RemoveExternalGateway`) moved out of
//! `mqtt/monitor.rs` into `libs/lorawan/external_gateway.rs` so the v2 radio gate in front of them
//! is testable (`radio_gate_simulated.rs`). This file is the other half: on a v1-style Hub — no
//! `PROXIMOS_CONFIG` in this process, and no `/run/proximos/control/config.yaml` on a build host —
//! the gate stays open and the arms behave exactly as they did inline.
//!
//! The dependencies are real but temporary: a `ConfigApplier` on a tempdir, and ChirpStack
//! offline, so registration and deregistration fail and the path continues past them. That is the
//! behaviour being pinned: local config is the source of truth.

use fiber_app::libs::lorawan::{add_external_gateway, remove_external_gateway};
use fiber_app::ConfigApplier;

fn hub() -> (ConfigApplier, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("fiber.config.yaml"),
        "system:\n  device_label: TEST\n",
    )
    .expect("seed fiber.config.yaml");
    let applier = ConfigApplier::new(dir.path()).expect("ConfigApplier on tempdir");
    (applier, dir)
}

fn config(dir: &tempfile::TempDir) -> String {
    std::fs::read_to_string(dir.path().join("fiber.config.yaml")).expect("config")
}

#[test]
fn without_a_rendered_radio_config_add_and_remove_still_reach_the_config() {
    assert!(
        std::env::var_os("PROXIMOS_CONFIG").is_none(),
        "this file pins the v1-style Hub; a rendered radio config would gate it"
    );
    let (applier, dir) = hub();

    add_external_gateway(
        Some(&applier),
        "70b3d54900aa0001".to_string(),
        "Basement".to_string(),
    )
    .expect("ChirpStack is offline, the config save still succeeds");
    let saved = config(&dir);
    assert!(saved.contains("70b3d54900aa0001"), "{saved}");
    assert!(saved.contains("Basement"), "{saved}");

    remove_external_gateway(Some(&applier), "70b3d54900aa0001".to_string())
        .expect("ChirpStack is offline, the config removal still succeeds");
    assert!(!config(&dir).contains("70b3d54900aa0001"));
}

#[test]
fn an_absent_applier_is_still_the_hard_error() {
    let e = add_external_gateway(None, "70b3d54900aa0001".to_string(), "x".to_string())
        .expect_err("no applier");
    assert!(e.contains("Config applier"), "{e}");
    let e = remove_external_gateway(None, "70b3d54900aa0001".to_string()).expect_err("no applier");
    assert!(e.contains("Config applier"), "{e}");
}
