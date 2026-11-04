//! Simulated FB11/FB12 — the Node list and remove paths with no BLE adapter and no ChirpStack.
//!
//! The mirror of `node_add_simulated.rs`, and it carries the assertion that
//! `plan/control/ble/plan.md` §9.6 states as the acceptance criterion for every mutation:
//!
//! > FB12's effects and the signed `remove-node` command's effects must be identical, asserted **at
//! > the store layer**, not the API layer. Two surfaces that agree today because someone wrote them
//! > carefully is not the same as two surfaces that cannot disagree.
//!
//! `ble_remove_and_mqtt_remove_leave_the_same_store_behind` is that test: it runs both surfaces on
//! two identically-seeded Hubs and compares what is left in `fiber.config.yaml`, in the shared
//! configs and in the shared state. Since `libs/lorawan/node_remove.rs` was extracted it is a
//! near-tautology — which is the point. It fails the day someone writes a second removal.
//!
//! The dependencies are real but temporary:
//!   - `ConfigApplier` on a tempdir (a real YAML write, no `/data` needed)
//!   - in-memory `lorawan_configs` + `lorawan_state`
//!   - `storage: None` (as on a no-storage boot)
//!   - ChirpStack offline → the deprovision fails, which the remove path treats as best-effort and
//!     continues past. That is the behaviour being pinned: an installer standing in front of a Hub
//!     whose network server is unreachable must still be able to un-enrol a Node.

use std::path::Path;
use std::sync::Arc;

use control_ble::host::{
    NodeAddRequest, NodeEnroll, NodeEntry, NodeRemoveOutcome, NodeRemoveTarget,
};
use fiber_app::libs::ble_host::node_enroll::LorawanNodeEnroll;
use fiber_app::libs::lorawan::{
    create_shared_lorawan_sensor_configs, create_shared_lorawan_state, NodeAddDeps,
    SharedLoRaWANSensorConfigs, SharedLoRaWANState,
};
use fiber_app::ConfigApplier;

/// The seeded Hub: a real applier over a tempdir, plus the two in-memory mirrors.
struct Hub {
    deps: NodeAddDeps,
    configs: SharedLoRaWANSensorConfigs,
    state: SharedLoRaWANState,
    dir: tempfile::TempDir,
}

impl Hub {
    fn new() -> Hub {
        let dir = tempfile::tempdir().expect("tempdir");
        // `apply_lorawan_sensor_config` edits an existing fiber.config.yaml (the lorawan.sensors
        // array lives there). On a device it always exists; seed a minimal valid one.
        std::fs::write(
            dir.path().join("fiber.config.yaml"),
            "system:\n  device_label: TEST\n",
        )
        .expect("seed fiber.config.yaml");
        let applier = ConfigApplier::new(dir.path()).expect("ConfigApplier on tempdir");
        let configs = create_shared_lorawan_sensor_configs(vec![]);
        let state = create_shared_lorawan_state(false);
        Hub {
            deps: NodeAddDeps {
                config_applier: Some(Arc::new(applier)),
                storage: None,
                lorawan_configs: Some(configs.clone()),
                lorawan_state: Some(state.clone()),
            },
            configs,
            state,
            dir,
        }
    }

    fn config_path(&self) -> std::path::PathBuf {
        self.dir.path().join("fiber.config.yaml")
    }

    fn seam(&self) -> LorawanNodeEnroll {
        LorawanNodeEnroll::new(self.deps.clone(), self.config_path())
    }

    fn enrol(&self, deveui: &str, serial: &str, name: &str) {
        let r = self.seam().add(&NodeAddRequest {
            deveui: deveui.to_string(),
            profile_id: None,
            joineui: "8899aabbccddeeff".to_string(),
            appkey: "00112233445566778899AABBCCDDEEFF".to_string(),
            name: name.to_string(),
            serial_number: serial.to_string(),
        });
        assert!(r.success, "seeding {deveui} failed: {}", r.message);
    }

    fn list(&self) -> Vec<NodeEntry> {
        self.seam().list().expect("the list is answerable")
    }

    fn remove(&self, target: NodeRemoveTarget) -> NodeRemoveOutcome {
        self.seam().remove(&target)
    }

    /// Everything a removal is allowed to have touched, as one comparable value.
    fn store(&self) -> (String, Vec<String>, Vec<String>) {
        let yaml = std::fs::read_to_string(self.config_path()).expect("config");
        let mut configs: Vec<String> = self
            .configs
            .read()
            .expect("lock")
            .iter()
            .map(|c| format!("{}|{:?}|{:?}", c.dev_eui, c.name, c.serial_number))
            .collect();
        configs.sort();
        let mut sensors: Vec<String> = self
            .state
            .read()
            .expect("lock")
            .sensors
            .keys()
            .cloned()
            .collect();
        sensors.sort();
        (yaml, configs, sensors)
    }
}

/// What the signed MQTT `remove-node` command does, reached the way `monitor.rs` reaches it.
fn mqtt_remove(hub: &Hub, dev_eui: &str) -> Result<(), String> {
    fiber_app::libs::lorawan::remove_lorawan_node(&hub.deps, dev_eui.to_string())
}

