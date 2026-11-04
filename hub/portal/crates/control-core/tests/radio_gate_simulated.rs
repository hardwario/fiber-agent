//! The v2 radio gate at every legacy LoRaWAN entry point — `plan/control/radio/lorawan.md` §9,
//! amended 2026-10-09 (GitLab #75).
//!
//! `libs::radio_gate::disabled_reason` is pure and unit-tested in its module. What this file pins
//! is the wiring: that each entry point which reaches the legacy ChirpStack client
//! (`libs/lorawan/provisioning.rs`) actually asks the gate, reading the rendered config the way a
//! Hub does, and refuses **before anything is written**.
//!
//! The rendered config is process-global (`PROXIMOS_CONFIG`), which is why this is a test binary
//! of its own: every test here runs on a Hub whose `radio.mode` is `lorawan`, and none of the
//! other binaries sees that file. The legacy behaviour on a Hub with no rendered config — the
//! other half of the decision — is what `node_remove_simulated.rs` and
//! `external_gateway_simulated.rs` pin, in processes with no `PROXIMOS_CONFIG`.
//!
//! The BLE Node verbs FB0D/FB11/FB12 do not reach the legacy stack on such a Hub at all since
//! GitLab #118: they take the Node registry path. What is pinned here is that the shipped wiring
//! — the rendered config, the registry under `PROXIMOS_RADIO_DATA` — routes them there; the
//! registry path itself is `ble_nodes_registry.rs`'s.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use control_ble::host::{NodeAddRequest, NodeEnroll, NodeEntry, NodeRemoveTarget};
use fiber_app::libs::ble_host::node_enroll::LorawanNodeEnroll;
use fiber_app::libs::lorawan::{
    add_external_gateway, create_shared_lorawan_sensor_configs, create_shared_lorawan_state,
    remove_external_gateway, remove_lorawan_node, NodeAddDeps,
};
use fiber_app::ConfigApplier;

/// The `radio:` section as `proximosctl render` writes it, trimmed to what
/// `RadioConfig::from_device_config_yaml` requires — the same fixture `radio_gate`'s unit tests
/// use.
const V2_CONFIG: &str = r#"
radio:
  region: eu868
  mode: lorawan
  application:
    name: "ProXimos Sensors"
  device_profiles:
    otaa:
      default:
        name: "ProXimos Node (OTAA)"
        mac_version: LORAWAN_1_0_4
        reg_params_revision: RP002_1_0_4
        uplink_interval: 3600
        device_status_req_interval: 1
        decoder: "node"
    p2p:
      default:
        name: "ProXimos Node (P2P)"
        decoder: "node"
  p2p_radio:
    frequency_hz: 868100000
    spreading_factor: 10
    bandwidth_khz: 125
    coding_rate: "4/5"
    preamble: 8
  gateway:
    name: "ProXimos Gateway"
"#;

/// The one Node this Hub's registry holds, registered in the Portal on LoRaWAN.
const REGISTRY_NODE: &str = "5876070000000413";

/// Render the v2 config once for the whole process and point `PROXIMOS_CONFIG` at it, and
/// `PROXIMOS_RADIO_DATA` at a radio store whose registry holds [`REGISTRY_NODE`].
///
/// Every test calls this first, so the variables are set before any test reads them and never
/// change afterwards — no test observes a half-configured process. No test writes the registry.
fn on_a_v2_hub() -> &'static Path {
    static RENDERED: OnceLock<PathBuf> = OnceLock::new();
    RENDERED.get_or_init(|| {
        let dir = tempfile::tempdir().expect("tempdir").keep();
        let path = dir.join("config.yaml");
        std::fs::write(&path, V2_CONFIG).expect("render the v2 config");
        let radio = dir.join("radio");
        control_radio::registry::apply(
            &control_radio::store::Store::new(&radio),
            control_radio::Transport::Lorawan,
            &[control_radio::registry::CarrierRecord {
                dev_eui: REGISTRY_NODE.into(),
                serial: "2162164755".into(),
                profile: "default".into(),
                name: Some("Cold store".into()),
                app_key: Some("00112233445566778899aabbccddeeff".into()),
                lorawan: None,
            }],
            &[],
        )
        .expect("seed the registry");
        std::env::set_var("PROXIMOS_CONFIG", &path);
        std::env::set_var("PROXIMOS_RADIO_DATA", &radio);
        radio
    })
}

/// A Hub that enrolled one Node and one external gateway through the legacy path before its
/// radio config was rendered.
const SEEDED: &str = "\
system:
  device_label: TEST
lorawan:
  sensors:
    - dev_eui: 0011223344556677
      serial_number: SN-001
      name: Fridge 1
  gateways:
    - gateway_eui: 0016c001f11b34a6
      name: Roof
";

struct Hub {
    deps: NodeAddDeps,
    dir: tempfile::TempDir,
}

impl Hub {
    fn new() -> Hub {
        on_a_v2_hub();
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("fiber.config.yaml"), SEEDED).expect("seed");
        let applier = ConfigApplier::new(dir.path()).expect("ConfigApplier on tempdir");
        Hub {
            deps: NodeAddDeps {
                config_applier: Some(Arc::new(applier)),
                storage: None,
                lorawan_configs: Some(create_shared_lorawan_sensor_configs(vec![])),
                lorawan_state: Some(create_shared_lorawan_state(false)),
            },
            dir,
        }
    }

    fn config_path(&self) -> PathBuf {
        self.dir.path().join("fiber.config.yaml")
    }

    fn applier(&self) -> Option<&ConfigApplier> {
        self.deps.config_applier.as_deref()
    }

    /// Everything a refused command could have touched on disk: the config file and whatever
    /// else the applier keeps beside it (backups, markers).
    fn disk(&self) -> Vec<(String, Vec<u8>)> {
        snapshot(self.dir.path())
    }
}

fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir)
        .expect("read_dir")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.is_file())
        .map(|p| {
            let bytes = std::fs::read(&p).expect("read");
            (p.file_name().unwrap().to_string_lossy().into_owned(), bytes)
        })
        .collect();
    files.sort();
    files
}

/// The refusal must say why (the gate's reason, with the mode) and what to run instead.
fn assert_refusal(e: &str, verb: &str) {
    assert!(e.contains("legacy LoRaWAN stack disabled"), "{e}");
    assert!(e.contains("radio.mode=lorawan"), "{e}");
    assert!(
        e.contains(&format!("proximosctl control.radio {verb}")),
        "the refusal must name `{verb}`: {e}"
    );
}

#[test]
fn the_mqtt_node_remove_refuses_and_writes_nothing() {
    let hub = Hub::new();
    let before = hub.disk();

    let e = remove_lorawan_node(&hub.deps, "0011223344556677".to_string())
        .expect_err("the legacy remove must refuse on a v2 Hub");
    assert_refusal(&e, "node-remove");
    assert_eq!(before, hub.disk(), "a refusal must not touch the config");
}

/// GitLab #118: on a v2 Hub the BLE Node verbs leave the legacy stack. FB12 resolves against the
/// Node registry, so a Node only `lorawan.sensors[]` holds is not found there — refused before
/// anything runs, and neither the config nor the registry is touched. Until #118 this was the
/// gate's refusal, and a Node the Portal registered could not be removed from the phone at all.
#[test]
fn fb12_resolves_against_the_registry_and_never_reaches_the_legacy_stack() {
    let hub = Hub::new();
    let radio = on_a_v2_hub();
    let before = (hub.disk(), snapshot(radio));

    let out = LorawanNodeEnroll::new(hub.deps.clone(), hub.config_path())
        .remove(&NodeRemoveTarget::Serial("SN-001".to_string()));
    assert!(!out.success, "FB12 reported success: {}", out.message);
    assert!(
        out.message
            .contains("no node is enrolled with serial SN-001"),
        "{}",
        out.message
    );
    assert!(
        !out.message.contains("legacy LoRaWAN stack disabled"),
        "the legacy path was taken: {}",
        out.message
    );
    assert_eq!(before, (hub.disk(), snapshot(radio)));
}

/// FB11 on a v2 Hub lists the registry the Portal writes, not `lorawan.sensors[]`.
#[test]
fn fb11_lists_the_registry_not_the_legacy_sensors() {
    let hub = Hub::new();
    let list = LorawanNodeEnroll::new(hub.deps.clone(), hub.config_path())
        .list()
        .expect("FB11 answers");
    assert_eq!(
        list,
        vec![NodeEntry {
            deveui: REGISTRY_NODE.into(),
            serial: Some("80e00413".into()),
            name: Some("Cold store".into()),
        }]
    );
}

/// FB0D on a v2 Hub is no longer the gate's refusal: it takes the registry path, whose own
/// checks answer first — here the Portal's rule that a Node's serial is a STICKER serial.
#[test]
fn fb0d_takes_the_registry_path_on_a_v2_hub() {
    let hub = Hub::new();
    let before = hub.disk();
    let r = LorawanNodeEnroll::new(hub.deps.clone(), hub.config_path()).add(&NodeAddRequest {
        deveui: "5876070000000902".into(),
        profile_id: None,
        joineui: "0000000000000000".into(),
        appkey: "00112233445566778899aabbccddeeff".into(),
        name: "Cold room".into(),
        serial_number: "SN-002".into(),
    });
    assert!(!r.success, "{r:?}");
    assert!(r.message.contains("serial_number"), "{}", r.message);
    assert!(
        !r.message.contains("legacy LoRaWAN stack disabled"),
        "{}",
        r.message
    );
    assert_eq!(before, hub.disk());
}

#[test]
fn an_external_gateway_add_refuses_and_writes_nothing() {
    let hub = Hub::new();
    let before = hub.disk();

    let e = add_external_gateway(
        hub.applier(),
        "70b3d54900aa0001".to_string(),
        "Basement".to_string(),
    )
    .expect_err("the legacy gateway add must refuse on a v2 Hub");
    assert_refusal(&e, "gateway-add");
    assert_eq!(before, hub.disk());
}

#[test]
fn an_external_gateway_remove_refuses_and_writes_nothing() {
    let hub = Hub::new();
    let before = hub.disk();

    let e = remove_external_gateway(hub.applier(), "0016c001f11b34a6".to_string())
        .expect_err("the legacy gateway remove must refuse on a v2 Hub");
    assert_refusal(&e, "gateway-remove");
    assert_eq!(before, hub.disk());
}

#[test]
fn the_gate_comes_before_the_applier_check() {
    // Without an applier the legacy path's own hard error is "Config applier not initialized".
    // On a v2 Hub the gate answers first: that error would send an operator looking for a
    // broken agent when the real answer is "use the other stack".
    on_a_v2_hub();
    let e = remove_lorawan_node(&NodeAddDeps::default(), "0011223344556677".to_string())
        .expect_err("refused");
    assert_refusal(&e, "node-remove");
    let e = add_external_gateway(None, "70b3d54900aa0001".to_string(), "x".to_string())
        .expect_err("refused");
    assert_refusal(&e, "gateway-add");
    let e = remove_external_gateway(None, "70b3d54900aa0001".to_string()).expect_err("refused");
    assert_refusal(&e, "gateway-remove");
}
