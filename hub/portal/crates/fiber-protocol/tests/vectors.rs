//! `fiber-protocol`'s half of the one-fixture-three-implementations
//! contract (plan/portal/mqtt/design.md §10).
//!
//! Before v2 the pre-image strings were hand-written in this crate's unit
//! tests only: `control-core` had **no** test asserting its canonical
//! output and the mock re-derived the same shape in Python, so
//! byte-exactness between the implementations was asserted in prose. The
//! checked-in `vectors/` are now the artefact all three read — this file
//! is the first of the three consumers, and the compliance evidence
//! design.md §11 points at.
//!
//! Regenerate with `cargo run -p fiber-protocol --example gen_vectors`.
//! A diff in `vectors/*.json` is a wire-contract change and must be read
//! as one in review.

use std::collections::{BTreeSet, HashSet};

use ed25519_dalek::{SigningKey, VerifyingKey};
use fiber_protocol::certificate::{
    canonical_certificate_message, verify_certificate, Certificate, CertificatePayload,
};
use fiber_protocol::registry::{self, Level};
use fiber_protocol::{SignedConfirm, SignedRequest, ENVELOPE_VERSION};
use serde_json::Value;

fn load(name: &str) -> Value {
    let path = format!("{}/vectors/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parsing {path}: {e}"))
}

fn envelopes() -> Value {
    load("envelope_v2.json")
}

fn verifying_key(vectors: &Value) -> VerifyingKey {
    let hex_key = vectors["signing_key"]["public_key_ed25519"]
        .as_str()
        .expect("public_key_ed25519");
    let bytes: [u8; 32] = hex::decode(hex_key)
        .expect("hex public key")
        .try_into()
        .expect("32 bytes");
    VerifyingKey::from_bytes(&bytes).expect("valid Ed25519 public key")
}

/// The fixture's public key really is the seed's — so a reader can
/// reproduce every signature below from `seed_hex` alone.
#[test]
fn the_fixture_public_key_belongs_to_the_fixture_seed() {
    let vectors = envelopes();
    let seed: [u8; 32] = hex::decode(vectors["signing_key"]["seed_hex"].as_str().unwrap())
        .expect("hex seed")
        .try_into()
        .expect("32 bytes");
    assert_eq!(
        hex::encode(SigningKey::from_bytes(&seed).verifying_key().to_bytes()),
        vectors["signing_key"]["public_key_ed25519"]
            .as_str()
            .unwrap()
    );
    assert_eq!(vectors["envelope_version"], ENVELOPE_VERSION);
}

/// Every request vector: the wire JSON parses, rebuilds the fixture's
/// canonical string byte for byte, and its signature verifies under the
/// fixture key. This is the whole P0 acceptance criterion for the envelope
/// half.
#[test]
fn every_request_vector_canonicalizes_and_verifies() {
    let vectors = envelopes();
    let key = verifying_key(&vectors);
    let cases = vectors["requests"].as_array().expect("requests");
    assert!(cases.len() >= 12, "design.md §10 asks for ~12 cases");

    for case in cases {
        let name = case["name"].as_str().expect("name");
        let request: SignedRequest = serde_json::from_value(case["envelope"].clone())
            .unwrap_or_else(|e| panic!("{name}: {e}"));

        assert_eq!(request.command, "config_request", "{name}");
        assert_eq!(request.v, ENVELOPE_VERSION, "{name}");
        assert_eq!(
            request.canonical_message(),
            case["canonical"].as_str().expect("canonical"),
            "{name}: canonical string drifted from the fixture"
        );
        fiber_protocol::verify_request(&request, &key)
            .unwrap_or_else(|e| panic!("{name}: signature must verify: {e}"));
    }
}

#[test]
fn every_confirm_vector_canonicalizes_and_verifies() {
    let vectors = envelopes();
    let key = verifying_key(&vectors);
    for case in vectors["confirms"].as_array().expect("confirms") {
        let name = case["name"].as_str().expect("name");
        let confirm: SignedConfirm = serde_json::from_value(case["envelope"].clone())
            .unwrap_or_else(|e| panic!("{name}: {e}"));

        assert_eq!(confirm.command, "config_confirm", "{name}");
        assert_eq!(confirm.v, ENVELOPE_VERSION, "{name}");
        assert_eq!(
            confirm.canonical_message(),
            case["canonical"].as_str().expect("canonical"),
            "{name}: canonical string drifted from the fixture"
        );
        fiber_protocol::verify_confirm(&confirm, &key)
            .unwrap_or_else(|e| panic!("{name}: signature must verify: {e}"));
    }
}

