//! The shipped `fiber.config.yaml` must parse under the schemas actually in force.
//!
//! This is the test that would have caught the B4 hazard directly: `control_ble::config::BleConfig`
//! is `deny_unknown_fields`, so a `ble:` block carrying a leftover v1 key does not degrade — the
//! Bluetooth component refuses to start. Shipping such a file would produce a Hub that boots green,
//! measures temperature, and cannot be reached by a phone, with the reason only in the journal.

use fiber_app::libs::config::Config;

const SHIPPED: &str = include_str!("../fiber.config.yaml");

#[test]
fn the_shipped_ble_section_parses_under_the_crates_strict_schema() {
    let cfg: Config = serde_yaml::from_str(SHIPPED).expect("the shipped config parses");
    let section = cfg.ble_section_yaml().expect("ble: section");
    let ble = control_ble::config::BleConfig::from_device_config_yaml(&section)
        .expect("the shipped ble: section is valid");

    assert_eq!(
        ble.mode,
        control_ble::mode::Mode::Dual,
        "the shipped default"
    );
    assert!(ble.adapter.is_none(), "one adapter, chosen by BlueZ");
    assert_eq!(ble.central.discovered_max, 64);
    assert_eq!(ble.peripheral.session_idle_timeout_s, 300);
}

#[test]
fn the_shipped_config_carries_no_v1_ble_keys() {
    // `pin` and `enable_terminal` are the two that matter: one was a stored shared secret, the
    // other a switch for a feature CP-1 deleted. Both would now be a start-up failure, but the
    // point of asserting on the text is that they must not come back in a merge.
    let cfg: Config = serde_yaml::from_str(SHIPPED).expect("parses");
    // Asserted on the key set rather than on the text: `recovery.enabled` is a legitimate nested
    // key, and a substring search for "enabled:" would flag it.
    let keys: Vec<String> = cfg
        .ble
        .as_mapping()
        .expect("the ble: section is a mapping")
        .keys()
        .filter_map(|k| k.as_str().map(str::to_string))
        .collect();
    assert_eq!(
        keys,
        vec!["mode", "adapter", "central", "peripheral", "bluez"],
        "the shipped ble: section has drifted from the design's six keys"
    );
}

#[test]
fn the_shipped_eye_section_parses_and_carries_the_en12830_flag() {
    let cfg: Config = serde_yaml::from_str(SHIPPED).expect("parses");
    let text = serde_yaml::to_string(&serde_yaml::Value::Mapping(
        [(serde_yaml::Value::String("eye".into()), cfg.beacon.clone())]
            .into_iter()
            .collect(),
    ))
    .expect("re-serialise");
    let eye = fiber_app::libs::ble_host::config_store::section_from_yaml(&text)
        .expect("the shipped eye: section is valid");

    assert!(eye.enabled);
    assert!(eye.auto_provision);
    assert!(eye.en12830.enabled, "the archive path ships on");
    assert_eq!(eye.en12830.protocol_failure_alarm_after, 3);
    assert!(
        !eye.legacy.contains_key("publish_interval_s"),
        "the shipped file drops the legacy transport keys: {:?}",
        eye.legacy.keys().collect::<Vec<_>>()
    );
    assert!(!eye.legacy.contains_key("tag_timeout_s"));
}

#[test]
fn a_deployed_config_with_a_v1_ble_key_fails_loudly_and_names_the_key() {
    // The migration hazard. A Hub updated in the field still has v1's `ble:` block in /data, and
    // the failure has to name what is wrong — "invalid config" would send an engineer reading the
    // whole file.
    let err = control_ble::config::BleConfig::from_device_config_yaml(
        "ble:\n  mode: dual\n  enable_terminal: true\n",
    )
    .expect_err("refused");
    assert!(
        err.message().contains("enable_terminal"),
        "the failure must name the key: {}",
        err.message()
    );

    let err = control_ble::config::BleConfig::from_device_config_yaml(
        "ble:\n  mode: dual\n  pin: \"123456\"\n",
    )
    .expect_err("refused");
    assert!(err.message().contains("pin"), "{}", err.message());
}

#[test]
fn a_deployed_config_with_legacy_eye_transport_keys_is_tolerated() {
    // The other half of the same migration: `eye:` keeps working. The crate accepts and ignores the
    // two transport keys rather than refusing, because a Hub that will not monitor Beacons after an
    // update is a worse outcome than one that ignores two stale numbers.
    let eye = fiber_app::libs::ble_host::config_store::section_from_yaml(
        "eye:\n  enabled: true\n  publish_interval_s: 30\n  tag_timeout_s: 600\n",
    )
    .expect("tolerated");
    assert!(eye.enabled);
    assert!(eye.legacy.contains_key("publish_interval_s"));
    assert!(eye.legacy.contains_key("tag_timeout_s"));
}
