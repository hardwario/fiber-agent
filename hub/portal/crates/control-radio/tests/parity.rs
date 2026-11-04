//! The ttn.js parity acceptance gate — `plan/control/radio/plan.md` §4.
//!
//! > **Parity acceptance**: the Rust decoder's output must be field-for-field identical to the
//! > firmware's own reference decoder (`app/decoder/ttn.js@v1.4.0`) on identical bytes; its
//! > `ttn.test.js` vectors are the CI fixture.
//!
//! The fixture was not hand-transcribed. `app/decoder/ttn.js` and `ttn.test.js` were fetched
//! at tag `v1.4.0`, the codec was wrapped to record every `(bytes, fPort) -> output` pair the
//! suite exercises — which captures computed frames such as `buildAlarmReport(...)`, not only
//! the hex literals in the source — and the reference suite was run green (64/64) while
//! recording. So these are the reference decoder's own answers, not someone's reading of them.
//!
//! *Amended 2026-09-25 (#90):* the fPort-2 vectors were answered again by the v2.0.0 ttn.js
//! (sticker-firmware 177a3dd), which adds `alarm_status` and `alarm_status_flags` from
//! `system_flags`. No other field of any vector changed.
//!
//! *Amended 2026-10-09 (#119, v2.0.0 sensor channel model):* re-recorded from ttn.js at
//! sticker-firmware 435273c6 the same way (its suite 91/91 green while recording, 36 vectors),
//! plus the 15 earlier vectors that suite no longer exercises, answered by the 435273c6
//! decoder, plus one alarm batch built for `alarm_values_unscale_by_the_channel_wire_scale`
//! and answered by it too. Every expected output is the 435273c6 reference's.
//!
//! *Amended 2026-10-09 (sticker-firmware #480, telemetry paging):* re-answered by ttn.js at
//! eff819ed (suite 94/94 green while recording, 38 vectors), which adds `page_index`,
//! `page_count` and `pages` on fPort 2 and names a whole 1-Wire device's no-data channel
//! `device` (with the failed chip's `part` on a per-channel one), plus the three HIL captures
//! of #480 (EU868, machine probe on s1: DR0 pages 1/2 and 2/2, DR5 one frame). 57 vectors,
//! every expected output the eff819ed reference's.
//!
//! *Amended 2026-10-14 (v2.0.0 minimum, sticker-firmware 408eeaf):* every vector re-answered
//! by ttn.js at 408eeaf, none changed. The one vector of the retired flat probe fields 10..17
//! (`0150cc21686c`) is dropped: ttn.js still decodes them for a mixed fleet, the Hub does not
//! (they are `reserved` in the 2.0.0 proto). Two vectors of the analog GP input voltages
//! (telemetry 30/31) are added, answered by 408eeaf. 58 vectors.
//!
//! Only fPort 2 and 3 are vendored: fPort 1 is the legacy bitmap, dropped in v2 because the
//! minimum supported firmware is v2.0.0, and fPort 85 is the command engine's rather than the
//! field decoder's.

use serde_json::Value;

#[derive(serde::Deserialize)]
struct Vector {
    f_port: u8,
    hex: String,
    expected: Value,
}

const FIXTURE: &str = include_str!("fixtures/ttn-parity-uplink.json");

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("fixture hex is valid"))
        .collect()
}