/// The nine-key request / seven-key confirm pre-images (design.md §3.2),
/// asserted on the fixture's own strings rather than on a rebuild — so the
/// shape is pinned even if `request_preimage` learns a new field.
#[test]
fn the_fixture_pins_nine_and_seven_key_preimages() {
    let vectors = envelopes();
    for (half, expected_keys) in [
        (
            "requests",
            [
                "command_type",
                "level",
                "nonce",
                "params",
                "reason",
                "request_id",
                "signer_id",
                "timestamp",
                "v",
            ]
            .as_slice(),
        ),
        (
            "confirms",
            [
                "challenge_id",
                "confirmation",
                "level",
                "nonce",
                "signer_id",
                "timestamp",
                "v",
            ]
            .as_slice(),
        ),
    ] {
        for case in vectors[half].as_array().expect(half) {
            let name = case["name"].as_str().expect("name");
            let canonical = case["canonical"].as_str().expect("canonical");
            let parsed: Value = serde_json::from_str(canonical).expect("canonical is JSON");
            let keys: Vec<&str> = parsed
                .as_object()
                .expect("canonical is an object")
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(keys, expected_keys, "{half}/{name}");
            assert!(
                !canonical.contains("\"command\"") && !canonical.contains("\"algorithm\""),
                "{half}/{name}: the unsigned discriminator must never be in the pre-image"
            );
        }
    }
}

/// The six float pins live in the fixture now (design.md §3.2's last
/// bullet). The text this crate signs is the text that goes on the wire —
/// which is the half of the parity chain this crate controls.
#[test]
fn the_six_float_pins_survive_the_fixture() {
    let vectors = envelopes();
    let mut seen = Vec::new();
    for case in vectors["requests"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        if let Some(rest) = name.strip_prefix("float_") {
            let canonical = case["canonical"].as_str().unwrap();
            let on_the_wire = case["envelope"]["params"]["warning_high"].to_string();
            assert!(
                canonical.contains(&format!("\"warning_high\":{on_the_wire}")),
                "{name}: signed text and wire text differ"
            );
            seen.push(rest.to_string());
        }
    }
    assert_eq!(
        seen,
        vec![
            "21_5",
            "0_1",
            "negative_18_2",
            "2_675",
            "1e16",
            "1e_minus_7"
        ],
        "the six pins from envelope.rs's module doc"
    );
}