fn seeded() -> Hub {
    let hub = Hub::new();
    hub.enrol("0011223344556677", "SN-001", "Fridge 1");
    hub.enrol("8899aabbccddeeff", "SN-002", "Fridge 2");
    hub
}

// -------------------------------------------------------------------------------------------
// §9.6 — the acceptance criterion, at the store layer
// -------------------------------------------------------------------------------------------

#[test]
fn ble_remove_and_mqtt_remove_leave_the_same_store_behind() {
    let over_ble = seeded();
    let over_mqtt = seeded();
    assert_eq!(
        over_ble.store(),
        over_mqtt.store(),
        "the two Hubs must start identical or the comparison below proves nothing"
    );

    let out = over_ble.remove(NodeRemoveTarget::DevEui("0011223344556677".to_string()));
    assert!(out.success, "{}", out.message);
    mqtt_remove(&over_mqtt, "0011223344556677").expect("the signed command removes it");

    assert_eq!(
        over_ble.store(),
        over_mqtt.store(),
        "a BLE-originated removal and an MQTT-originated one must be indistinguishable in what \
         they leave in fiber.config.yaml, the shared configs and the shared state"
    );
    // And it is not vacuously equal — something actually went.
    assert_eq!(over_ble.list().len(), 1);
    assert!(!over_ble.store().0.contains("0011223344556677"));
}

#[test]
fn addressing_by_serial_is_the_same_removal_as_addressing_by_deveui() {
    // `plan/control/radio/plan.md` §5 "One identifier": a caller never needs to know the transport
    // to know how to address a node, whichever identifier it happens to hold. Both spellings must
    // therefore reach the same store, not merely both report success.
    let by_eui = seeded();
    let by_serial = seeded();

    assert!(
        by_eui
            .remove(NodeRemoveTarget::DevEui("8899aabbccddeeff".to_string()))
            .success
    );
    assert!(
        by_serial
            .remove(NodeRemoveTarget::Serial("SN-002".to_string()))
            .success
    );

    assert_eq!(by_eui.store(), by_serial.store());
}

// -------------------------------------------------------------------------------------------
// Refusals — nothing is written
// -------------------------------------------------------------------------------------------

#[test]
fn an_unknown_node_is_refused_by_name_and_nothing_is_written() {
    let hub = seeded();
    let before = hub.store();

    for (label, target) in [
        (
            "an unenrolled deveui",
            NodeRemoveTarget::DevEui("ffffffffffffffff".to_string()),
        ),
        (
            "an unenrolled serial",
            NodeRemoveTarget::Serial("SN-999".to_string()),
        ),
    ] {
        let out = hub.remove(target);
        assert!(!out.success, "{label} was accepted");
        assert!(
            out.message.contains("no node is enrolled"),
            "{label}: the refusal must say what happened, got {:?}",
            out.message
        );
    }
    assert_eq!(before, hub.store(), "a refusal must not touch the store");
}

#[test]
fn a_malformed_deveui_is_refused_before_the_config_is_touched() {
    // The same shape check the add path applies. Without it a typo reaches the applier and comes
    // back as "no such sensor", which sends an installer looking for a device that was never asked
    // for.
    let hub = seeded();
    let before = hub.store();
    for (bad, expect) in [
        ("00112233", "16 hex chars"),
        ("zzzzzzzzzzzzzzzz", "16 hex chars"),
        ("0000000000000000", "all zeros"),
    ] {
        let out = hub.remove(NodeRemoveTarget::DevEui(bad.to_string()));
        assert!(!out.success, "{bad} was accepted");
        assert!(
            out.message.contains(expect),
            "{bad}: {:?} must name the field",
            out.message
        );
    }
    assert_eq!(before, hub.store());
}

#[test]
fn an_ambiguous_serial_removes_nothing() {
    // Two devices carrying the same serial is a data defect upstream. Guessing which one the
    // installer meant would turn it into a deleted device, so the refusal names the ambiguity and
    // points at the identifier that cannot be ambiguous.
    let hub = Hub::new();
    hub.enrol("0011223344556677", "SN-DUP", "Fridge 1");
    hub.enrol("8899aabbccddeeff", "SN-DUP", "Fridge 2");
    let before = hub.store();

    let out = hub.remove(NodeRemoveTarget::Serial("SN-DUP".to_string()));
    assert!(!out.success);
    assert!(out.message.contains('2'), "{}", out.message);
    assert!(out.message.contains("deveui"), "{}", out.message);
    assert_eq!(before, hub.store(), "nothing was removed");
    assert_eq!(hub.list().len(), 2);
}

#[test]
fn a_serial_is_matched_exactly_after_trimming_never_fuzzily() {
    let hub = Hub::new();
    hub.enrol("0011223344556677", "SN-001", "Fridge 1");

    // A prefix, a suffix and a case difference are all different devices as far as this is
    // concerned — a fuzzy match here deletes the wrong one.
    for near_miss in ["SN-00", "SN-0011", "sn-001", "SN 001"] {
        let out = hub.remove(NodeRemoveTarget::Serial(near_miss.to_string()));
        assert!(!out.success, "{near_miss} matched {:?}", out.message);
    }
    // Surrounding whitespace is the one thing forgiven: it is what a scanner or a copy-paste adds.
    let out = hub.remove(NodeRemoveTarget::Serial("  SN-001 ".to_string()));
    assert!(out.success, "{}", out.message);
    assert!(hub.list().is_empty());
}

