//! The exit-code table and the structured-detail schema — `plan/control/radio/plan.md` §6.
//!
//! These are a **consumed contract**, not cosmetics: the provisioning wizard and the fleet
//! updater read them over SSH. The table is additive-only — a new variant gets a new number,
//! existing numbers never change meaning — so this file is a golden baseline. Changing a row
//! here should be as hard as changing a wire format.

use control_radio::{Component, Error, ErrorDetail, Kind, Transport};

/// §6's assigned table, row for row.
#[test]
fn exit_codes_are_the_assigned_table() {
    let expected: &[(Kind, i32)] = &[
        (Kind::InvalidArgument, 2),
        (Kind::NotBootstrapped, 10),
        (Kind::Unauthenticated, 11),
        (Kind::PermissionDenied, 12),
        (Kind::NotFound, 13),
        (Kind::AlreadyExists, 14),
        (Kind::AddressCollision, 16),
        (Kind::ServiceDown, 20),
        (Kind::Timeout, 21),
        (Kind::Transport, 22),
        (Kind::DaemonDown, 23),
        (Kind::BridgeDisabled, 30),
        (Kind::LorawanUnavailable, 31),
        (Kind::P2pUnavailable, 32),
        (Kind::RadioParameterRequired, 33),
        (Kind::Phase2NotImplemented, 34),
        (Kind::OffMode, 35),
        (Kind::StorageError, 40),
    ];
    assert_eq!(
        expected.len(),
        Kind::ALL.len(),
        "every variant must have a row"
    );
    for &(kind, code) in expected {
        assert_eq!(kind.exit_code(), code, "{kind} moved");
    }
}

/// Codes are unique — two variants sharing a number would make the contract unbranchable.
#[test]
fn exit_codes_are_unique() {
    let mut seen = std::collections::BTreeMap::new();
    for kind in Kind::ALL {
        if let Some(other) = seen.insert(kind.exit_code(), kind) {
            panic!(
                "{kind} and {other} both claim exit code {}",
                kind.exit_code()
            );
        }
    }
}

/// The banding is what lets scripts branch on ranges, and what `radio-setup`'s boot retry
/// policy keys on: 10-19 fail fast, 20-29 retry with backoff, 30-39 fail fast, 40+ storage.
#[test]
fn only_the_transient_band_is_retryable() {
    for kind in Kind::ALL {
        let code = kind.exit_code();
        let banded_transient = (20..=29).contains(&code);
        assert_eq!(
            kind.is_retryable(),
            banded_transient,
            "{kind} (exit {code}) disagrees with its band"
        );
    }
    // The specific policy §6 states: retry ServiceDown/Timeout, fail fast on the
    // bootstrap/auth trio.
    assert!(Kind::ServiceDown.is_retryable());
    assert!(Kind::Timeout.is_retryable());
    assert!(!Kind::NotBootstrapped.is_retryable());
    assert!(!Kind::Unauthenticated.is_retryable());
    assert!(!Kind::PermissionDenied.is_retryable());
}

/// The `error` string is the consumed contract, and it is *not* the Rust identifier: the plan
/// spells two variants with LoRaWAN's and P2P's own casing.
#[test]
fn wire_names_keep_the_plans_casing() {
    assert_eq!(Kind::LorawanUnavailable.as_str(), "LoRaWANUnavailable");
    assert_eq!(Kind::P2pUnavailable.as_str(), "P2PUnavailable");
    assert_eq!(Kind::StorageError.as_str(), "StorageError");
    assert_eq!(Kind::NotBootstrapped.as_str(), "NotBootstrapped");
}

/// §6: every error carries the full `{error, transport, component, message}` detail, and
/// `component` is drawn from the closed vocabulary.
#[test]
fn the_structured_detail_serializes_to_the_documented_shape() {
    let err = Error::new(
        Kind::ServiceDown,
        Transport::P2p,
        Component::NorthbridgeUart,
        "no response on /dev/ttyAMA3 after 3 retries",
    );
    let json = serde_json::to_value(err.detail()).expect("detail serializes");
    assert_eq!(json["error"], "ServiceDown");
    assert_eq!(json["transport"], "p2p");
    assert_eq!(json["component"], "northbridge-uart");
    assert_eq!(
        json["message"],
        "no response on /dev/ttyAMA3 after 3 retries"
    );
    assert_eq!(err.exit_code(), 20);

    // Round-trips, because portal and the control socket both parse it back.
    let back: ErrorDetail = serde_json::from_value(json).expect("detail round-trips");
    assert_eq!(&back, err.detail());
}

/// A pre-dispatch failure has no arm to name; `transport` is then absent rather than a null,
/// so consumers can distinguish "no arm involved" from "arm unknown".
#[test]
fn transport_is_omitted_when_no_arm_produced_the_error() {
    let err = Error::new(
        Kind::RadioParameterRequired,
        None,
        Component::ControlSocket,
        "--radio is required while radio.mode is dual",
    );
    let json = serde_json::to_value(err.detail()).expect("serializes");
    assert!(json.get("transport").is_none(), "absent, not null: {json}");
}

