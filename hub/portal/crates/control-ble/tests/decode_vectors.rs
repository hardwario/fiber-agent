//! The BTSMP1 advertisement parser against `plan/control/ble/beacon-btsmp1.md` §1's vector table.
//!
//! §1 is the one **publicly specified** layer of the beacon contract, with published captures, so
//! these are real vectors rather than a restatement of the implementation. The fixture is the doc's
//! table; if the two drift, this fails.

use control_ble::decode::{registry, DECODE_FIELDS};

const FIXTURE: &str = include_str!("fixtures/btsmp1-vectors.json");

fn bytes_of(spec: &str) -> Vec<u8> {
    spec.split_whitespace()
        .map(|b| u8::from_str_radix(b, 16).expect("hex byte"))
        .collect()
}

#[test]
fn every_documented_vector_decodes_exactly_as_documented() {
    let doc: serde_json::Value = serde_json::from_str(FIXTURE).expect("fixture parses");
    let vectors = doc["vectors"].as_array().expect("vectors array");
    assert_eq!(vectors.len(), 9, "all nine documented rows must be present");

    let parse = registry::lookup("btsmp1").expect("the btsmp1 parser is registered");

    for v in vectors {
        let spec = v["bytes"].as_str().expect("bytes");
        let why = v["why"].as_str().unwrap_or("");
        let raw = bytes_of(spec);

        if let Some(expected_err) = v.get("error").and_then(|e| e.as_str()) {
            let err = parse(&raw).expect_err(&format!("{spec} must be rejected ({why})"));
            assert_eq!(err.as_str(), expected_err, "{spec}: {why}");
            continue;
        }

        let reading = parse(&raw).unwrap_or_else(|e| panic!("{spec} must decode ({why}): {e:?}"));
        let got = serde_json::to_value(reading).expect("serialize");
        let expect = v["expect"].as_object().expect("expect object");

        // Every field the doc states, exactly.
        for (key, want) in expect {
            assert_eq!(&got[key], want, "{spec} field {key} ({why})");
        }
        // And every field it does NOT state must be absent — null for the optional ones, false for
        // the flags. Absent must never surface as zero: a beacon with no humidity sensor would
        // otherwise render as 0 % on the portal card (UI-10).
        for field in DECODE_FIELDS {
            if expect.contains_key(field) {
                continue;
            }
            let actual = &got[field];
            assert!(
                actual.is_null() || actual == &serde_json::Value::Bool(false),
                "{spec}: {field} is not stated by the doc, so it must be absent \
                 (null) or false — got {actual} ({why})"
            );
        }
    }
}

#[test]
fn the_registry_has_exactly_the_documented_parsers() {
    let names: Vec<&str> = registry::PARSERS.iter().map(|(n, _)| *n).collect();
    assert_eq!(names, vec!["btsmp1"]);
    assert!(registry::lookup("btsmp1").is_some());
    assert!(
        registry::lookup("some-other-variant").is_none(),
        "the registry is a lookup, not a fallback — variant proliferation is the normal case \
         (beacon-btsmp1.md §4) and an unknown name must not silently decode as BTSMP1"
    );
}

#[test]
fn an_absent_field_is_none_not_zero() {
    let parse = registry::lookup("btsmp1").expect("registered");
    // `01 40` — the low-battery flag alone, no value bytes at all.
    let r = parse(&[0x01, 0x40]).expect("decodes");
    assert!(r.low_battery, "the flag itself is set");
    assert_eq!(r.temperature_c, None);
    assert_eq!(r.humidity_pct, None);
    assert_eq!(r.battery_mv, None, "the low-battery FLAG is not a voltage");
    assert_eq!(r.movement_count, None);
    assert_eq!(r.moving, None);
    assert_eq!(r.pitch_deg, None);
    assert_eq!(r.roll_deg, None);
    assert!(!r.magnet_present);
    assert!(!r.magnet_detected);
}

#[test]
fn the_wiki_worked_example_decodes_field_for_field() {
    let parse = registry::lookup("btsmp1").expect("registered");
    let raw = bytes_of("01 B7 08 B4 12 0C CB 0B FF C7 67");
    let r = parse(&raw).expect("decodes");
    assert_eq!(r.protocol_version, 0x01);
    assert_eq!(r.flags, 0xB7);
    assert_eq!(r.temperature_c, Some(22.28));
    assert_eq!(r.humidity_pct, Some(18));
    assert!(r.magnet_present);
    assert!(!r.magnet_detected);
    assert_eq!(r.moving, Some(false));
    assert_eq!(r.movement_count, Some(3275));
    assert_eq!(r.pitch_deg, Some(11));
    assert_eq!(r.roll_deg, Some(-57), "roll is a SIGNED big-endian i16");
    assert_eq!(r.battery_mv, Some(3030));
    assert!(!r.low_battery);
}

#[test]
fn field_offsets_follow_the_flag_bits_and_not_a_fixed_layout() {
    // The same value byte means different things depending on which flags precede it — which is
    // the whole reason §1 says a parser must walk the flags rather than seek to an offset.
    let parse = registry::lookup("btsmp1").expect("registered");

    // 0x02 = humidity only: the byte after flags is humidity.
    let r = parse(&[0x01, 0x02, 0x2A]).expect("humidity only");
    assert_eq!(r.humidity_pct, Some(42));
    assert_eq!(r.temperature_c, None);

    // 0x80 = battery only: the SAME byte position is now a battery code.
    let r = parse(&[0x01, 0x80, 0x2A]).expect("battery only");
    assert_eq!(r.battery_mv, Some(2000 + 42 * 10));
    assert_eq!(r.humidity_pct, None);
}

#[test]
fn a_truncated_frame_is_rejected_whole_and_never_partially_decoded() {
    let parse = registry::lookup("btsmp1").expect("registered");
    // Flags claim temperature + humidity + battery, but only the temperature is carried.
    let err = parse(&bytes_of("01 83 09 AB")).expect_err("must reject");
    assert_eq!(err.as_str(), "truncated");
    // Orientation needs three bytes; two is not "pitch plus a default roll".
    let err = parse(&bytes_of("01 20 0B FF")).expect_err("must reject");
    assert_eq!(err.as_str(), "truncated");
}

#[test]
fn magnet_detected_is_only_meaningful_when_the_sensor_is_present() {
    let parse = registry::lookup("btsmp1").expect("registered");
    // bit 3 set without bit 2: the device is claiming a reading from a sensor it does not report
    // having. The doc says bit 3 is "only meaningful when bit 2 is also set", so it does not stand
    // alone.
    let r = parse(&[0x01, 0x08]).expect("decodes");
    assert!(!r.magnet_present);
    assert!(!r.magnet_detected);
    // Both set: detected.
    let r = parse(&[0x01, 0x0C]).expect("decodes");
    assert!(r.magnet_present);
    assert!(r.magnet_detected);
}

#[test]
fn a_trailing_byte_the_flags_do_not_claim_is_ignored_not_an_error() {
    // Frames are variable length and the flags are authoritative about what is carried; a longer
    // frame is a firmware that added something we do not read, not a malformed one.
    let parse = registry::lookup("btsmp1").expect("registered");
    let r = parse(&bytes_of("01 01 09 AB FF FF")).expect("decodes the claimed fields");
    assert_eq!(r.temperature_c, Some(24.75));
}