// -------------------------------------------------------------------------------------------
// The list — read from the persisted config, not the in-memory mirror
// -------------------------------------------------------------------------------------------

#[test]
fn the_list_is_what_the_config_file_holds_and_a_removal_shows_up_in_it() {
    let hub = seeded();
    let mut listed = hub.list();
    listed.sort_by(|a, b| a.deveui.cmp(&b.deveui));
    assert_eq!(
        listed,
        vec![
            NodeEntry {
                deveui: "0011223344556677".to_string(),
                serial: Some("SN-001".to_string()),
                name: Some("Fridge 1".to_string()),
            },
            NodeEntry {
                deveui: "8899aabbccddeeff".to_string(),
                serial: Some("SN-002".to_string()),
                name: Some("Fridge 2".to_string()),
            },
        ]
    );

    assert!(
        hub.remove(NodeRemoveTarget::Serial("SN-001".to_string()))
            .success
    );
    let after = hub.list();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].deveui, "8899aabbccddeeff");
}

#[test]
fn the_list_answers_on_a_hub_whose_lorawan_monitor_never_started() {
    // This is why `list` reads the file rather than `deps.lorawan_configs`: that mirror is an
    // `Option` and on a Beacon-first Hub it is `None`, but the phone is standing in front of that
    // Hub asking what it holds. "Nothing" would be a lie.
    let hub = seeded();
    let unmirrored = LorawanNodeEnroll::new(
        NodeAddDeps {
            config_applier: hub.deps.config_applier.clone(),
            storage: None,
            lorawan_configs: None,
            lorawan_state: None,
        },
        hub.config_path(),
    );
    assert_eq!(
        unmirrored.list().expect("answerable").len(),
        2,
        "the persisted config is the answer, with or without the in-memory mirrors"
    );
}

#[test]
fn a_config_with_no_lorawan_section_is_an_empty_list_not_a_failure() {
    // The ordinary state of a Beacon-only site. An `Err` here would show the phone an error where
    // the honest answer is "no Nodes".
    let dir = tempfile::tempdir().expect("tempdir");
    for (label, body) in [
        ("no lorawan: at all", "system:\n  device_label: TEST\n"),
        (
            "lorawan: with no sensors:",
            "system:\n  device_label: TEST\nlorawan:\n  enabled: true\n",
        ),
        (
            "an empty sensors:",
            "system:\n  device_label: TEST\nlorawan:\n  sensors:\n",
        ),
        (
            "an explicitly empty sensors:",
            "system:\n  device_label: TEST\nlorawan:\n  sensors: []\n",
        ),
    ] {
        let path = dir.path().join("fiber.config.yaml");
        std::fs::write(&path, body).expect("write");
        let seam = LorawanNodeEnroll::new(NodeAddDeps::default(), &path);
        assert!(
            seam.list().expect(label).is_empty(),
            "{label} must list empty rather than fail"
        );
    }
}

#[test]
fn an_unreadable_config_is_an_error_the_phone_can_tell_from_an_empty_list() {
    // The distinction FB11 turns into an ATT-level error: a Hub that cannot look must not be
    // reported as a Hub that holds nothing.
    let missing = LorawanNodeEnroll::new(
        NodeAddDeps::default(),
        Path::new("/nonexistent/fiber.config.yaml"),
    );
    let e = missing.list().expect_err("cannot read");
    assert!(e.contains("cannot read"), "{e}");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("fiber.config.yaml");
    std::fs::write(&path, "lorawan:\n  sensors:\n    - not: a sensor\n").expect("write");
    let invalid = LorawanNodeEnroll::new(NodeAddDeps::default(), &path);
    let e = invalid.list().expect_err("invalid");
    assert!(e.contains("lorawan.sensors"), "{e}");

    // …and a removal on an unreadable config refuses rather than writing blind.
    assert!(
        !missing
            .remove(&NodeRemoveTarget::DevEui("0011223344556677".to_string()))
            .success
    );
}

#[test]
fn the_listed_deveui_is_lowercase_however_it_was_written_to_the_file() {
    // v1 rows predate `prepare`'s normalisation. The list must not invent a second spelling of the
    // wire identity, because the phone uses it verbatim on the next FB12 write.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("fiber.config.yaml");
    std::fs::write(
        &path,
        "lorawan:\n  sensors:\n    - dev_eui: 00AABBCCDDEEFF11\n      serial_number: \"\"\n",
    )
    .expect("write");
    let seam = LorawanNodeEnroll::new(NodeAddDeps::default(), &path);
    let listed = seam.list().expect("answerable");
    assert_eq!(listed[0].deveui, "00aabbccddeeff11");
    assert_eq!(
        listed[0].serial, None,
        "an empty serial is absent, not an empty string the phone would render as a label"
    );
}