/// The `component` vocabulary is closed (decision #6 — declared surface). This pins both the
/// membership and the wire spellings.
#[test]
fn the_component_vocabulary_is_the_documented_closed_set() {
    let expected = [
        "chirpstack-api",
        "chirpstack-service",
        "token-file",
        "config-json",
        "enrollment-record",
        "mqtt-broker",
        "northbridge-uart",
        "northbridge-firmware",
        "session-db",
        "node-registry",
        "control-socket",
        "radio-mode",
    ];
    let actual: Vec<&str> = Component::ALL.iter().map(|c| c.as_str()).collect();
    assert_eq!(actual, expected);

    // Serde agrees with as_str(), so the JSON surface cannot drift from the Rust one.
    for c in Component::ALL {
        let json = serde_json::to_value(c).expect("component serializes");
        assert_eq!(json, c.as_str());
    }
}

/// A free-form component is not representable — that is the point of the enum.
#[test]
fn an_unknown_component_string_does_not_deserialize() {
    let raw = serde_json::json!({
        "error": "ServiceDown",
        "component": "something-invented",
        "message": "x"
    });
    assert!(serde_json::from_value::<ErrorDetail>(raw).is_err());
}

/// The retained-topic half of `node-add`/`node-remove` (§9.4) fails as a **broker** failure,
/// and that has to stay in the retryable band.
///
/// `node-remove` deletes in ChirpStack and then publishes the tombstone; if the broker refuses,
/// the device is gone and the topic is not, and the remedy is to run the command again. So the
/// two variants that path can produce must both be 20-29 — the band `radio-setup`'s boot retry
/// policy keys on — and must name `mqtt-broker`, or an operator reading the envelope cannot
/// tell a broker outage from a ChirpStack one.
#[test]
fn a_broker_failure_after_a_chirpstack_write_is_retryable_and_names_the_broker() {
    for kind in [Kind::ServiceDown, Kind::Timeout] {
        let e = Error::new(
            kind,
            Transport::Lorawan,
            Component::MqttBroker,
            "the device IS deleted in ChirpStack, but the retained topic remains; re-run \
             node-remove to clear it",
        );
        assert!(
            e.kind().is_retryable(),
            "{kind} must stay in the retry band: a re-run is the documented remedy"
        );
        let json = serde_json::to_value(e.detail()).expect("serializes");
        assert_eq!(json["component"], "mqtt-broker");
        assert_eq!(json["transport"], "lorawan");
    }
    // And they stay distinguishable: "the broker is not there" and "the broker did not
    // acknowledge" send an operator to different places.
    assert_ne!(Kind::ServiceDown.exit_code(), Kind::Timeout.exit_code());
}

/// §6, added 2026-08-21 (MR !14 finding 2.5, overruled): `radio.mode: off` is its own variant,
/// not one of the `*Unavailable` pair.
///
/// The `*Unavailable` rows are defined as "a LoRaWAN command while `radio.mode: p2p`" and its
/// mirror. `off` is neither, and it has a different remediation — the Hub was built with no
/// radio arm enabled, rather than the caller asking for the wrong one.
#[test]
fn off_mode_is_its_own_variant_and_carries_the_asked_for_transport() {
    assert_eq!(Kind::OffMode.exit_code(), 35);
    assert_eq!(Kind::OffMode.as_str(), "OffMode");
    assert!(
        !Kind::OffMode.is_retryable(),
        "a mode refusal is fail-fast: retrying cannot help"
    );

    let e = Error::new(
        Kind::OffMode,
        Transport::P2p,
        Component::RadioMode,
        "radio.mode is off: no radio arm is enabled on this Hub",
    );
    let json = serde_json::to_value(e.detail()).expect("serializes");
    assert_eq!(json["error"], "OffMode");
    assert_eq!(
        json["transport"], "p2p",
        "the asked-for transport is carried"
    );
    assert_eq!(json["component"], "radio-mode");
}

/// `AddressCollision` (2026-09-28, `p2p_tower_gateway.md` §3.1): a config fact, so fail-fast in
/// the 10–19 band, on 16 because 15 is control.ble's `PairingRejected` in the shared table.
#[test]
fn address_collision_is_a_fail_fast_registry_refusal() {
    assert_eq!(Kind::AddressCollision.exit_code(), 16);
    assert_eq!(Kind::AddressCollision.as_str(), "AddressCollision");
    assert!(!Kind::AddressCollision.is_retryable());
    let e = Error::new(
        Kind::AddressCollision,
        Transport::P2p,
        Component::NodeRegistry,
        "5876070000000413 shares P2P address 00000413 with 70b3d57e00000413",
    );
    let json = serde_json::to_value(e.detail()).expect("serializes");
    assert_eq!(json["error"], "AddressCollision");
    assert_eq!(json["component"], "node-registry");
}