#[test]
fn the_rust_decoder_matches_the_reference_field_for_field() {
    let vectors: Vec<Vector> = serde_json::from_str(FIXTURE).expect("fixture parses");
    assert!(!vectors.is_empty(), "the fixture must not be empty");

    let mut failures = Vec::new();
    for v in &vectors {
        let bytes = unhex(&v.hex);
        let got = control_radio::decode::decode_uplink("node", v.f_port, &bytes)
            .unwrap_or_else(|| panic!("fPort {} must be decoded here", v.f_port));

        let got_json = serde_json::to_value(&got).expect("serializes");

        // Compare the three arms separately so a failure says which one moved.
        for key in ["data", "warnings", "errors"] {
            let a = &got_json[key];
            let b = &v.expected[key];
            if a != b {
                failures.push(format!(
                    "  fPort {} {}\n    {key} ours: {a}\n    {key} ref : {b}",
                    v.f_port, v.hex
                ));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} vectors differ from ttn.js@408eeaf:\n{}",
        failures.len(),
        vectors.len(),
        failures.join("\n")
    );
}

/// Number *representation* is part of parity, not just value: the reference is JavaScript, so
/// a scaled field landing on a whole number serializes as `54`, never `54.0`. A Rust decoder
/// that emitted floats everywhere would compare equal numerically and still fail a
/// golden-file diff.
#[test]
fn integral_values_serialize_as_integers_like_the_reference() {
    // 0108641000900100980100 -> voltage 100/50 = 2
    let bytes = unhex("0108641000900100980100");
    let got = control_radio::decode::decode_uplink("node", 2, &bytes).expect("decodes");
    let json = serde_json::to_string(&got.data).expect("serializes");
    assert!(
        json.contains("\"voltage\":2"),
        "expected an integer 2 in {json}"
    );
    assert!(!json.contains("2.0"), "must not render as a float: {json}");
}

/// The distinction a struct of `Option`s would lose: an absent sensor omits its key, while an
/// enabled sensor with no valid sample emits an explicit `null`. That is how a dashboard tells
/// "not fitted" from "fitted and broken".
#[test]
fn a_sentinel_reading_is_null_while_an_absent_sensor_is_omitted() {
    let got = control_radio::decode::decode_uplink("node", 2, &unhex("0118ffffffff0f20ffffffff0f"))
        .expect("decodes");
    assert_eq!(
        got.data.get("temperature"),
        Some(&Value::Null),
        "sentinel -> null"
    );
    assert_eq!(got.data.get("humidity"), Some(&Value::Null));
    assert!(
        got.data.get("pressure").is_none(),
        "absent -> key omitted entirely"
    );
}

/// An unexpected version byte warns and still decodes — which is the reference's behaviour and
/// `node-firmware-v1.4.0.md` §1's, and is **not** what `plan.md` §7 step 1 says ("log + skip
/// the frame"). The pinned repo is the wire truth by that document's own rule, and a skipped
/// frame fails parity outright. Recorded in the plan-feedback set.
#[test]
fn an_unknown_version_byte_warns_and_still_decodes() {
    let got = control_radio::decode::decode_uplink("node", 2, &unhex("020864")).expect("decodes");
    assert_eq!(got.warnings, vec!["unknown payload version 0x2"]);
    assert_eq!(
        got.data.get("voltage"),
        Some(&Value::from(2)),
        "the frame is not skipped"
    );
}

/// v2.0.0 (#430): an alarm value unscales by its **channel's wire scale** from the registry,
/// the scale the same channel has in telemetry. v1.4.0's per-quantity table (humidity /100) is
/// gone: mb humidity raw 90 is 45 %RH (/2), s1 machine-probe magnetic-field raw -1500 is
/// -1.5 mT (/1000), the low-battery watchdog raw 2900 is 2.9 V (/1000) with no rule, and a
/// sensor mismatch carries the detected type id raw, named in `detected_type`.
#[test]
fn alarm_values_unscale_by_the_channel_wire_scale() {
    let got = control_radio::decode::decode_uplink(
        "node",
        3,
        &unhex(
            "010880f7c4d50610051a0b200128b4013802480250011a0f0801200228b71738044801500458031a\
             0d08022003280638ff01480558021a0c200438fe014801501428a82d2001",
        ),
    )
    .expect("decodes");
    let alarms = got.data["alarms"].as_array().expect("alarms array");
    assert_eq!(alarms[0]["slot"], "mb");
    assert_eq!(alarms[0]["channel"], "humidity");
    assert_eq!(alarms[0]["rule"], 2);
    assert_eq!(alarms[0]["value"], Value::from(45));
    assert_eq!(alarms[1]["channel"], "magnetic-field");
    assert_eq!(alarms[1]["sensor_type"], "machine-probe");
    assert_eq!(alarms[1]["value"], Value::from(-1.5));
    assert_eq!(alarms[2]["channel"], Value::Null);
    assert_eq!(alarms[2]["type"], "sensor_mismatch");
    assert_eq!(alarms[2]["rule"], Value::Null);
    assert_eq!(alarms[2]["value"], Value::from(3));
    assert_eq!(alarms[2]["detected_type"], "machine-probe");
    assert_eq!(alarms[3]["channel"], "battery-voltage");
    assert_eq!(alarms[3]["rule"], Value::Null);
    assert_eq!(alarms[3]["value"], Value::from(2.9));
}

/// `truncated` says some alarms were dropped to fit the data rate — `alarms.len() < total`.
#[test]
fn truncation_is_reported_when_fewer_events_arrive_than_the_total() {
    let got = control_radio::decode::decode_uplink(
        "node",
        3,
        &unhex("010880cae2d00610051a0b0802100128a84630014801"),
    )
    .expect("decodes");
    assert_eq!(got.data["total"], Value::from(5));
    assert_eq!(got.data["alarms"].as_array().expect("array").len(), 1);
    assert_eq!(got.data["truncated"], Value::Bool(true));
}

/// An unsynced clock means base_time is uptime-relative, so an absolute timestamp would be a
/// lie: `time` is null rather than a plausible-looking wrong number.
#[test]
fn an_unsynced_clock_yields_a_null_time_not_a_wrong_one() {
    let got = control_radio::decode::decode_uplink(
        "node",
        3,
        &unhex("01087810011a09200a28c829380748022000"),
    )
    .expect("decodes");
    assert_eq!(got.data["time_synced"], Value::Bool(false));
    assert_eq!(got.data["alarms"][0]["time"], Value::Null);
}
