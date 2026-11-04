//! The exit-code table and the structured-detail schema — `plan/control/ble/plan.md` §6, one table
//! shared with `control/radio`. Additive-only; changing a row is a wire-format change.
use control_ble::{Component, Error, ErrorDetail, Kind, Role};

#[test]
fn exit_codes_are_the_assigned_table() {
    let expected: &[(Kind, i32, &str)] = &[
        (Kind::InvalidArgument, 2, "InvalidArgument"),
        (Kind::NotBootstrapped, 10, "NotBootstrapped"),
        (Kind::NotFound, 13, "NotFound"),
        (Kind::AlreadyExists, 14, "AlreadyExists"),
        (Kind::PairingRejected, 15, "PairingRejected"),
        (Kind::ServiceDown, 20, "ServiceDown"),
        (Kind::Timeout, 21, "Timeout"),
        (Kind::Transport, 22, "Transport"),
        (Kind::DaemonDown, 23, "DaemonDown"),
        (Kind::AdapterDown, 24, "AdapterDown"),
        (Kind::BeaconUnreachable, 25, "BeaconUnreachable"),
        (Kind::Phase2NotImplemented, 34, "Phase2NotImplemented"),
        (Kind::OffMode, 35, "OffMode"),
        (Kind::CentralUnavailable, 36, "CentralUnavailable"),
        (Kind::PeripheralUnavailable, 37, "PeripheralUnavailable"),
        (Kind::RoleParameterRequired, 38, "RoleParameterRequired"),
        (Kind::CapabilityDisabled, 39, "CapabilityDisabled"),
        (Kind::StorageError, 40, "StorageError"),
    ];
    assert_eq!(
        expected.len(),
        Kind::ALL.len(),
        "every variant must have a row"
    );
    for &(kind, code, name) in expected {
        assert_eq!(kind.exit_code(), code, "{name} moved");
        assert_eq!(kind.as_str(), name);
    }
}

#[test]
fn exit_codes_are_unique_within_the_crate() {
    let mut seen = std::collections::BTreeMap::new();
    for kind in Kind::ALL {
        if let Some(other) = seen.insert(kind.exit_code(), kind) {
            panic!(
                "{} and {} both claim exit code {}",
                kind.as_str(),
                other.as_str(),
                kind.exit_code()
            );
        }
    }
}

#[test]
fn only_the_transient_band_is_retryable() {
    for kind in Kind::ALL {
        assert_eq!(
            kind.is_retryable(),
            (20..=29).contains(&kind.exit_code()),
            "{}",
            kind.as_str()
        );
    }
}

#[test]
fn the_structured_detail_serializes_to_the_documented_shape() {
    let err = Error::new(
        Kind::BeaconUnreachable,
        Role::Central,
        Component::BeaconGatt,
        "DC:A6:32:AA:BB:CC did not answer within 30 s (attempt 3 of 5)",
    );
    let json = serde_json::to_value(err.detail()).unwrap();
    assert_eq!(json["error"], "BeaconUnreachable");
    assert_eq!(json["role"], "central");
    assert_eq!(json["component"], "beacon-gatt");
    assert_eq!(err.exit_code(), 25);
    let back: ErrorDetail = serde_json::from_value(json).unwrap();
    assert_eq!(&back, err.detail());
}

#[test]
fn role_is_omitted_when_no_role_produced_the_error() {
    let err = Error::new(
        Kind::RoleParameterRequired,
        None,
        Component::BleMode,
        "--role is required while ble.mode is dual",
    );
    let json = serde_json::to_value(err.detail()).unwrap();
    assert!(json.get("role").is_none(), "absent, not null: {json}");
}

#[test]
fn the_component_vocabulary_is_the_documented_closed_set() {
    let expected = [
        "bluetooth-adapter",
        "bluez-dbus",
        "gatt-server",
        "phone-session",
        "beacon-gatt",
        "beacon-registry",
        "l2cap",
        "advertising",
        "ble-mode",
        "mqtt-broker",
        "control-socket",
        "device-config",
    ];
    let actual: Vec<&str> = Component::ALL.iter().map(|c| c.as_str()).collect();
    assert_eq!(actual, expected);
    for c in Component::ALL {
        assert_eq!(serde_json::to_value(c).unwrap(), c.as_str());
    }
    let raw =
        serde_json::json!({"error": "Timeout", "component": "something-invented", "message": "x"});
    assert!(serde_json::from_value::<ErrorDetail>(raw).is_err());
}

#[test]
fn off_mode_and_the_four_role_refusals_report_ble_mode() {
    for (kind, code) in [
        (Kind::OffMode, 35),
        (Kind::CentralUnavailable, 36),
        (Kind::PeripheralUnavailable, 37),
        (Kind::RoleParameterRequired, 38),
    ] {
        assert_eq!(kind.exit_code(), code);
        assert!(!kind.is_retryable());
    }
    let e = Error::new(
        Kind::OffMode,
        Role::Central,
        Component::BleMode,
        "ble.mode is off: no BLE role is enabled on this Hub",
    );
    let json = serde_json::to_value(e.detail()).unwrap();
    assert_eq!(json["error"], "OffMode");
    assert_eq!(json["role"], "central");
    assert_eq!(json["component"], "ble-mode");
}