/// status.md records that Python's `json.dumps` escapes non-ASCII while
/// `serde_json` does not — the one known way the three implementations
/// could disagree. Pinned here rather than assumed: the mock must load this
/// vector and match it (design.md §10, P3).
#[test]
fn the_non_ascii_reason_is_not_escaped() {
    let vectors = envelopes();
    let case = vectors["requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "non_ascii_reason")
        .expect("the non-ASCII case exists");
    let canonical = case["canonical"].as_str().unwrap();
    assert!(
        canonical.contains("čidlo"),
        "non-ASCII rides raw, not \\u-escaped"
    );
    assert!(!canonical.contains("\\u010d"), "no \\u escaping");
    // The one escape that *is* required: a literal quote inside the string.
    assert!(canonical.contains(r#"\"A\""#));
}

// ---------------------------------------------------------------- catalogue

fn catalogue() -> Value {
    load("catalogue.json")
}

/// P0's acceptance criterion, plus the three v2 Node verbs (control.radio.md
/// §7), the two rows #58 and #59 added and `radio-set-mode` (node_registry.md
/// §11, R7) and the two cluster-client verbs (cluster.md §7), less the retired `set_lorawan_cluster`
/// (cluster.md §3), plus `northbridge-update` (northbridge_update.md §3.4) and
/// `southbridge-update` (southbridge_update.md §3.7) and `cluster-client-gateway` (#164): 63 rows, zero
/// `Plain` — there is
/// no such routing any more, so "zero Plain" is checked as "the eight v1
/// flat-dialect verbs are ordinary signed rows with a level and a
/// permission" (design.md §4.2, §7).
#[test]
fn the_catalogue_has_sixty_three_rows_and_no_plain_dialect() {
    let catalogue = catalogue();
    let rows = catalogue["commands"].as_array().expect("commands");
    assert_eq!(
        rows.len(),
        63,
        "+ set_sample_interval (config_sync.md Q19); 44 signed after the rename + 8 ex-PLAIN + node-add/node-remove/node-move + \
         setup-complete (#58) + export-destination-set (#59) + radio-set-mode (R7) + \
         cluster-client-add/remove (cluster.md §7) - set_lorawan_cluster (retired, \
         cluster.md §3) + northbridge-update (northbridge_update.md §3.4) + southbridge-update + \
         cluster-client-gateway (#164)"
    );

    for row in rows {
        let ct = row["command_type"].as_str().expect("command_type");
        assert!(row.get("routing").is_none(), "{ct}: routing is gone");
        assert!(
            row.get("cert_preflight").is_none(),
            "{ct}: preflight is universal"
        );
        assert!(
            !row["permission"].as_str().expect("permission").is_empty(),
            "{ct}: every verb has a permission now"
        );
        assert!(
            Level::parse(row["level"].as_str().expect("level")).is_some(),
            "{ct}"
        );
    }

    for ex_plain in [
        "get_sensor_config",
        "get_interval",
        "silence_buzzer",
        "get_sticker_config",
        "get_sticker_info",
        "get_sticker_full_config",
        "get_sticker_history",
        "sticker_force_send",
    ] {
        let spec = registry::lookup(ex_plain)
            .unwrap_or_else(|| panic!("{ex_plain} must be a signed row now"));
        assert!(
            spec.level.is_single_shot(),
            "{ex_plain}: the eight ex-PLAIN verbs are all read or operate"
        );
    }
}

/// The registry and the fixture are the same table. This is the assertion
/// that makes `catalogue.json` load-bearing rather than documentation: the
/// device's own table and the mock's are checked against the same file
/// (P1, P3).
#[test]
fn the_registry_is_the_catalogue() {
    let catalogue = catalogue();
    let rows = catalogue["commands"].as_array().unwrap();

    let from_file: BTreeSet<(String, String, String, bool)> = rows
        .iter()
        .map(|r| {
            (
                r["command_type"].as_str().unwrap().to_string(),
                r["permission"].as_str().unwrap().to_string(),
                r["level"].as_str().unwrap().to_string(),
                r["single_shot"].as_bool().unwrap(),
            )
        })
        .collect();
    let from_code: BTreeSet<(String, String, String, bool)> = registry::all()
        .iter()
        .map(|s| {
            (
                s.command_type.to_string(),
                s.permission.to_string(),
                s.level.to_string(),
                s.level.is_single_shot(),
            )
        })
        .collect();
    assert_eq!(
        from_file, from_code,
        "registry.rs and vectors/catalogue.json disagree — regenerate with \
         `cargo run -p fiber-protocol --example gen_vectors` and read the diff"
    );
}

/// `single_shot` is exactly `read | operate` (design.md §5) — stated once
/// in the fixture so the device and the mock read it rather than each
/// deriving it.
#[test]
fn single_shot_is_exactly_read_and_operate() {
    for row in catalogue()["commands"].as_array().unwrap() {
        let level = Level::parse(row["level"].as_str().unwrap()).unwrap();
        assert_eq!(
            row["single_shot"].as_bool().unwrap(),
            matches!(level, Level::Read | Level::Operate),
            "{}",
            row["command_type"]
        );
    }
}

/// Per-level counts, spelled out so a row moving between levels is a
/// review conversation and not a silent diff. design.md §4.2 states these
/// including the device-only arms that carry no portal row — the numbers
/// here are the portal rows alone.
#[test]
fn the_per_level_counts_are_pinned() {
    let mut counts = std::collections::BTreeMap::new();
    for spec in registry::all() {
        *counts.entry(spec.level).or_insert(0usize) += 1;
    }
    assert_eq!(
        counts[&Level::Read],
        7,
        "+ device-only get_status/get_info (history_request was deleted, \
         not re-homed — handoff.md §1)"
    );
    assert_eq!(
        counts[&Level::Operate],
        6,
        "+ device-only set_screen/beacon-detect"
    );
    assert_eq!(
        counts[&Level::Manage],
        29,
        "+ set_sample_interval (config_sync.md Q19), + node-add, + setup-complete (GitLab #58), - set_lorawan_cluster (retired, \
         cluster.md §3)"
    );
    assert_eq!(
        counts[&Level::Admin],
        13,
        "+ export-destination-set (GitLab #59), + radio-set-mode (R7), + node-move, \
         + cluster-client-add/remove (cluster.md §7), + northbridge-update, \
         + southbridge-update, + cluster-client-gateway (#164)"
    );
    assert_eq!(
        counts[&Level::Destructive],
        8,
        "+ device-only flush_storage"
    );
}

/// None of the v1 `*_eye_*` wire names exists in v2
/// (plan/control/ble/plan.md §9.8's rule), with §9.8's own two named
/// exceptions — the threshold pair, which is fleet config and renames with
/// the rest of the threshold family in plan/portal/control.md.
#[test]
fn the_eye_vocabulary_is_gone_except_the_two_named_thresholds() {
    let leftovers: Vec<&str> = registry::all()
        .iter()
        .map(|s| s.command_type)
        .filter(|ct| ct.contains("eye"))
        .collect();
    assert_eq!(
        leftovers,
        vec!["set_eye_field_threshold", "delete_eye_field_threshold"]
    );

    for withdrawn in [
        "add_eye_tag",
        "remove_eye_tag",
        "detect_eye_tag",
        "set_eye_config",
    ] {
        assert!(
            registry::lookup(withdrawn).is_none(),
            "{withdrawn} must be gone"
        );
    }
    for beacon in [
        "beacon-add",
        "beacon-remove",
        "beacon-history",
        "beacon-provision",
        "beacon-monitoring",
        "beacon-recording",
        "beacon-send",
        "known-beacons",
    ] {
        let spec = registry::lookup(beacon).unwrap_or_else(|| panic!("{beacon} must exist"));
        assert_eq!(spec.permission, "beacon.manage", "{beacon}");
    }
}

#[test]
fn no_duplicate_command_types_and_no_fallback() {
    let mut seen = HashSet::new();
    for spec in registry::all() {
        assert!(
            seen.insert(spec.command_type),
            "duplicate: {}",
            spec.command_type
        );
    }
    assert!(registry::lookup("this_command_type_does_not_exist").is_none());
    assert!(registry::lookup("").is_none());
}

/// The five `level:*` pseudo-permissions (design.md §4.3) are ordinary
/// strings in the certificate's `permissions` list — the mechanism that
/// makes the level enforceable without touching the certificate schema.
#[test]
fn the_level_pseudo_permissions_are_spelled_out_in_the_fixture() {
    let catalogue = catalogue();
    let map = catalogue["level_permissions"]
        .as_object()
        .expect("level_permissions");
    assert_eq!(map.len(), 5);
    for level in Level::ALL {
        assert_eq!(
            map[level.as_str()].as_str().unwrap(),
            format!("level:{level}"),
            "{level}"
        );
        assert_eq!(level.permission(), format!("level:{level}"));
    }
}

/// The ladder: a role's ceiling is one comparison (design.md §4.3).
#[test]
fn the_levels_are_ordered_as_a_ladder() {
    assert!(Level::Read < Level::Operate);
    assert!(Level::Operate < Level::Manage);
    assert!(Level::Manage < Level::Admin);
    assert!(Level::Admin < Level::Destructive);
    assert_eq!(Level::ALL.len(), 5);
    for level in Level::ALL {
        assert_eq!(Level::parse(level.as_str()), Some(level));
    }
    assert_eq!(
        Level::parse("Manage"),
        None,
        "the wire spelling is lowercase"
    );
    assert_eq!(Level::parse("root"), None);
}

// -------------------------------------------------------------- certificate

/// design.md §3.4: the certificate does not change. The fixture exists so
/// the review can see that — the eight-field canonical form and its
/// signature still verify.
#[test]
fn the_certificate_did_not_move() {
    let vectors = load("certificate.json");
    let payload: CertificatePayload =
        serde_json::from_value(vectors["payload"].clone()).expect("payload");
    assert_eq!(
        canonical_certificate_message(&payload),
        vectors["canonical"].as_str().expect("canonical")
    );

    let certificate = Certificate {
        payload,
        certificate_signature: vectors["certificate_signature"]
            .as_str()
            .unwrap()
            .to_string(),
    };
    verify_certificate(
        vectors["ca_public_key_ed25519"].as_str().unwrap(),
        &certificate,
    )
    .expect("the checked-in certificate must still verify");

    let parsed: Value = serde_json::from_str(vectors["canonical"].as_str().unwrap()).unwrap();
    assert_eq!(parsed.as_object().unwrap().len(), 8, "eight fields, still");
}
