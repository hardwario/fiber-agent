//! The `ble:` section and the `eye:` fleet subset — `plan/control/ble/plan.md` §4. The fixture is
//! the plan's own YAML block, extracted verbatim: if the doc and the schema drift, this fails.
use control_ble::config::{BleConfig, EyeSection, Sensor};
use control_ble::{Kind, Mode};
const PLAN_EXAMPLE: &str = include_str!("fixtures/ble-section.yaml");

#[test]
fn the_plans_example_block_parses_and_matches_its_stated_defaults() {
    let c = BleConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example parses");
    assert_eq!(c.mode, Mode::Dual);
    assert_eq!(c.adapter, None);
    assert_eq!(c.central.scan_interval_s, 30);
    assert_eq!(c.central.beacon_timeout_s, 600);
    assert_eq!(c.central.discovered_max, 64);
    assert_eq!(c.central.beacon_profile.advertising_interval_s, 10);
    assert_eq!(
        c.central.beacon_profile.active_sensors,
        vec![Sensor::Temperature, Sensor::Humidity]
    );
    assert_eq!(c.central.beacon_profile.tx_power_dbm, 8);
    assert_eq!(c.central.dial_budget.per_beacon_attempts, 5);
    assert_eq!(c.central.recovery.cooldown_secs, 120);
    assert_eq!(c.peripheral.session_idle_timeout_s, 300);
    assert_eq!(c.peripheral.max_page_bytes, 180);
    assert!(c.bluez.is_empty());
}

#[test]
fn a_missing_ble_section_behaves_like_the_defaults_except_mode_which_is_required() {
    // mode is a factory choice with no safe default: absent → NotBootstrapped, not silently `off`.
    let err = BleConfig::from_device_config_yaml("power: {}\n").unwrap_err();
    assert_eq!(err.kind(), Kind::NotBootstrapped);
    let c = BleConfig::from_device_config_yaml("ble:\n  mode: central\n").unwrap();
    assert_eq!(c.central.discovered_max, 64);
}

#[test]
fn unknown_keys_and_a_non_empty_bluez_block_are_parse_errors() {
    assert_eq!(
        BleConfig::from_device_config_yaml("ble:\n  mode: dual\n  pin: '123456'\n")
            .unwrap_err()
            .kind(),
        Kind::InvalidArgument
    );
    assert_eq!(
        BleConfig::from_device_config_yaml("ble:\n  mode: dual\n  bluez: {x: 1}\n")
            .unwrap_err()
            .kind(),
        Kind::InvalidArgument
    );
    assert_eq!(
        BleConfig::from_device_config_yaml("ble:\n  mode: dual\n  central: {adapter: hci1}\n")
            .unwrap_err()
            .kind(),
        Kind::InvalidArgument,
        "no per-role adapter key can exist"
    );
}

#[test]
fn profile_bounds_are_validated() {
    for bad in [
        "advertising_interval_s: 0",
        "advertising_interval_s: 11",
        "tx_power_dbm: 3",
        "tx_power_dbm: 9",
    ] {
        let y = format!("ble:\n  mode: dual\n  central:\n    beacon_profile:\n      {bad}\n");
        assert_eq!(
            BleConfig::from_device_config_yaml(&y).unwrap_err().kind(),
            Kind::InvalidArgument,
            "{bad}"
        );
    }
}

#[test]
fn the_eye_section_accepts_legacy_transport_keys_and_reports_them() {
    let y = "eye:\n  enabled: true\n  publish_interval_s: 30\n  tag_timeout_s: 600\n  auto_provision: true\n  tags:\n    - mac: \"7C:D9:F4:13:10:DE\"\n      name: \"EYE tag 1\"\n      enabled: true\n";
    let e = EyeSection::from_fleet_yaml(y).unwrap();
    assert!(e.enabled && e.auto_provision);
    assert_eq!(e.tags[0].mac, "7C:D9:F4:13:10:DE");
    assert!(e.en12830.enabled, "ships on");
    assert_eq!(e.en12830.protocol_failure_alarm_after, 3);
    let mut ignored: Vec<&str> = e.legacy.keys().map(String::as_str).collect();
    ignored.sort();
    assert_eq!(ignored, vec!["publish_interval_s", "tag_timeout_s"]);
    assert!(
        EyeSection::from_fleet_yaml("power: {}\n")
            .unwrap()
            .tags
            .is_empty(),
        "absent eye: is defaults"
    );
}

#[test]
fn the_sensor_bitmask_is_the_documented_one() {
    // beacon-btsmp1.md §2: temperature 0x01, humidity 0x02, magnet 0x04, movement 0x08.
    assert_eq!(Sensor::Temperature.bit(), 0x01);
    assert_eq!(Sensor::Humidity.bit(), 0x02);
    assert_eq!(Sensor::Magnet.bit(), 0x04);
    assert_eq!(Sensor::Movement.bit(), 0x08);
    // The fleet default, temperature + humidity, is 0x03 on the wire.
    let mask = [Sensor::Temperature, Sensor::Humidity]
        .iter()
        .fold(0u8, |m, s| m | s.bit());
    assert_eq!(mask, 0x03);
}

#[test]
fn the_remaining_validate_bounds_are_enforced() {
    let cases = [
        ("central:\n    discovered_max: 0\n", "discovered_max"),
        (
            "central:\n    dial_budget:\n      concurrent_dials: 2\n",
            "concurrent_dials",
        ),
        ("peripheral:\n    max_page_bytes: 39\n", "max_page_bytes"),
        ("peripheral:\n    max_page_bytes: 513\n", "max_page_bytes"),
    ];
    for (frag, key) in cases {
        let y = format!("ble:\n  mode: dual\n  {frag}");
        let err = BleConfig::from_device_config_yaml(&y).unwrap_err();
        assert_eq!(err.kind(), Kind::InvalidArgument, "{frag}");
        assert!(
            err.message().contains(key),
            "message must name the key {key}: {}",
            err.message()
        );
    }
}
